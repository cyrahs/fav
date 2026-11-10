//! The part of APScheduler's `AsyncIOScheduler` the worker uses.
//!
//! Python origin: APScheduler 3.11 (`BaseScheduler._process_jobs`, the memory job store and
//! the asyncio executor) as `run.py` configures it: cron triggers only, `coalesce=True`,
//! `max_instances=1`, `misfire_grace_time=300`. Due-ness is decided on instants rather than
//! wall times, and the next fire time is always strictly after the last one (D3).
//!
//! The scheduler owns the tasks of the runs it started: [`Scheduler::run`] returns once the
//! stop flag is set and every run it started has finished, so a run cut short by the
//! shutdown still records its outcome (D16).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono_tz::Tz;
use fav_core::logging::{self, Logger};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use crate::cron::{CronTrigger, ZonedTime};
use crate::{BoxFuture, StopSignal};

/// What a scheduled job runs.
pub type JobFn = Arc<dyn Fn() -> BoxFuture<'static, ()> + Send + Sync>;

/// `misfire_grace_time`: a run more than this late is skipped.
pub const MISFIRE_GRACE_SECONDS: i64 = 300;
/// `max_instances`.
pub const MAX_INSTANCES: usize = 1;
/// APScheduler's `TIMEOUT_MAX`: the longest the loop sleeps without a due job.
const TIMEOUT_MAX_SECONDS: i64 = 4_294_967;

struct Entry {
    id: String,
    name: String,
    trigger: CronTrigger,
    func: JobFn,
    next_run: Option<ZonedTime>,
    running: Arc<AtomicUsize>,
}

impl Entry {
    /// `str(job)`: `name (trigger: cron[...], next run at: 2026-01-01 04:00:00 UTC)`.
    fn describe(&self) -> String {
        let next = self.next_run.as_ref().map_or_else(
            || "None".to_owned(),
            |next| format!("{} {}", next.naive().format("%Y-%m-%d %H:%M:%S"), next.tzname()),
        );
        format!("{} (trigger: {}, next run at: {next})", self.name, self.trigger)
    }
}

#[derive(Default)]
struct State {
    /// In the order they were added, like the memory job store's insertion order for ties.
    jobs: Vec<Entry>,
}

/// A cron scheduler for the worker's jobs.
#[derive(Clone)]
pub struct Scheduler {
    zone: Tz,
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
}

impl std::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("zone", &self.zone)
            .field("jobs", &self.job_ids())
            .finish_non_exhaustive()
    }
}

fn scheduler_log() -> Logger {
    logging::get_external("apscheduler.scheduler")
}

fn executor_log() -> Logger {
    logging::get_external("apscheduler.executors.default")
}

impl Scheduler {
    /// `AsyncIOScheduler(timezone=zone)`.
    pub fn new(zone: Tz) -> Self {
        Self {
            zone,
            state: Arc::default(),
            wake: Arc::new(Notify::new()),
        }
    }

    pub fn zone(&self) -> Tz {
        self.zone
    }

    fn state(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data behind; keep using it.
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `add_job(func, trigger=trigger, id=id, name=name, ...)`; replaces a job with the same id.
    pub fn add_job(&self, id: &str, name: &str, trigger: CronTrigger, func: JobFn) {
        let next_run = trigger.next_fire_time(None, &ZonedTime::now(self.zone));
        let entry = Entry {
            id: id.to_owned(),
            name: name.to_owned(),
            trigger,
            func,
            next_run,
            running: Arc::default(),
        };
        let mut state = self.state();
        state.jobs.retain(|job| job.id != id);
        // APScheduler drops a job whose trigger never fires again.
        if entry.next_run.is_some() {
            state.jobs.push(entry);
        }
        drop(state);
        self.wake.notify_one();
    }

    /// `reschedule_job(id, trigger=trigger)`: a new trigger, its next fire time from now.
    pub fn reschedule_job(&self, id: &str, trigger: CronTrigger) -> bool {
        let mut state = self.state();
        let Some(index) = state.jobs.iter().position(|job| job.id == id) else {
            return false;
        };
        let next_run = trigger.next_fire_time(None, &ZonedTime::now(self.zone));
        if next_run.is_none() {
            state.jobs.remove(index);
        } else {
            let entry = &mut state.jobs[index];
            entry.trigger = trigger;
            entry.next_run = next_run;
        }
        drop(state);
        self.wake.notify_one();
        true
    }

    /// `remove_job(id)`; whether there was one.
    pub fn remove_job(&self, id: &str) -> bool {
        let mut state = self.state();
        let before = state.jobs.len();
        state.jobs.retain(|job| job.id != id);
        before != state.jobs.len()
    }

    /// `str(get_job(id).trigger)`, if the job exists.
    pub fn trigger_string(&self, id: &str) -> Option<String> {
        self.state().jobs.iter().find(|job| job.id == id).map(|job| job.trigger.to_string())
    }

    /// When the job runs next, if it exists.
    pub fn next_run_time(&self, id: &str) -> Option<ZonedTime> {
        self.state().jobs.iter().find(|job| job.id == id).and_then(|job| job.next_run)
    }

    pub fn job_ids(&self) -> Vec<String> {
        self.state().jobs.iter().map(|job| job.id.clone()).collect()
    }

    /// `_process_jobs`: start what is due at `now`; returns how long until the next due job.
    fn process_due(&self, now: &ZonedTime, runs: &mut JoinSet<()>) -> Option<chrono::Duration> {
        let mut state = self.state();
        let mut due: Vec<usize> = (0..state.jobs.len())
            .filter(|index| state.jobs[*index].next_run.is_some_and(|next| next.to_utc() <= now.to_utc()))
            .collect();
        due.sort_by_key(|index| state.jobs[*index].next_run.map(|next| next.to_utc()));
        let mut finished: Vec<String> = Vec::new();
        for index in due {
            let entry = &mut state.jobs[index];
            // `_get_run_times(now)`, coalesced to the last one.
            let mut run_time = entry.next_run.expect("due jobs have a next run time");
            while let Some(next) = entry.trigger.next_fire_time_after(&run_time, now) {
                if next.to_utc() > now.to_utc() {
                    break;
                }
                run_time = next;
            }
            let full = entry.running.load(Ordering::SeqCst) >= MAX_INSTANCES;
            if full {
                scheduler_log().warning(format!(
                    "Execution of job \"{}\" skipped: maximum number of running instances reached ({MAX_INSTANCES})",
                    entry.describe()
                ));
            }
            entry.next_run = entry.trigger.next_fire_time_after(&run_time, now);
            if entry.next_run.is_none() {
                finished.push(entry.id.clone());
            }
            if full {
                continue;
            }
            // Same zone on both sides: Python subtracts the wall times.
            let late = now.naive() - run_time.naive();
            if late > chrono::Duration::seconds(MISFIRE_GRACE_SECONDS) {
                executor_log().warning(format!(
                    "Run time of job \"{}\" was missed by {}",
                    entry.describe(),
                    crate::pystr::timedelta(late)
                ));
                continue;
            }
            let running = Arc::clone(&entry.running);
            running.fetch_add(1, Ordering::SeqCst);
            let run = (entry.func)();
            runs.spawn(async move {
                run.await;
                running.fetch_sub(1, Ordering::SeqCst);
            });
        }
        state.jobs.retain(|job| !finished.contains(&job.id));
        state
            .jobs
            .iter()
            .filter_map(|job| job.next_run)
            .map(|next| next.to_utc() - now.to_utc())
            .min()
    }

    /// Run the scheduler until `stop` is set, then wait for the runs it started.
    pub async fn run(&self, stop: StopSignal) {
        let mut runs = JoinSet::new();
        loop {
            let now = ZonedTime::now(self.zone);
            let wait = self
                .process_due(&now, &mut runs)
                .unwrap_or(chrono::Duration::seconds(TIMEOUT_MAX_SECONDS))
                .clamp(chrono::Duration::zero(), chrono::Duration::seconds(TIMEOUT_MAX_SECONDS));
            let wait = wait.to_std().unwrap_or_default();
            tokio::select! {
                () = stop.wait() => break,
                () = self.wake.notified() => {}
                () = tokio::time::sleep(wait) => {}
                Some(_) = runs.join_next(), if !runs.is_empty() => {}
            }
        }
        while runs.join_next().await.is_some() {}
    }
}

#[cfg(test)]
mod tests;
