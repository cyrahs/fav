//! The worker: scheduler, control-request consumer, notification delivery, settings
//! watcher, and the one-shot `fav trigger`.
//!
//! Python origin: `run.py`. Everything the worker talks to is injected through
//! [`WorkerOptions`]; the process entry points (signals, logging, the database URL) live in
//! the `fav` binary. Each job's runner is built once at startup from the job as the
//! settings described it then, as Python's closures are (D15); which jobs are runnable is
//! re-read per control request and on every settings change.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, SubsecRound as _, Utc};
use chrono_tz::Tz;
use fav_core::logging::{self, Logger};
use fav_core::pyjson::{Dict, Value};
use fav_db::{Db, SettingsStore};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::control::{self, ControlRequest, KIND_TRIGGER_JOB, STATUS_FAILED, STATUS_INTERRUPTED, STATUS_REJECTED, STATUS_SUCCEEDED};
use crate::cron::CronTrigger;
use crate::jobs::{ScheduledJob, build_jobs, resolve_trigger_jobs};
use crate::notifications::{NewNotification, NotificationRecord, Outbox, Resolution, SCOPE_JOB};
use crate::notifications::{format_job_failure_dedupe_key, format_job_run_failure_dedupe_key};
use crate::pystr::repr_str;
use crate::scheduler::{JobFn, Scheduler};
use crate::source::{JobError, Registry, ResidentSource, Source, SourceKind};
use crate::telegram_bot::{BotClient, TelegramBotConfig, load_config};
use crate::which::which;
use crate::{BoxFuture, Error, StopHandle, StopSignal};

pub const CONTROL_REQUEST_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const SETTINGS_POLL_INTERVAL: Duration = Duration::from_secs(15);
pub const NOTIFICATION_DELIVERY_POLL_INTERVAL: Duration = Duration::from_secs(1);
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// The job key whose source stays resident in the worker.
const RESIDENT_JOB: &str = "telegram";

fn log() -> Logger {
    logging::get("main")
}

/// How one run of a job ended (`JobRunResult`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRunResult {
    pub job_key: String,
    pub job_name: String,
    pub success: bool,
    pub error: String,
    /// Cut short by a shutdown: says nothing about the job.
    pub cancelled: bool,
}

impl JobRunResult {
    pub fn succeeded(job: &ScheduledJob) -> Self {
        Self {
            job_key: job.key.clone(),
            job_name: job.name.clone(),
            success: true,
            error: String::new(),
            cancelled: false,
        }
    }

    pub fn failed(job: &ScheduledJob, error: impl Into<String>) -> Self {
        Self {
            success: false,
            error: error.into(),
            ..Self::succeeded(job)
        }
    }

    pub fn cancelled(job: &ScheduledJob) -> Self {
        Self {
            cancelled: true,
            ..Self::failed(job, "CancelledError")
        }
    }

    /// `_unsuccessful_status`: a cancelled run was cut short by a shutdown or restart.
    fn unsuccessful_status(&self) -> &'static str {
        if self.cancelled { STATUS_INTERRUPTED } else { STATUS_FAILED }
    }

    /// `_result_label`.
    fn label(&self) -> &'static str {
        if self.success {
            "ok"
        } else if self.cancelled {
            "interrupted"
        } else {
            "failed"
        }
    }
}

/// One way to run a job (`runner_by_key[key]`).
pub type RunnerFn = Arc<dyn Fn() -> BoxFuture<'static, JobRunResult> + Send + Sync>;

/// The runners by job key: only jobs whose source exists in Rust have one.
pub type Runners = HashMap<String, RunnerFn>;

/// The source a run works with.
#[allow(missing_debug_implementations)] // reason: holds trait objects of sources, which have no Debug
pub enum Instance {
    /// Created for this run, closed after it.
    PerRun(Box<dyn Source>),
    /// A resident source; `close` says whether this run owns it (`close_worker`).
    Shared { source: Arc<dyn ResidentSource>, close: bool },
}

/// What the worker works with.
#[derive(Clone)]
#[allow(missing_debug_implementations)] // reason: the registry holds factories, which have no Debug
pub struct WorkerOptions {
    pub db: Db,
    pub settings: Arc<SettingsStore>,
    pub registry: Registry,
    pub bot: BotClient,
    pub timezone: Tz,
}

/// Why the worker or a trigger stopped short.
#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    /// `SystemExit`: exit with status 1, printing the message if there is one.
    #[error("{}", .0.as_deref().unwrap_or(""))]
    Exit(Option<String>),
    /// `RuntimeError` raised out of `main`.
    #[error("{0}")]
    Failed(String),
    #[error(transparent)]
    Store(#[from] Error),
}

/// `_serialize_datetime`: UTC, whole seconds, `Z`.
fn serialize_datetime(value: DateTime<Utc>) -> String {
    value.trunc_subsecs(0).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `round(value, 3)` as a JSON float: the exact binary value rounded half to even, which
/// formatting does and `(value * 1000.0).round()` does not (76.3775 is 76.37749... in binary).
fn round3(value: f64) -> Value {
    Value::Float(format!("{value:.3}").parse().unwrap_or(value))
}

/// `_disabled_job_reason`.
fn disabled_job_reason(job: &ScheduledJob) -> String {
    if job.missing_fields.is_empty() {
        format!("Job is disabled: {}", job.key)
    } else {
        format!("Job is not configured: {} (missing {})", job.key, job.missing_fields.join(", "))
    }
}

/// `_missing_commands`.
fn missing_commands(job: &ScheduledJob) -> Vec<String> {
    job.required_commands
        .iter()
        .filter(|command| which(command).is_none())
        .cloned()
        .collect()
}

/// `_validate_commands(jobs, respect_enabled=False)`: log every missing command and exit 1.
fn validate_commands(jobs: &[ScheduledJob]) -> Result<(), WorkerError> {
    let mut missing = false;
    for job in jobs {
        for command in missing_commands(job) {
            log().error(format!("{} requires command {} in PATH", job.name, repr_str(&command)));
            missing = true;
        }
    }
    if missing { Err(WorkerError::Exit(None)) } else { Ok(()) }
}

/// `_resolve_scheduler_timezone`: `TZ`, stripped, or UTC.
pub fn resolve_scheduler_timezone(tz: Option<&str>) -> Tz {
    let name = tz.map(fav_core::pytext::py_strip).filter(|name| !name.is_empty()).unwrap_or("UTC");
    name.parse().unwrap_or_else(|_| {
        log().warning(format!("Invalid TZ {}, fallback to UTC", repr_str(name)));
        Tz::UTC
    })
}

/// The worker's shared state: the stores, the bot client and the stop flag.
#[derive(Clone, Debug)]
pub struct Worker {
    db: Db,
    settings: Arc<SettingsStore>,
    outbox: Outbox,
    bot: BotClient,
    stop: StopSignal,
}

impl Worker {
    pub fn new(db: Db, settings: Arc<SettingsStore>, bot: BotClient, stop: StopSignal) -> Self {
        Self {
            outbox: Outbox::new(db.clone(), Arc::clone(&settings)),
            db,
            settings,
            bot,
            stop,
        }
    }

    // ---------- one run ----------

    /// `_enqueue_job_failed_notification`. The outbox gates on the notification's own
    /// source, and this one is the worker rather than the job that failed, so the job's
    /// toggle is applied here.
    async fn enqueue_job_failed_notification(
        &self,
        job: &ScheduledJob,
        started_at: DateTime<Utc>,
        finished_at: DateTime<Utc>,
        elapsed_seconds: f64,
        error: &JobError,
    ) {
        if !job.notify {
            log().debug(format!("Notifications are off for {}; not reporting its failure", job.key));
            return;
        }
        let dedupe_key = match error
            .notification_dedupe_key()
            .filter(|key| !fav_core::pytext::py_strip(key).is_empty())
        {
            Some(failure_key) => format_job_failure_dedupe_key(&job.key, failure_key),
            // Anything else (an expired cookie, a network error) is "this job's run is
            // broken", one row per job, however many runs it stays broken for.
            None => format_job_run_failure_dedupe_key(&job.key),
        };
        let mut payload = Dict::new();
        payload.insert("job", Value::from(job.key.as_str()));
        payload.insert("dedupe_key", Value::from(dedupe_key.as_str()));
        payload.insert("started_at", Value::from(serialize_datetime(started_at)));
        payload.insert("finished_at", Value::from(serialize_datetime(finished_at)));
        payload.insert("elapsed_seconds", round3(elapsed_seconds));
        payload.insert("error_class", Value::from(error.class_name()));
        payload.insert("error_message", Value::from(error.message()));
        let notification = NewNotification {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            header: job.name.clone(),
            title: "Job failed".to_owned(),
            body: error.formatted(),
            payload: Some(payload),
            dedupe_key,
            scope: Some(SCOPE_JOB.to_owned()),
            ..NewNotification::default()
        };
        if let Err(notify_error) = self.outbox.enqueue(&notification).await {
            log().warning(format!("Failed to enqueue job_failed notification for {}: {notify_error}", job.key));
        }
    }

    /// `_resolve_job_failed_notifications`: clear whatever this job raised on earlier runs
    /// now that it went through. Runs on every success, so a restart between the two does
    /// not leave a pinned failure standing; not gated on `notify`.
    async fn resolve_job_failed_notifications(&self, job: &ScheduledJob, finished_at: DateTime<Utc>) {
        let mut payload = Dict::new();
        payload.insert("job", Value::from(job.key.as_str()));
        payload.insert("finished_at", Value::from(serialize_datetime(finished_at)));
        let resolution = Resolution::job_recovered(&job.name, &format!("{} completed successfully again.", job.name), Some(payload));
        match self.outbox.resolve_job_failures(&job.key, &resolution).await {
            Ok(resolved) => {
                for notification in resolved {
                    log().info(format!(
                        "Resolved job_failed notification {} ({})",
                        notification.notification_id, notification.dedupe_key
                    ));
                }
            }
            Err(error) => log().warning(format!("Failed to resolve job_failed notifications for {}: {error}", job.key)),
        }
    }

    /// `_run_job`: one run of `job`. A shutdown cuts the run short and reports it as
    /// cancelled; a failure is queued as a notification, a success resolves the old ones.
    pub async fn run_job(&self, job: &ScheduledJob, mut instance: Instance) -> JobRunResult {
        let started_at = Utc::now();
        let started = Instant::now();
        log().notice(format!("Job started: {}", job.name));
        let update = async {
            match &mut instance {
                Instance::PerRun(source) => source.update().await,
                Instance::Shared { source, .. } => source.update().await,
            }
        };
        let outcome = tokio::select! {
            biased;
            outcome = update => Some(outcome),
            () = self.stop.wait() => None,
        };
        let elapsed = started.elapsed().as_secs_f64();
        let result = match outcome {
            // A cancelled run is the worker going down, not the job being broken: the next
            // scheduled run simply runs, and nothing is reported.
            None => {
                log().notice(format!("Job cancelled: {} ({elapsed:.1}s)", job.name));
                JobRunResult::cancelled(job)
            }
            Some(Err(error)) => {
                self.enqueue_job_failed_notification(job, started_at, Utc::now(), elapsed, &error)
                    .await;
                log().error(format!("Job failed: {}\n{}", job.name, error.formatted()));
                JobRunResult::failed(job, error.formatted())
            }
            Some(Ok(())) => {
                log().notice(format!("Job completed: {} ({elapsed:.1}s)", job.name));
                self.resolve_job_failed_notifications(job, Utc::now()).await;
                JobRunResult::succeeded(job)
            }
        };
        let closed = match &mut instance {
            Instance::PerRun(source) => source.close().await,
            Instance::Shared { source, close: true } => source.close().await,
            Instance::Shared { close: false, .. } => Ok(()),
        };
        if let Err(error) = closed {
            log().warning(format!("Failed to close {} resources: {error}", job.name));
        }
        result
    }

    /// The runner of one job (the `runner` closure of `main`): one run at a time, the rest
    /// skipped while it lasts.
    pub fn runner(&self, job: ScheduledJob, kind: SourceKind, resident: Option<Arc<dyn ResidentSource>>) -> RunnerFn {
        let worker = self.clone();
        let lock = Arc::new(Mutex::new(()));
        let job = Arc::new(job);
        Arc::new(move || {
            let (worker, lock, job, kind, resident) = (worker.clone(), Arc::clone(&lock), Arc::clone(&job), kind.clone(), resident.clone());
            Box::pin(async move {
                let Ok(_guard) = lock.try_lock_owned() else {
                    log().warning(format!("{} is still running, skip this trigger", job.name));
                    return JobRunResult::failed(&job, "Job is already running");
                };
                let instance = match (resident, &kind) {
                    (Some(source), _) => Instance::Shared { source, close: false },
                    (None, SourceKind::PerRun(factory)) => Instance::PerRun(factory()),
                    (None, SourceKind::Resident(factory)) => Instance::Shared {
                        source: factory(),
                        close: true,
                    },
                };
                worker.run_job(&job, instance).await
            })
        })
    }

    // ---------- control requests ----------

    /// `_current_jobs`: job state as of right now, not as of worker startup, so "configure,
    /// then press Run" works.
    pub async fn current_jobs(&self) -> Result<Vec<ScheduledJob>, Error> {
        let settings = self.settings.load_force().await?;
        Ok(build_jobs(&settings))
    }

    /// `_execute_control_request`: run what a request asks for and record how it went.
    pub async fn execute_control_request(
        &self,
        request: &ControlRequest,
        all_jobs: &[ScheduledJob],
        runners: &Runners,
    ) -> Result<(), Error> {
        let db = &self.db;
        let id = request.request_id;
        if request.kind != KIND_TRIGGER_JOB {
            let error = format!("Unsupported control request kind: {}", request.kind);
            return control::update(db, id, STATUS_REJECTED, "", &error).await;
        }
        let target = fav_core::pytext::py_strip(&request.target).to_lowercase();
        if target == "all" {
            let enabled: Vec<&ScheduledJob> = all_jobs.iter().filter(|job| job.enabled).collect();
            if enabled.is_empty() {
                return control::update(db, id, STATUS_REJECTED, "", "No enabled jobs available for target all").await;
            }
            let mut results = Vec::new();
            for job in enabled {
                let Some(runner) = runners.get(&job.key) else {
                    results.push(JobRunResult::failed(job, "Runner unavailable"));
                    continue;
                };
                let result = runner().await;
                let cancelled = result.cancelled;
                results.push(result);
                if cancelled {
                    break;
                }
            }
            let summary = results
                .iter()
                .map(|result| format!("{}={}", result.job_key, result.label()))
                .collect::<Vec<_>>()
                .join(", ");
            let failures: Vec<&JobRunResult> = results.iter().filter(|result| !result.success).collect();
            if failures.is_empty() {
                return control::update(db, id, STATUS_SUCCEEDED, &summary, "").await;
            }
            let error = failures
                .iter()
                .map(|result| format!("{}: {}", result.job_key, result.error))
                .collect::<Vec<_>>()
                .join("; ");
            // A real failure outranks the cancellation that cut the batch short.
            let status = if failures.iter().any(|result| !result.cancelled) {
                STATUS_FAILED
            } else {
                STATUS_INTERRUPTED
            };
            return control::update(db, id, status, &summary, &error).await;
        }
        let Some(job) = all_jobs.iter().find(|job| job.key == target) else {
            return control::update(db, id, STATUS_REJECTED, "", &format!("Unknown job target: {target}")).await;
        };
        if !job.enabled {
            return control::update(db, id, STATUS_REJECTED, "", &disabled_job_reason(job)).await;
        }
        let Some(runner) = runners.get(&target) else {
            return control::update(db, id, STATUS_REJECTED, "", &format!("Runner unavailable for job: {target}")).await;
        };
        let result = runner().await;
        if result.success {
            control::update(db, id, STATUS_SUCCEEDED, &format!("Completed {target}."), "").await
        } else {
            control::update(db, id, result.unsuccessful_status(), "", &result.error).await
        }
    }

    /// `_consume_control_requests`, with the job state from `jobs_provider` per request.
    ///
    /// Unlike Python, an error (the database away, the settings unreadable) does not end the
    /// consumer: it is logged, a request it interrupted is marked failed, and the loop goes
    /// on after the poll interval (D14).
    pub async fn consume_control_requests<P>(&self, runners: &Runners, jobs_provider: P)
    where
        P: Fn() -> BoxFuture<'static, Result<Vec<ScheduledJob>, Error>>,
    {
        while !self.stop.is_set() {
            let mut claimed: Option<i64> = None;
            let step = async {
                let Some(request) = control::claim_next(&self.db).await? else {
                    return Ok(false);
                };
                claimed = Some(request.request_id);
                let jobs = jobs_provider().await?;
                self.execute_control_request(&request, &jobs, runners).await?;
                Ok::<bool, Error>(true)
            }
            .await;
            match step {
                Ok(true) => continue,
                Ok(false) => {}
                Err(error) => {
                    log().warning(format!("Control request loop failed: {error}"));
                    if let Some(id) = claimed
                        && let Err(update_error) = control::update(&self.db, id, STATUS_FAILED, "", &error.to_string()).await
                    {
                        log().warning(format!("Failed to record control request {id}: {update_error}"));
                    }
                }
            }
            if self.stop.wait_timeout(CONTROL_REQUEST_POLL_INTERVAL).await {
                break;
            }
        }
    }

    // ---------- the scheduler ----------

    /// `_make_scheduled_runner`: a cron fire leaves a `control_requests` row. Bookkeeping
    /// failures never stop the job.
    pub fn scheduled_runner(&self, job: &ScheduledJob, runner: RunnerFn) -> JobFn {
        let db = self.db.clone();
        let key = job.key.clone();
        Arc::new(move || {
            let (db, key, runner) = (db.clone(), key.clone(), Arc::clone(&runner));
            Box::pin(async move {
                let request_id = match control::record_scheduled_run_start(&db, &key).await {
                    Ok(id) => Some(id),
                    Err(error) => {
                        log().warning(format!("Failed to record scheduled run for {key}: {error}"));
                        None
                    }
                };
                let result = runner().await;
                let Some(request_id) = request_id else { return };
                let recorded = if result.success {
                    control::update(&db, request_id, STATUS_SUCCEEDED, &format!("Completed {key}."), "").await
                } else {
                    control::update(&db, request_id, result.unsuccessful_status(), "", &result.error).await
                };
                if let Err(error) = recorded {
                    log().warning(format!("Failed to record scheduled run result for {key}: {error}"));
                }
            })
        })
    }

    /// `_sync_scheduled_jobs`: bring the scheduler in line with `jobs`; the number of changes.
    pub fn sync_scheduled_jobs(&self, scheduler: &Scheduler, jobs: &[ScheduledJob], runners: &Runners) -> usize {
        let mut changes = 0;
        for job in jobs {
            let existing = scheduler.trigger_string(&job.key);
            let runner = runners.get(&job.key);
            let Some(runner) = runner.filter(|_| job.enabled) else {
                if existing.is_some() {
                    scheduler.remove_job(&job.key);
                    log().info(format!("Unscheduled {}", job.name));
                    changes += 1;
                }
                continue;
            };
            let missing = missing_commands(job);
            if !missing.is_empty() {
                // Enabled from the UI on a host that cannot run it; keep it parked rather
                // than taking the whole worker down.
                log().error(format!(
                    "{} requires command(s) {} in PATH; not scheduling",
                    job.name,
                    missing.join(", ")
                ));
                if existing.is_some() {
                    scheduler.remove_job(&job.key);
                    changes += 1;
                }
                continue;
            }
            let trigger = match CronTrigger::from_crontab(&job.cron, scheduler.zone()) {
                Ok(trigger) => trigger,
                Err(error) => {
                    log().error(format!(
                        "Invalid cron for {} ({}); not scheduling\nValueError: {error}",
                        job.name, job.cron
                    ));
                    if existing.is_some() {
                        scheduler.remove_job(&job.key);
                        changes += 1;
                    }
                    continue;
                }
            };
            match existing {
                None => {
                    scheduler.add_job(&job.key, &job.name, trigger, self.scheduled_runner(job, Arc::clone(runner)));
                    log().info(format!("Scheduled {} with cron {}", job.name, repr_str(&job.cron)));
                    changes += 1;
                }
                Some(current) if current != trigger.to_string() => {
                    scheduler.reschedule_job(&job.key, trigger);
                    log().info(format!("Rescheduled {} with cron {}", job.name, repr_str(&job.cron)));
                    changes += 1;
                }
                Some(_) => {}
            }
        }
        changes
    }

    /// `_watch_settings`: reschedule jobs when the web UI edits their enabled/cron settings.
    /// Independent of the control-request loop, which runs jobs serially.
    pub async fn watch_settings(&self, scheduler: &Scheduler, runners: &Runners) {
        let mut last_version: Option<String> = None;
        while !self.stop.is_set() {
            let step = async {
                let version = self.settings.settings_version().await?;
                if last_version.as_ref() != Some(&version) {
                    if last_version.is_some() {
                        let settings = self.settings.load_force().await?;
                        let changes = self.sync_scheduled_jobs(scheduler, &build_jobs(&settings), runners);
                        log().info(format!("Applied settings change ({changes} schedule update(s))"));
                    }
                    last_version = Some(version);
                }
                Ok::<(), Error>(())
            }
            .await;
            if let Err(error) = step {
                log().warning(format!("Settings watcher failed: {error}"));
            }
            if self.stop.wait_timeout(SETTINGS_POLL_INTERVAL).await {
                break;
            }
        }
    }

    // ---------- notifications ----------

    /// `_load_telegram_bot_config`: per attempt, so UI changes apply without a restart.
    async fn telegram_config(&self) -> Result<Option<TelegramBotConfig>, Error> {
        Ok(load_config(&*self.settings.load().await?, true))
    }

    /// `_deliver_notification_to_telegram`: one claimed row out, its outcome recorded.
    pub async fn deliver_notification(&self, notification: &NotificationRecord, config: &TelegramBotConfig) -> Result<(), Error> {
        let attempt_count = notification.attempt_count + 1;
        let id = notification.notification_id;
        match self.bot.deliver(notification, config).await {
            Ok(result) => {
                self.outbox
                    .mark_delivered(id, notification.event_version, result.message_id, result.pinned_message_id)
                    .await?;
                let message = result.message_id.map_or_else(|| "None".to_owned(), |message| message.to_string());
                log().info(format!("Delivered notification {id} through Telegram (message {message})"));
            }
            Err(error) if error.retryable => {
                self.outbox
                    .mark_retry(
                        id,
                        notification.event_version,
                        attempt_count,
                        &error.message,
                        error.retry_after_seconds,
                    )
                    .await?;
                log().warning(format!("Telegram delivery retry scheduled for notification {id}: {error}"));
            }
            Err(error) => {
                self.outbox
                    .mark_failed(id, notification.event_version, attempt_count, &error.message)
                    .await?;
                log().warning(format!("Telegram delivery permanently failed for notification {id}: {error}"));
            }
        }
        Ok(())
    }

    /// `_deliver_next_notification`: whether a row was handled. Nothing is claimed while
    /// Telegram delivery is off or unconfigured.
    pub async fn deliver_next_notification(&self) -> Result<bool, Error> {
        let Some(config) = self.telegram_config().await? else {
            return Ok(false);
        };
        let Some(notification) = self.outbox.claim_next_pending().await? else {
            return Ok(false);
        };
        self.deliver_notification(&notification, &config).await?;
        Ok(true)
    }

    /// `_drain_pending_notifications`: deliver until nothing is due; how many went out.
    pub async fn drain_pending_notifications(&self) -> Result<usize, Error> {
        let mut delivered = 0;
        while self.deliver_next_notification().await? {
            delivered += 1;
        }
        Ok(delivered)
    }

    /// `_consume_notification_deliveries`.
    pub async fn consume_notification_deliveries(&self) {
        let mut warned_unconfigured = false;
        while !self.stop.is_set() {
            let processed = self.deliver_next_notification().await.unwrap_or_else(|error| {
                log().warning(format!("Notification delivery loop failed: {error}"));
                false
            });
            if processed {
                warned_unconfigured = false;
                continue;
            }
            if !warned_unconfigured && matches!(self.telegram_config().await, Ok(None)) {
                log().warning("Telegram notifications are not configured; notifications stay queued until bot_token and chat_id are set");
                warned_unconfigured = true;
            }
            if self.stop.wait_timeout(NOTIFICATION_DELIVERY_POLL_INTERVAL).await {
                break;
            }
        }
    }
}

/// `_shutdown_task`: wait for a task that saw the stop flag, at most [`SHUTDOWN_TIMEOUT`].
async fn shutdown_task(task: Option<JoinHandle<()>>, name: &str) {
    let Some(mut task) = task else { return };
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task).await {
        Err(_) => {
            log().warning(format!("Timed out while waiting {name} to stop"));
            task.abort();
        }
        Ok(Err(error)) if !error.is_cancelled() => log().warning(format!("{name} stop failed: {error}")),
        Ok(_) => {}
    }
}

/// The jobs and their runners as the worker starts.
struct Startup {
    worker: Worker,
    all_jobs: Vec<ScheduledJob>,
    runners: Runners,
    resident: Option<Arc<dyn ResidentSource>>,
}

async fn startup(options: &WorkerOptions, stop: &StopSignal, keep_resident: bool) -> Result<Startup, Error> {
    let worker = Worker::new(options.db.clone(), Arc::clone(&options.settings), options.bot.clone(), stop.clone());
    let all_jobs = build_jobs(&*options.settings.load().await?);
    let resident = if keep_resident
        && all_jobs.iter().any(|job| job.key == RESIDENT_JOB && job.enabled)
        && let Some(SourceKind::Resident(factory)) = options.registry.get(RESIDENT_JOB)
    {
        Some(factory())
    } else {
        None
    };
    let mut runners = Runners::new();
    for job in &all_jobs {
        if let Some(kind) = options.registry.get(&job.key) {
            let shared = if job.key == RESIDENT_JOB { resident.clone() } else { None };
            runners.insert(job.key.clone(), worker.runner(job.clone(), kind.clone(), shared));
        }
    }
    Ok(Startup {
        worker,
        all_jobs,
        runners,
        resident,
    })
}

/// `main(trigger_target=target)`: run the target's jobs once, deliver what they queued.
///
/// A job without a Rust source yet cannot be triggered: that ends with an error rather than
/// running Python's source (D17).
pub async fn run_trigger(options: WorkerOptions, target: &str, stop: StopSignal) -> Result<(), WorkerError> {
    let result = async {
        let Startup {
            worker, all_jobs, runners, ..
        } = startup(&options, &stop, false).await?;
        let trigger_jobs = resolve_trigger_jobs(target, &all_jobs).map_err(|message| WorkerError::Exit(Some(message)))?;
        validate_commands(&trigger_jobs)?;
        // Every runner before any job runs, so `all` does not stop half-way (D17).
        let mut triggered = Vec::with_capacity(trigger_jobs.len());
        for job in &trigger_jobs {
            let runner = runners
                .get(&job.key)
                .ok_or_else(|| WorkerError::Exit(Some(format!("Runner unavailable for job: {}", job.key))))?;
            triggered.push((job, runner));
        }
        for (job, runner) in triggered {
            if !job.enabled {
                log().warning(format!("{} is disabled in config; running once due to --trigger", job.name));
            }
            runner().await;
            if stop.is_set() {
                return Ok(());
            }
        }
        worker.drain_pending_notifications().await?;
        Ok(())
    }
    .await;
    options.db.close().await;
    result
}

/// `main()`: the long-running worker, until `stop` is set (or the resident source fails).
pub async fn run_worker(options: WorkerOptions, stop: StopHandle) -> Result<(), WorkerError> {
    let signal = stop.signal();
    log().info(format!("Scheduler timezone: {}", options.timezone.name()));
    let scheduler = Scheduler::new(options.timezone);
    let mut tasks: Vec<(&str, Option<JoinHandle<()>>)> = Vec::new();
    let mut resident_task: Option<JoinHandle<Result<(), JobError>>> = None;
    let mut resident: Option<Arc<dyn ResidentSource>> = None;

    let result = async {
        let Startup {
            worker,
            all_jobs,
            runners,
            resident: started_resident,
        } = startup(&options, &signal, true).await?;
        resident = started_resident;
        let runners = Arc::new(runners);
        for job in all_jobs.iter().filter(|job| !job.enabled) {
            log().info(format!("{} is disabled; skipping schedule", job.name));
        }
        worker.sync_scheduled_jobs(&scheduler, &all_jobs, &runners);
        if !all_jobs.iter().any(|job| job.enabled) {
            log().warning("No jobs enabled. Waiting indefinitely.");
        }

        if let Some(source) = resident.clone() {
            let running = Arc::clone(&source);
            let run_stop = signal.clone();
            let mut run = tokio::spawn(async move { running.run(run_stop).await });
            tokio::select! {
                outcome = &mut run => {
                    if let Ok(Err(error)) = outcome {
                        log().error(format!("Telegram runtime failed: {error}"));
                    }
                    return Err(WorkerError::Failed("Telegram runtime exited before all accounts became ready".to_owned()));
                }
                ready = source.wait_until_ready() => {
                    resident_task = Some(run);
                    ready.map_err(|error| WorkerError::Failed(error.formatted()))?;
                }
                // A signal cancels Python's `main` wherever it waits: a clean shutdown.
                () = signal.wait() => {
                    resident_task = Some(run);
                    return Ok(());
                }
            }
            log().info("Telegram event listeners and queue workers enabled");
        }

        // Before the scheduler starts and before anything is claimed, so every `running`
        // row this sees belongs to a worker that has already stopped.
        match control::interrupt_running(&options.db).await {
            Ok(0) => {}
            Ok(count) => log().warning(format!("Marked {count} run(s) left by the previous worker as interrupted")),
            Err(error) => log().warning(format!("Failed to close out runs left by the previous worker: {error}")),
        }

        tasks.extend(spawn_consumers(&worker, &runners, &scheduler));

        let scheduler_task = {
            let (scheduler, signal) = (scheduler.clone(), signal.clone());
            tokio::spawn(async move { scheduler.run(signal).await })
        };
        tasks.push(("scheduler", Some(scheduler_task)));

        match resident_task.as_mut() {
            None => signal.wait().await,
            Some(run) => {
                tokio::select! {
                    () = signal.wait() => {}
                    outcome = run => {
                        resident_task = None;
                        if !signal.is_set() {
                            if let Ok(Err(error)) = outcome {
                                log().error(format!("Telegram runtime failed: {error}"));
                            }
                            return Err(WorkerError::Failed("Telegram runtime exited unexpectedly".to_owned()));
                        }
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    shut_down(&stop, tasks, resident.as_deref(), resident_task, &options.db).await;
    result
}

/// The control request consumer, the notification delivery consumer and the settings watcher.
fn spawn_consumers(worker: &Worker, runners: &Arc<Runners>, scheduler: &Scheduler) -> Vec<(&'static str, Option<JoinHandle<()>>)> {
    let consumer = {
        let (worker, runners) = (worker.clone(), Arc::clone(runners));
        tokio::spawn(async move {
            let provider_worker = worker.clone();
            worker
                .consume_control_requests(&runners, move || {
                    let worker = provider_worker.clone();
                    Box::pin(async move { worker.current_jobs().await })
                })
                .await;
        })
    };
    let delivery = {
        let worker = worker.clone();
        tokio::spawn(async move { worker.consume_notification_deliveries().await })
    };
    let watcher = {
        let (worker, runners, scheduler) = (worker.clone(), Arc::clone(runners), scheduler.clone());
        tokio::spawn(async move { worker.watch_settings(&scheduler, &runners).await })
    };
    log().info("Control request consumer enabled");
    log().info("Notification delivery consumer enabled");
    log().info(format!(
        "Settings watcher enabled (poll every {}s)",
        SETTINGS_POLL_INTERVAL.as_secs()
    ));
    vec![
        ("control request consumer", Some(consumer)),
        ("notification delivery consumer", Some(delivery)),
        ("settings watcher", Some(watcher)),
    ]
}

/// The shutdown half of [`run_worker`]: everything watches the same flag.
async fn shut_down(
    stop: &StopHandle,
    mut tasks: Vec<(&str, Option<JoinHandle<()>>)>,
    resident: Option<&dyn ResidentSource>,
    resident_task: Option<JoinHandle<Result<(), JobError>>>,
    db: &Db,
) {
    stop.stop();
    for (name, task) in tasks.drain(..3.min(tasks.len())) {
        shutdown_task(task, name).await;
    }
    if let Some(source) = resident
        && let Err(error) = source.close().await
    {
        log().warning(format!("Failed to close Telegram resources: {error}"));
    }
    if let Some(run) = resident_task {
        let run = tokio::spawn(async move {
            if let Ok(Err(error)) = run.await {
                log().warning(format!("telegram runtime stop failed: {error}"));
            }
        });
        shutdown_task(Some(run), "telegram runtime").await;
    }
    // The scheduler last: it waits for its runs, which record how they ended (D16).
    for (name, task) in tasks {
        shutdown_task(task, name).await;
    }
    db.close().await;
}

/// Install the SIGINT/SIGTERM handlers (`_install_signal_handlers`): the first signal sets
/// the stop flag, a repeat is only logged.
pub fn install_signal_handlers(stop: StopHandle) -> std::io::Result<JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(tokio::spawn(async move {
        loop {
            let name = tokio::select! {
                Some(()) = interrupt.recv() => "SIGINT",
                Some(()) = terminate.recv() => "SIGTERM",
                else => return,
            };
            if stop.stop() {
                log().info(format!("Received {name} again, shutdown already in progress"));
            } else {
                log().info(format!("Received {name}, starting graceful shutdown"));
            }
        }
    }))
}

#[cfg(test)]
mod tests;
