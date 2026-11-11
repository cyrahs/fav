use std::sync::atomic::AtomicUsize;

use chrono::{Duration, SubsecRound as _, Utc};
use fav_core::logging::{self, Level};

use super::*;
use crate::stop_signal;

fn counting_job(counter: &Arc<AtomicUsize>) -> JobFn {
    let counter = Arc::clone(counter);
    Arc::new(move || {
        let counter = Arc::clone(&counter);
        Box::pin(async move {
            counter.fetch_add(1, Ordering::SeqCst);
        })
    })
}

fn trigger(expr: &str) -> CronTrigger {
    CronTrigger::from_crontab(expr, Tz::UTC).expect("valid cron")
}

/// Move a job's next run time, as if the scheduler had been asleep.
fn set_next_run(scheduler: &Scheduler, id: &str, next: ZonedTime) {
    let mut state = scheduler.state();
    state.jobs.iter_mut().find(|job| job.id == id).expect("job").next_run = Some(next);
}

#[test]
fn jobs_are_added_rescheduled_and_removed() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    scheduler.add_job("bilibili", "Bilibili", trigger("*/30 * * * *"), counting_job(&counter));
    assert_eq!(scheduler.job_ids(), ["bilibili"]);
    assert_eq!(
        scheduler.trigger_string("bilibili").as_deref(),
        Some("cron[month='*', day='*', day_of_week='*', hour='*', minute='*/30']")
    );
    let next = scheduler.next_run_time("bilibili").expect("scheduled");
    assert!(next.to_utc() > Utc::now() && next.to_utc() <= Utc::now() + Duration::minutes(30));

    assert!(scheduler.reschedule_job("bilibili", trigger("0 4 * * *")));
    assert_eq!(
        scheduler.trigger_string("bilibili").as_deref(),
        Some("cron[month='*', day='*', day_of_week='*', hour='4', minute='0']")
    );
    assert_eq!(
        scheduler
            .next_run_time("bilibili")
            .expect("scheduled")
            .naive()
            .format("%H:%M")
            .to_string(),
        "04:00"
    );
    assert!(!scheduler.reschedule_job("missing", trigger("0 4 * * *")));
    assert!(scheduler.remove_job("bilibili"));
    assert!(!scheduler.remove_job("bilibili"));
    assert_eq!(scheduler.trigger_string("bilibili"), None);
}

#[tokio::test]
async fn missed_runs_coalesce_into_one() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    scheduler.add_job("x", "X", trigger("* * * * *"), counting_job(&counter));
    let now = ZonedTime::now(Tz::UTC);
    // Three fire times passed, the last one within the grace time.
    set_next_run(&scheduler, "x", ZonedTime::from_utc(now.to_utc() - Duration::seconds(150), Tz::UTC));
    let mut runs = JoinSet::new();
    let wait = scheduler.process_due(&now, &mut runs).expect("another run is due later");
    while runs.join_next().await.is_some() {}
    assert_eq!(counter.load(Ordering::SeqCst), 1);
    assert!(wait > Duration::zero() && wait <= Duration::seconds(60), "{wait}");
    assert!(scheduler.next_run_time("x").expect("next").to_utc() > now.to_utc());
}

#[test]
fn a_run_missed_by_more_than_the_grace_time_is_skipped() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    scheduler.add_job("x", "Yearly", trigger("0 0 1 1 *"), counting_job(&counter));
    let now = ZonedTime::now(Tz::UTC);
    let missed = ZonedTime::from_utc((now.to_utc() - Duration::seconds(600)).trunc_subsecs(0), Tz::UTC);
    set_next_run(&scheduler, "x", missed);
    let records = logging::capture(|| {
        let mut runs = JoinSet::new();
        scheduler.process_due(&now, &mut runs);
        assert!(runs.is_empty());
    });
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    let [record] = records.as_slice() else { panic!("{records:?}") };
    assert_eq!(record.name, "apscheduler.executors.default");
    assert_eq!(record.level, Level::Warning);
    let next = scheduler.next_run_time("x").expect("next year");
    let expected = format!(
        "Run time of job \"Yearly (trigger: cron[month='1', day='1', day_of_week='*', hour='0', minute='0'], next run at: {} UTC)\" was missed by {}",
        next.naive().format("%Y-%m-%d %H:%M:%S"),
        crate::pystr::timedelta(now.to_utc() - missed.to_utc()),
    );
    assert_eq!(record.message, expected);
}

#[test]
fn a_job_still_running_is_not_started_again() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    scheduler.add_job("x", "X", trigger("* * * * *"), counting_job(&counter));
    let now = ZonedTime::now(Tz::UTC);
    let due = ZonedTime::from_utc((now.to_utc() - Duration::seconds(5)).trunc_subsecs(0), Tz::UTC);
    set_next_run(&scheduler, "x", due);
    scheduler.state().jobs[0].running.store(1, Ordering::SeqCst);
    let records = logging::capture(|| {
        let mut runs = JoinSet::new();
        scheduler.process_due(&now, &mut runs);
        assert!(runs.is_empty());
    });
    let [record] = records.as_slice() else { panic!("{records:?}") };
    assert_eq!(record.name, "apscheduler.scheduler");
    assert_eq!(
        record.message,
        format!(
            "Execution of job \"X (trigger: cron[month='*', day='*', day_of_week='*', hour='*', minute='*'], next run at: {} UTC)\" skipped: maximum number of running instances reached (1)",
            due.naive().format("%Y-%m-%d %H:%M:%S")
        )
    );
    assert!(scheduler.next_run_time("x").expect("next").to_utc() > now.to_utc());
}

#[tokio::test]
async fn run_starts_due_jobs_and_waits_for_them_after_stop() {
    let scheduler = Scheduler::new(Tz::UTC);
    let (handle, signal) = stop_signal();
    let finished = Arc::new(AtomicUsize::new(0));
    let job: JobFn = {
        let (finished, signal) = (Arc::clone(&finished), signal.clone());
        Arc::new(move || {
            let (finished, signal) = (Arc::clone(&finished), signal.clone());
            Box::pin(async move {
                // A run that only ends once the worker stops, like a cancelled crawl.
                signal.wait().await;
                finished.fetch_add(1, Ordering::SeqCst);
            })
        })
    };
    scheduler.add_job("x", "X", trigger("* * * * *"), job);
    let running = tokio::spawn({
        let scheduler = scheduler.clone();
        async move { scheduler.run(signal).await }
    });
    // Once the loop has started (and set the first fire time), make the job due now.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    set_next_run(&scheduler, "x", ZonedTime::now(Tz::UTC));
    scheduler.wake.notify_one();
    // Let the loop start the run, then stop.
    for _ in 0..100 {
        if scheduler.state().jobs[0].running.load(Ordering::SeqCst) == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(scheduler.state().jobs[0].running.load(Ordering::SeqCst), 1);
    handle.stop();
    running.await.expect("scheduler ends");
    assert_eq!(finished.load(Ordering::SeqCst), 1, "the run finished before run() returned");
}

#[test]
fn a_trigger_that_never_fires_again_pauses_the_job() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    // February 30th never comes: APScheduler keeps the job with no next run time.
    scheduler.add_job("x", "X", trigger("0 0 30 2 *"), counting_job(&counter));
    assert_eq!(scheduler.job_ids(), ["x"]);
    assert_eq!(scheduler.next_run_time("x"), None);
    let mut runs = JoinSet::new();
    assert_eq!(scheduler.process_due(&ZonedTime::now(Tz::UTC), &mut runs), None);
    assert!(runs.is_empty());
    // Rescheduling onto a trigger that fires resumes it; one that never fires pauses it.
    assert!(scheduler.reschedule_job("x", trigger("0 4 * * *")));
    assert!(scheduler.next_run_time("x").is_some());
    assert!(scheduler.reschedule_job("x", trigger("0 0 31 4 *")));
    assert_eq!(scheduler.job_ids(), ["x"]);
    assert_eq!(scheduler.next_run_time("x"), None);
}

#[tokio::test]
async fn a_run_time_in_a_dst_gap_is_late_by_the_instant_difference() {
    let zone: Tz = "America/New_York".parse().expect("zone");
    let scheduler = Scheduler::new(zone);
    let counter = Arc::default();
    let trigger = CronTrigger::from_crontab("30 2 * * *", zone).expect("valid cron");
    // 2026-03-08 02:30 does not exist in New York; APScheduler keeps the -05:00 offset.
    let midnight = ZonedTime::from_utc("2026-03-08T05:00:00Z".parse().expect("instant"), zone);
    let gap = trigger.next_fire_time(None, &midnight).expect("fires");
    assert_eq!(
        gap.to_utc(),
        "2026-03-08T07:30:00Z".parse::<chrono::DateTime<Utc>>().expect("instant")
    );
    scheduler.add_job("x", "X", trigger, counting_job(&counter));
    set_next_run(&scheduler, "x", gap);
    // One second after the instant (03:30:01 EDT on the wall clock).
    let now = ZonedTime::from_utc("2026-03-08T07:30:01Z".parse().expect("instant"), zone);
    let records = logging::capture(|| {
        let mut runs = JoinSet::new();
        scheduler.process_due(&now, &mut runs);
        assert_eq!(runs.len(), 1, "started, not missed");
    });
    assert!(records.is_empty(), "{records:?}");
}

#[tokio::test]
async fn a_fire_time_that_passes_before_start_is_neither_run_nor_missed() {
    let scheduler = Scheduler::new(Tz::UTC);
    let counter = Arc::default();
    scheduler.add_job("x", "X", trigger("* * * * *"), counting_job(&counter));
    // Came due while the worker was still starting up (a long Telegram readiness wait).
    let earlier = ZonedTime::from_utc(Utc::now() - Duration::minutes(10), Tz::UTC);
    set_next_run(&scheduler, "x", earlier);
    let (handle, signal) = stop_signal();
    let records = logging::capture(|| {
        let running = scheduler.run(signal);
        handle.stop();
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut running = std::pin::pin!(running);
        assert!(running.as_mut().poll(&mut context).is_ready());
    });
    assert!(records.is_empty(), "{records:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert!(scheduler.next_run_time("x").expect("next").to_utc() > Utc::now() - Duration::seconds(1));
}
