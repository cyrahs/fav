use std::sync::Mutex as StdMutex;

use fav_core::settings::Settings;
use fav_db::settings_store::default_settings;
use fav_db::testdb::TestDatabase;
use serde_json::json;

use super::*;
use crate::control::STATUS_RUNNING;
use crate::notifications::{DELIVERY_DELIVERED, DELIVERY_PENDING, SELECT_FIELDS, WEBHOOK_ACTION_RESOLVE, WEBHOOK_ACTION_UPSERT};
use crate::stop_signal;
use crate::testutil::{Answer, FakeBotApi, database, execute};

// ---------- fixtures ----------

/// `_job()`: a job as `build_jobs` would describe it, named after its key.
fn job(key: &str, enabled: bool, missing_fields: &[&str]) -> ScheduledJob {
    let mut chars = key.chars();
    let name = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    ScheduledJob {
        key: key.to_owned(),
        name,
        cron: "*/30 * * * *".to_owned(),
        enabled,
        required_commands: Vec::new(),
        section: format!("web.{key}"),
        notify: true,
        missing_fields: missing_fields.iter().map(|field| (*field).to_owned()).collect(),
    }
}

fn worker_with(db: &Db, settings: Settings, bot: BotClient, stop: StopSignal) -> Worker {
    Worker::new(db.clone(), Arc::new(SettingsStore::pinned(settings)), bot, stop)
}

fn offline_bot() -> BotClient {
    BotClient::with_base_url("http://127.0.0.1:1").expect("client")
}

fn worker(db: &Db) -> Worker {
    worker_with(db, default_settings(), offline_bot(), stop_signal().1)
}

/// A runner that hands back `result` and counts its calls.
fn fixed(result: JobRunResult, calls: &Arc<StdMutex<Vec<String>>>) -> RunnerFn {
    let calls = Arc::clone(calls);
    Arc::new(move || {
        calls.lock().expect("calls").push(result.job_key.clone());
        let result = result.clone();
        Box::pin(async move { result })
    })
}

fn calls() -> Arc<StdMutex<Vec<String>>> {
    Arc::default()
}

fn recorded(calls: &Arc<StdMutex<Vec<String>>>) -> Vec<String> {
    calls.lock().expect("calls").clone()
}

/// A request claimed from the queue, as the consumer hands it over.
async fn claimed_request(db: &Db, target: &str) -> ControlRequest {
    control::create(db, KIND_TRIGGER_JOB, target, None).await.expect("created");
    control::claim_next(db).await.expect("claim").expect("pending")
}

/// `(status, result, error)` of a request.
async fn outcome(db: &Db, id: i64) -> (String, String, String) {
    let row = control::get(db, id).await.expect("query").expect("row");
    (row.status, row.result, row.error)
}

fn strings(status: &str, result: &str, error: &str) -> (String, String, String) {
    (status.to_owned(), result.to_owned(), error.to_owned())
}

/// What a fake source does when it runs.
#[derive(Clone)]
enum Outcome {
    Succeed,
    Fail(JobError),
    /// Never finishes on its own: only a shutdown ends it.
    Hang,
}

struct FakeSource {
    outcome: Outcome,
    calls: Arc<StdMutex<Vec<String>>>,
}

impl Source for FakeSource {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        self.calls.lock().expect("calls").push("update".to_owned());
        let outcome = self.outcome.clone();
        Box::pin(async move {
            match outcome {
                Outcome::Succeed => Ok(()),
                Outcome::Fail(error) => Err(error),
                Outcome::Hang => std::future::pending().await,
            }
        })
    }

    fn close(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        self.calls.lock().expect("calls").push("close".to_owned());
        Box::pin(async { Ok(()) })
    }
}

fn source(outcome: Outcome, calls: &Arc<StdMutex<Vec<String>>>) -> Instance {
    Instance::PerRun(Box::new(FakeSource {
        outcome,
        calls: Arc::clone(calls),
    }))
}

async fn notifications(db: &Db) -> Vec<NotificationRecord> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {SELECT_FIELDS} FROM notifications ORDER BY id;"
    )))
    .fetch_all(db.pool())
    .await
    .expect("rows");
    rows.iter().map(|row| NotificationRecord::from_row(row).expect("decodes")).collect()
}

fn payload_str(record: &NotificationRecord, key: &str) -> String {
    match record.payload_json().get(key) {
        Some(Value::Str(text)) => text.clone(),
        other => panic!("payload {key} is {other:?}"),
    }
}

// ---------- control requests ----------

// py: tests/test_run_control_requests.py::test_execute_control_request_rejects_disabled_target
#[tokio::test]
async fn execute_control_request_rejects_disabled_target() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "telegram").await;
    worker(db)
        .execute_control_request(&request, &[job("telegram", false, &[])], &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "Job is disabled: telegram")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_names_missing_fields
#[tokio::test]
async fn execute_control_request_names_missing_fields() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "azurlane").await;
    worker(db)
        .execute_control_request(&request, &[job("azurlane", false, &["origin_proxy"])], &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "Job is not configured: azurlane (missing origin_proxy)")
    );
    database.drop_database().await;
}

#[tokio::test]
async fn execute_control_request_rejects_unknown_kinds_targets_and_runners() {
    let database = database().await;
    let db = database.db();
    let worker = worker(db);
    let jobs = [job("bilibili", true, &[])];
    execute(
        &database,
        "INSERT INTO control_requests (kind, target, status) VALUES ('reboot', 'all', 'pending');",
    )
    .await;
    let request = control::claim_next(db).await.expect("claim").expect("pending");
    worker
        .execute_control_request(&request, &jobs, &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "Unsupported control request kind: reboot")
    );
    let request = claimed_request(db, " Nope ").await;
    worker
        .execute_control_request(&request, &jobs, &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "Unknown job target: nope")
    );
    // An enabled job whose source has no Rust port yet (D17).
    let request = claimed_request(db, "BILIBILI").await;
    worker
        .execute_control_request(&request, &jobs, &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "Runner unavailable for job: bilibili")
    );
    let request = claimed_request(db, "all").await;
    worker
        .execute_control_request(&request, &[job("bilibili", false, &[])], &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_REJECTED, "", "No enabled jobs available for target all")
    );
    let request = claimed_request(db, "all").await;
    worker
        .execute_control_request(&request, &jobs, &Runners::new())
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_FAILED, "bilibili=failed", "bilibili: Runner unavailable")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_consume_control_requests_rereads_job_state
#[tokio::test]
async fn consume_control_requests_rereads_job_state() {
    let database = database().await;
    let db = database.db();
    let (handle, stop) = stop_signal();
    let worker = worker_with(db, default_settings(), offline_bot(), stop);
    let request = control::create(db, KIND_TRIGGER_JOB, "azurlane", None).await.expect("created");
    let invoked = calls();
    let mut runners = Runners::new();
    runners.insert(
        "azurlane".to_owned(),
        fixed(JobRunResult::succeeded(&job("azurlane", true, &[])), &invoked),
    );
    let consumer = tokio::spawn({
        let worker = worker.clone();
        async move {
            // The startup snapshot had azurlane parked; the provider reports it as runnable now.
            worker
                .consume_control_requests(&runners, || Box::pin(async { Ok(vec![job("azurlane", true, &[])]) }))
                .await;
        }
    });
    for _ in 0..200 {
        if outcome(db, request.request_id).await.0 == STATUS_SUCCEEDED {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.stop();
    tokio::time::timeout(Duration::from_secs(5), consumer)
        .await
        .expect("stops")
        .expect("no panic");
    assert_eq!(recorded(&invoked), ["azurlane"]);
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_SUCCEEDED, "Completed azurlane.", "")
    );
    database.drop_database().await;
}

#[tokio::test]
async fn the_control_consumer_survives_a_failing_job_lookup() {
    let database = database().await;
    let db = database.db();
    let (handle, stop) = stop_signal();
    let worker = worker_with(db, default_settings(), offline_bot(), stop);
    let request = control::create(db, KIND_TRIGGER_JOB, "azurlane", None).await.expect("created");
    let consumer = tokio::spawn({
        let worker = worker.clone();
        async move {
            worker
                .consume_control_requests(&Runners::new(), || {
                    Box::pin(async { Err(Error::Failed("settings unreadable".to_owned())) })
                })
                .await;
        }
    });
    for _ in 0..200 {
        if outcome(db, request.request_id).await.0 == STATUS_FAILED {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!consumer.is_finished(), "the consumer keeps going (D14)");
    handle.stop();
    tokio::time::timeout(Duration::from_secs(5), consumer)
        .await
        .expect("stops")
        .expect("no panic");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_FAILED, "", "settings unreadable")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_current_jobs_reloads_settings
#[tokio::test]
async fn current_jobs_reloads_settings() {
    let database = database().await;
    let db = database.db();
    let store = Arc::new(SettingsStore::new(db.clone()));
    let worker = Worker::new(db.clone(), Arc::clone(&store), offline_bot(), stop_signal().1);
    let enabled = |jobs: &[ScheduledJob]| jobs.iter().find(|job| job.key == "nikke").expect("nikke").enabled;
    assert!(!enabled(&build_jobs(&store.load().await.expect("load"))));
    execute(
        &database,
        "INSERT INTO app_settings (section, value) VALUES ('web.nikke', '{\"enabled\": true}');",
    )
    .await;
    // The cached snapshot still has it off; the trigger path reads the table again.
    assert!(!enabled(&build_jobs(&store.load().await.expect("load"))));
    assert!(enabled(&worker.current_jobs().await.expect("jobs")));
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_marks_failed_runner
#[tokio::test]
async fn execute_control_request_marks_failed_runner() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "bilibili").await;
    let bilibili = job("bilibili", true, &[]);
    let runners = Runners::from([(
        "bilibili".to_owned(),
        fixed(JobRunResult::failed(&bilibili, "RuntimeError: boom"), &calls()),
    )]);
    worker(db)
        .execute_control_request(&request, &[bilibili], &runners)
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_FAILED, "", "RuntimeError: boom")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_marks_cancelled_runner_interrupted
#[tokio::test]
async fn execute_control_request_marks_cancelled_runner_interrupted() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "telegram").await;
    let telegram = job("telegram", true, &[]);
    let runners = Runners::from([("telegram".to_owned(), fixed(JobRunResult::cancelled(&telegram), &calls()))]);
    worker(db)
        .execute_control_request(&request, &[telegram], &runners)
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_INTERRUPTED, "", "CancelledError")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_runs_all_enabled_jobs
#[tokio::test]
async fn execute_control_request_runs_all_enabled_jobs() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "all").await;
    let invoked = calls();
    let jobs = [job("bilibili", true, &[]), job("telegram", false, &[]), job("hanime1", true, &[])];
    let runners = Runners::from([
        ("bilibili".to_owned(), fixed(JobRunResult::succeeded(&jobs[0]), &invoked)),
        ("hanime1".to_owned(), fixed(JobRunResult::succeeded(&jobs[2]), &invoked)),
    ]);
    worker(db)
        .execute_control_request(&request, &jobs, &runners)
        .await
        .expect("recorded");
    assert_eq!(recorded(&invoked), ["bilibili", "hanime1"]);
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_SUCCEEDED, "bilibili=ok, hanime1=ok", "")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_all_stops_after_cancelled_runner
#[tokio::test]
async fn execute_control_request_all_stops_after_cancelled_runner() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "all").await;
    let invoked = calls();
    let jobs = [job("telegram", true, &[]), job("bilibili", true, &[])];
    let runners = Runners::from([
        ("telegram".to_owned(), fixed(JobRunResult::cancelled(&jobs[0]), &invoked)),
        ("bilibili".to_owned(), fixed(JobRunResult::succeeded(&jobs[1]), &invoked)),
    ]);
    worker(db)
        .execute_control_request(&request, &jobs, &runners)
        .await
        .expect("recorded");
    assert_eq!(recorded(&invoked), ["telegram"]);
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(STATUS_INTERRUPTED, "telegram=interrupted", "telegram: CancelledError")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_execute_control_request_all_keeps_real_failure_over_cancellation
#[tokio::test]
async fn execute_control_request_all_keeps_real_failure_over_cancellation() {
    let database = database().await;
    let db = database.db();
    let request = claimed_request(db, "all").await;
    let jobs = [job("bilibili", true, &[]), job("telegram", true, &[])];
    let runners = Runners::from([
        (
            "bilibili".to_owned(),
            fixed(JobRunResult::failed(&jobs[0], "RuntimeError: boom"), &calls()),
        ),
        ("telegram".to_owned(), fixed(JobRunResult::cancelled(&jobs[1]), &calls())),
    ]);
    worker(db)
        .execute_control_request(&request, &jobs, &runners)
        .await
        .expect("recorded");
    assert_eq!(
        outcome(db, request.request_id).await,
        strings(
            STATUS_FAILED,
            "bilibili=failed, telegram=interrupted",
            "bilibili: RuntimeError: boom; telegram: CancelledError"
        )
    );
    database.drop_database().await;
}

// ---------- scheduled runs ----------

async fn scheduled_rows(db: &Db) -> Vec<ControlRequest> {
    control::list(db, None, 200).await.expect("list")
}

// py: tests/test_run_control_requests.py::test_scheduled_runner_records_success
#[tokio::test]
async fn scheduled_runner_records_success() {
    let database = database().await;
    let db = database.db();
    let bilibili = job("bilibili", true, &[]);
    let scheduled = worker(db).scheduled_runner(&bilibili, fixed(JobRunResult::succeeded(&bilibili), &calls()));
    scheduled().await;
    let [row] = scheduled_rows(db).await.try_into().expect("one row");
    assert_eq!(row.kind, control::KIND_SCHEDULED_JOB);
    assert_eq!(row.target, "bilibili");
    assert_eq!(
        (row.status, row.result, row.error),
        strings(STATUS_SUCCEEDED, "Completed bilibili.", "")
    );
    assert!(row.finished_at.is_some());
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_scheduled_runner_records_failure
#[tokio::test]
async fn scheduled_runner_records_failure() {
    let database = database().await;
    let db = database.db();
    let bilibili = job("bilibili", true, &[]);
    let scheduled = worker(db).scheduled_runner(&bilibili, fixed(JobRunResult::failed(&bilibili, "RuntimeError: boom"), &calls()));
    scheduled().await;
    let [row] = scheduled_rows(db).await.try_into().expect("one row");
    assert_eq!(
        (row.status, row.result, row.error),
        strings(STATUS_FAILED, "", "RuntimeError: boom")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_scheduled_runner_records_cancelled_run_as_interrupted
#[tokio::test]
async fn scheduled_runner_records_cancelled_run_as_interrupted() {
    let database = database().await;
    let db = database.db();
    let bilibili = job("bilibili", true, &[]);
    let scheduled = worker(db).scheduled_runner(&bilibili, fixed(JobRunResult::cancelled(&bilibili), &calls()));
    scheduled().await;
    let [row] = scheduled_rows(db).await.try_into().expect("one row");
    assert_eq!(
        (row.status, row.result, row.error),
        strings(STATUS_INTERRUPTED, "", "CancelledError")
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_scheduled_runner_still_runs_job_when_bookkeeping_insert_fails
#[tokio::test]
async fn scheduled_runner_still_runs_job_when_bookkeeping_insert_fails() {
    let database = database().await;
    let db = database.db();
    execute(&database, "DROP TABLE control_requests;").await;
    let bilibili = job("bilibili", true, &[]);
    let invoked = calls();
    let scheduled = worker(db).scheduled_runner(&bilibili, fixed(JobRunResult::succeeded(&bilibili), &invoked));
    let records = capture_async(scheduled()).await;
    assert_eq!(recorded(&invoked), ["bilibili"]);
    let warnings: Vec<&str> = records.iter().map(|record| record.message.as_str()).collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].starts_with("Failed to record scheduled run for bilibili: "),
        "{warnings:?}"
    );
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_scheduled_runner_swallows_result_update_failure
#[tokio::test]
async fn scheduled_runner_swallows_result_update_failure() {
    let database = database().await;
    let db = database.db();
    execute(
        &database,
        "CREATE FUNCTION refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'db down'; END $$;
         CREATE TRIGGER refuse BEFORE UPDATE ON control_requests FOR EACH ROW EXECUTE FUNCTION refuse();",
    )
    .await;
    let bilibili = job("bilibili", true, &[]);
    let scheduled = worker(db).scheduled_runner(&bilibili, fixed(JobRunResult::succeeded(&bilibili), &calls()));
    let records = capture_async(scheduled()).await;
    let [record] = records.as_slice() else { panic!("{records:?}") };
    assert!(
        record.message.starts_with("Failed to record scheduled run result for bilibili: "),
        "{record:?}"
    );
    assert!(record.message.contains("db down"), "{record:?}");
    database.drop_database().await;
}

/// The records `future` logs, run on this thread.
async fn capture_async<F: std::future::Future>(future: F) -> Vec<logging::Record> {
    // `capture` collects per thread: drive the future to completion inside it.
    let mut future = std::pin::pin!(future);
    let mut records = Vec::new();
    loop {
        let mut done = false;
        let polled = logging::capture(|| {
            let waker = std::task::Waker::noop();
            let mut context = std::task::Context::from_waker(waker);
            done = future.as_mut().poll(&mut context).is_ready();
        });
        records.extend(polled);
        if done {
            return records;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

#[tokio::test]
async fn sync_scheduled_jobs_adds_reschedules_and_removes() {
    let database = database().await;
    let db = database.db();
    let worker = worker(db);
    let scheduler = Scheduler::new(Tz::UTC);
    let mut nikke = job("nikke", true, &[]);
    let runners = Runners::from([
        ("nikke".to_owned(), fixed(JobRunResult::succeeded(&nikke), &calls())),
        (
            "jandan".to_owned(),
            fixed(JobRunResult::succeeded(&job("jandan", true, &[])), &calls()),
        ),
    ]);
    let mut needs_tool = job("jandan", true, &[]);
    needs_tool.required_commands = vec!["fav-no-such-command".to_owned()];
    let unported = job("bilibili", true, &[]);
    let records = logging::capture(|| {
        assert_eq!(
            worker.sync_scheduled_jobs(&scheduler, &[nikke.clone(), needs_tool.clone(), unported.clone()], &runners),
            1
        );
    });
    assert_eq!(scheduler.job_ids(), ["nikke"]);
    let messages: Vec<&str> = records.iter().map(|record| record.message.as_str()).collect();
    assert_eq!(
        messages,
        [
            "Scheduled Nikke with cron '*/30 * * * *'",
            "Jandan requires command(s) fav-no-such-command in PATH; not scheduling",
        ]
    );
    // Unchanged: nothing to do.
    assert_eq!(worker.sync_scheduled_jobs(&scheduler, std::slice::from_ref(&nikke), &runners), 0);
    nikke.cron = "0 4 * * *".to_owned();
    let records = logging::capture(|| assert_eq!(worker.sync_scheduled_jobs(&scheduler, std::slice::from_ref(&nikke), &runners), 1));
    assert_eq!(records[0].message, "Rescheduled Nikke with cron '0 4 * * *'");
    nikke.cron = "61 * * * *".to_owned();
    let records = logging::capture(|| assert_eq!(worker.sync_scheduled_jobs(&scheduler, std::slice::from_ref(&nikke), &runners), 1));
    assert!(
        records[0]
            .message
            .starts_with("Invalid cron for Nikke (61 * * * *); not scheduling\nValueError: "),
        "{records:?}"
    );
    assert!(scheduler.job_ids().is_empty());
    nikke.cron = "0 4 * * *".to_owned();
    worker.sync_scheduled_jobs(&scheduler, std::slice::from_ref(&nikke), &runners);
    nikke.enabled = false;
    let records = logging::capture(|| assert_eq!(worker.sync_scheduled_jobs(&scheduler, std::slice::from_ref(&nikke), &runners), 1));
    assert_eq!(records[0].message, "Unscheduled Nikke");
    assert!(scheduler.job_ids().is_empty());
    database.drop_database().await;
}

// ---------- one run ----------

// py: tests/test_run_control_requests.py::test_run_job_enqueues_job_failed_notification
#[tokio::test]
async fn run_job_enqueues_job_failed_notification() {
    let database = database().await;
    let db = database.db();
    let bilibili = job("bilibili", true, &[]);
    let result = worker(db)
        .run_job(&bilibili, source(Outcome::Fail(JobError::new("RuntimeError", "boom")), &calls()))
        .await;
    assert!(!result.success);
    assert_eq!(result.error, "RuntimeError: boom");
    let [row] = notifications(db).await.try_into().expect("one notification");
    assert_eq!(row.kind, "job_failed");
    assert_eq!(row.source, "worker");
    assert_eq!(row.header, "Bilibili");
    assert_eq!(row.title, "Job failed");
    assert_eq!(row.body, "RuntimeError: boom");
    assert_eq!(row.scope, SCOPE_JOB);
    assert_eq!(payload_str(&row, "job"), "bilibili");
    assert_eq!(payload_str(&row, "error_class"), "RuntimeError");
    assert_eq!(payload_str(&row, "error_message"), "boom");
    // No exception-specific key, so the failure folds into the job's single run-level row.
    assert_eq!(row.dedupe_key, "job_failed:bilibili:run");
    assert_eq!(payload_str(&row, "dedupe_key"), "job_failed:bilibili:run");
    let started = payload_str(&row, "started_at");
    assert!(started.ends_with('Z') && started.len() == 20, "{started}");
    assert!(matches!(row.payload_json().get("elapsed_seconds"), Some(Value::Float(_))));
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_skips_job_failed_notification_for_a_muted_job
#[tokio::test]
async fn run_job_skips_job_failed_notification_for_a_muted_job() {
    let database = database().await;
    let db = database.db();
    let mut bilibili = job("bilibili", true, &[]);
    bilibili.notify = false;
    let result = worker(db)
        .run_job(&bilibili, source(Outcome::Fail(JobError::new("RuntimeError", "boom")), &calls()))
        .await;
    // Still a failed run, just an unannounced one.
    assert!(!result.success);
    assert_eq!(result.error, "RuntimeError: boom");
    assert!(notifications(db).await.is_empty());
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_uses_exception_notification_dedupe_key
#[tokio::test]
async fn run_job_uses_exception_notification_dedupe_key() {
    let database = database().await;
    let db = database.db();
    let error = JobError::new("_DownloadError", "temporary network error").with_notification_dedupe_key("bilibili:download:BV1TEST");
    let result = worker(db)
        .run_job(&job("bilibili", true, &[]), source(Outcome::Fail(error), &calls()))
        .await;
    assert!(!result.success);
    let [row] = notifications(db).await.try_into().expect("one notification");
    assert_eq!(row.dedupe_key, "job_failed:bilibili:bilibili:download:BV1TEST");
    assert_eq!(row.scope, SCOPE_JOB);
    assert_eq!(payload_str(&row, "dedupe_key"), "job_failed:bilibili:bilibili:download:BV1TEST");
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_dedupes_hanime1_parser_failure_notification
#[tokio::test]
async fn run_job_dedupes_hanime1_parser_failure_notification() {
    let database = database().await;
    let db = database.db();
    // What `Hanime1ParserIncompatibleError` carries; the source itself is ported later.
    let error = JobError::new("Hanime1ParserIncompatibleError", "playlist structure changed")
        .with_notification_dedupe_key("hanime1:parser:playlist-wrapper");
    worker(db)
        .run_job(&job("hanime1", true, &[]), source(Outcome::Fail(error), &calls()))
        .await;
    let [row] = notifications(db).await.try_into().expect("one notification");
    assert_eq!(row.dedupe_key, "job_failed:hanime1:hanime1:parser:playlist-wrapper");
    assert_eq!(payload_str(&row, "error_class"), "Hanime1ParserIncompatibleError");
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_resolves_job_failed_notifications_on_success
#[tokio::test]
async fn run_job_resolves_job_failed_notifications_on_success() {
    let database = database().await;
    let db = database.db();
    let worker = worker(db);
    let twitter = job("twitter", true, &[]);
    worker
        .run_job(&twitter, source(Outcome::Fail(JobError::new("RuntimeError", "boom")), &calls()))
        .await;
    let result = worker.run_job(&twitter, source(Outcome::Succeed, &calls())).await;
    assert!(result.success);
    let [row] = notifications(db).await.try_into().expect("one notification");
    assert_eq!(row.webhook_action, WEBHOOK_ACTION_RESOLVE);
    assert_eq!(row.kind, "job_recovered");
    assert_eq!(row.header, "Twitter");
    assert_eq!(row.title, "Job recovered");
    assert_eq!(row.body, "Twitter completed successfully again.");
    assert_eq!(payload_str(&row, "job"), "twitter");
    assert!(payload_str(&row, "finished_at").ends_with('Z'));
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_success_survives_a_failing_resolve
#[tokio::test]
async fn run_job_success_survives_a_failing_resolve() {
    let database = database().await;
    let db = database.db();
    execute(&database, "DROP TABLE notifications;").await;
    let result = worker(db)
        .run_job(&job("twitter", true, &[]), source(Outcome::Succeed, &calls()))
        .await;
    assert!(result.success);
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_does_not_resolve_after_a_failure
#[tokio::test]
async fn run_job_does_not_resolve_after_a_failure() {
    let database = database().await;
    let db = database.db();
    let worker = worker(db);
    let twitter = job("twitter", true, &[]);
    for _ in 0..2 {
        let result = worker
            .run_job(&twitter, source(Outcome::Fail(JobError::new("RuntimeError", "boom")), &calls()))
            .await;
        assert!(!result.success);
    }
    let [row] = notifications(db).await.try_into().expect("one notification");
    assert_eq!(row.webhook_action, WEBHOOK_ACTION_UPSERT);
    assert_eq!(row.occurrence_count, 2);
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_run_job_handles_cancelled_error_and_closes_worker
#[tokio::test]
async fn run_job_handles_cancelled_error_and_closes_worker() {
    let database = database().await;
    let db = database.db();
    let (handle, stop) = stop_signal();
    let worker = worker_with(db, default_settings(), offline_bot(), stop);
    let log = calls();
    handle.stop();
    let result = worker.run_job(&job("telegram", true, &[]), source(Outcome::Hang, &log)).await;
    assert!(!result.success);
    assert_eq!(result.error, "CancelledError");
    assert!(result.cancelled);
    assert_eq!(recorded(&log), ["update", "close"]);
    // A cancellation is a shutdown, not a failure worth a (pinned) notification.
    assert!(notifications(db).await.is_empty());
    database.drop_database().await;
}

struct FakeResident {
    calls: Arc<StdMutex<Vec<String>>>,
}

impl ResidentSource for FakeResident {
    fn run(&self, stop: StopSignal) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            stop.wait().await;
            Ok(())
        })
    }

    fn wait_until_ready(&self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async { Ok(()) })
    }

    fn update(&self) -> BoxFuture<'_, Result<(), JobError>> {
        self.calls.lock().expect("calls").push("update".to_owned());
        Box::pin(async { Ok(()) })
    }

    fn close(&self) -> BoxFuture<'_, Result<(), JobError>> {
        self.calls.lock().expect("calls").push("close".to_owned());
        Box::pin(async { Ok(()) })
    }
}

// py: tests/test_run_control_requests.py::test_run_job_can_reuse_singleton_worker_without_closing_it
#[tokio::test]
async fn run_job_can_reuse_singleton_worker_without_closing_it() {
    let database = database().await;
    let db = database.db();
    let log = calls();
    let resident: Arc<dyn ResidentSource> = Arc::new(FakeResident { calls: Arc::clone(&log) });
    let telegram = job("telegram", true, &[]);
    let factory_calls = calls();
    let factory_log = Arc::clone(&factory_calls);
    // The scheduled Telegram trigger must reuse its runtime, never make another.
    let kind = SourceKind::Resident(Arc::new(move || {
        factory_log.lock().expect("calls").push("factory".to_owned());
        Arc::new(FakeResident { calls: calls() }) as Arc<dyn ResidentSource>
    }));
    let runner = worker(db).runner(telegram, kind, Some(resident));
    let result = runner().await;
    assert!(result.success);
    assert_eq!(recorded(&log), ["update"]);
    assert!(recorded(&factory_calls).is_empty());
    database.drop_database().await;
}

#[tokio::test]
async fn a_runner_skips_a_trigger_while_its_job_runs() {
    let database = database().await;
    let db = database.db();
    let (handle, stop) = stop_signal();
    let worker = worker_with(db, default_settings(), offline_bot(), stop);
    let log = calls();
    let factory_log = Arc::clone(&log);
    let kind = SourceKind::PerRun(Arc::new(move || {
        Box::new(FakeSource {
            outcome: Outcome::Hang,
            calls: Arc::clone(&factory_log),
        }) as Box<dyn Source>
    }));
    let runner = worker.runner(job("nikke", true, &[]), kind, None);
    let first = tokio::spawn({
        let runner = Arc::clone(&runner);
        async move { runner().await }
    });
    while recorded(&log).is_empty() {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let skipped = runner().await;
    assert_eq!(skipped.error, "Job is already running");
    assert!(!skipped.cancelled);
    handle.stop();
    assert!(first.await.expect("first run").cancelled);
    assert_eq!(recorded(&log), ["update", "close"]);
    database.drop_database().await;
}

// ---------- notification delivery ----------

fn telegram_settings(enabled: bool) -> Settings {
    let mut settings = default_settings();
    settings.notifications.telegram.enabled = enabled;
    settings.notifications.telegram.bot_token = "123:token".to_owned();
    settings.notifications.telegram.chat_id = "-100123".to_owned();
    settings
}

async fn queue_failure(worker: &Worker) -> NotificationRecord {
    worker
        .outbox
        .enqueue(&NewNotification {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            header: "Bilibili".to_owned(),
            title: "Job failed".to_owned(),
            body: "RuntimeError: boom".to_owned(),
            dedupe_key: "job_failed:bilibili:bilibili:download:BV1TEST".to_owned(),
            ..NewNotification::default()
        })
        .await
        .expect("enqueued")
        .expect("not muted")
}

// py: tests/test_run_control_requests.py::test_deliver_next_notification_uses_direct_telegram
#[tokio::test]
async fn deliver_next_notification_uses_direct_telegram() {
    let database = database().await;
    let db = database.db();
    let api = FakeBotApi::start().await;
    let worker = worker_with(db, telegram_settings(true), api.client(), stop_signal().1);
    let queued = queue_failure(&worker).await;
    assert!(worker.deliver_next_notification().await.expect("delivered"));
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage"]);
    let sent = api.received()[0].json();
    assert_eq!(sent["chat_id"], "-100123");
    assert_eq!(sent["text"], "FAV · Bilibili\n*Job failed*\nRuntimeError: boom");
    let [row] = notifications(db).await.try_into().expect("one row");
    assert_eq!(row.notification_id, queued.notification_id);
    assert_eq!(row.delivery_status, DELIVERY_DELIVERED);
    assert_eq!(row.telegram_message_id, Some(99));
    assert_eq!(row.telegram_pinned_message_id, Some(99));
    // Nothing left to send.
    assert!(!worker.deliver_next_notification().await.expect("idle"));
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_deliver_next_notification_leaves_queue_untouched_when_telegram_is_disabled
#[tokio::test]
async fn deliver_next_notification_leaves_queue_untouched_when_telegram_is_disabled() {
    let database = database().await;
    let db = database.db();
    let api = FakeBotApi::start().await;
    let worker = worker_with(db, telegram_settings(false), api.client(), stop_signal().1);
    queue_failure(&worker).await;
    assert!(!worker.deliver_next_notification().await.expect("nothing claimed"));
    assert!(api.received().is_empty());
    let [row] = notifications(db).await.try_into().expect("one row");
    assert_eq!(row.delivery_status, DELIVERY_PENDING);
    assert_eq!(row.event_version, 1, "never claimed");
    database.drop_database().await;
}

// py: tests/test_run_control_requests.py::test_direct_telegram_retry_uses_server_retry_after
#[tokio::test]
async fn direct_telegram_retry_uses_server_retry_after() {
    let database = database().await;
    let db = database.db();
    let api = FakeBotApi::start().await;
    api.script(
        "sendMessage",
        vec![Answer::json(
            429,
            json!({"ok": false, "error_code": 429, "description": "Telegram rate limited", "parameters": {"retry_after": 120}}),
        )],
    );
    let worker = worker_with(db, telegram_settings(true), api.client(), stop_signal().1);
    queue_failure(&worker).await;
    let claimed = worker.outbox.claim_next_pending().await.expect("claim").expect("due");
    sqlx::query("UPDATE notifications SET attempt_count = 2 WHERE id = $1;")
        .bind(claimed.notification_id)
        .execute(db.pool())
        .await
        .expect("attempts");
    let claimed = NotificationRecord {
        attempt_count: 2,
        ..claimed
    };
    let config = load_config(&telegram_settings(true), true).expect("configured");
    worker.deliver_notification(&claimed, &config).await.expect("recorded");
    let [row] = notifications(db).await.try_into().expect("one row");
    assert_eq!(row.delivery_status, DELIVERY_PENDING);
    assert_eq!(row.attempt_count, 3);
    assert_eq!(row.last_error, "Telegram Bot API responded with error 429: Telegram rate limited");
    let delay = row.next_attempt_at - Utc::now();
    assert!(
        delay > chrono::Duration::seconds(110),
        "the server's 120 s beat the 120 s backoff floor: {delay}"
    );

    // A permanent error marks the row failed.
    api.script(
        "sendMessage",
        vec![Answer::json(400, json!({"ok": false, "error_code": 400, "description": "bad"}))],
    );
    sqlx::query("UPDATE notifications SET delivery_status = 'sending' WHERE id = $1;")
        .bind(row.notification_id)
        .execute(db.pool())
        .await
        .expect("resend");
    worker.deliver_notification(&row, &config).await.expect("recorded");
    let [row] = notifications(db).await.try_into().expect("one row");
    assert_eq!(row.delivery_status, crate::notifications::DELIVERY_FAILED);
    assert_eq!(row.attempt_count, 4);
    database.drop_database().await;
}

// ---------- main and trigger ----------

fn nikke_settings(telegram: bool) -> Settings {
    let mut settings = if telegram { telegram_settings(true) } else { default_settings() };
    settings.job_schedule_mut("nikke").expect("nikke").enabled = true;
    settings
}

fn options(database: &TestDatabase, settings: Settings, registry: Registry, bot: BotClient) -> WorkerOptions {
    WorkerOptions {
        // Its own pool: the worker closes the one it is given when it stops.
        db: Db::from_options((*database.db().pool().connect_options()).clone()),
        settings: Arc::new(SettingsStore::pinned(settings)),
        registry,
        bot,
        timezone: Tz::UTC,
    }
}

fn registry(outcome: Outcome, log: &Arc<StdMutex<Vec<String>>>) -> Registry {
    let log = Arc::clone(log);
    let mut registry = Registry::new();
    registry.per_run("nikke", move || {
        Box::new(FakeSource {
            outcome: outcome.clone(),
            calls: Arc::clone(&log),
        })
    });
    registry
}

// py: tests/test_run_control_requests.py::test_main_starts_notification_consumer_and_closes_client
#[tokio::test]
async fn main_starts_notification_consumer_and_closes_client() {
    let database = database().await;
    let db = database.db().clone();
    // A run the previous worker left behind, and a trigger waiting for this one.
    let leftover = control::record_scheduled_run_start(&db, "nikke").await.expect("leftover");
    let request = control::create(&db, KIND_TRIGGER_JOB, "nikke", None).await.expect("created");
    let log = calls();
    let (handle, _) = stop_signal();
    let worker = tokio::spawn(run_worker(
        options(&database, nikke_settings(false), registry(Outcome::Succeed, &log), offline_bot()),
        handle.clone(),
    ));
    for _ in 0..250 {
        if outcome(&db, request.request_id).await.0 != control::STATUS_PENDING && outcome(&db, request.request_id).await.0 != STATUS_RUNNING
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.stop();
    tokio::time::timeout(Duration::from_secs(15), worker)
        .await
        .expect("stops")
        .expect("no panic")
        .expect("clean exit");
    // Leftover rows are closed before the consumer can claim anything: the trigger went
    // through instead of being swept up as interrupted.
    assert_eq!(outcome(&db, leftover).await.0, STATUS_INTERRUPTED);
    assert_eq!(
        outcome(&db, request.request_id).await,
        strings(STATUS_SUCCEEDED, "Completed nikke.", "")
    );
    assert_eq!(recorded(&log), ["update", "close"]);
    database.drop_database().await;
}

#[tokio::test]
async fn trigger_runs_the_job_once_and_delivers_what_it_queued() {
    let database = database().await;
    let api = FakeBotApi::start().await;
    let log = calls();
    let failing = Outcome::Fail(JobError::new("RuntimeError", "boom"));
    run_trigger(
        options(&database, nikke_settings(true), registry(failing, &log), api.client()),
        " NIKKE ",
        stop_signal().1,
    )
    .await
    .expect("a failed job still exits cleanly");
    assert_eq!(recorded(&log), ["update", "close"]);
    assert_eq!(api.methods(), ["sendMessage", "pinChatMessage"]);
    assert_eq!(api.received()[0].json()["text"], "FAV · Nikke\n*Job failed*\nRuntimeError: boom");
    database.drop_database().await;
}

#[tokio::test]
async fn trigger_refuses_unknown_and_unported_targets() {
    let database = database().await;
    let log = calls();
    let error = run_trigger(
        options(&database, nikke_settings(false), registry(Outcome::Succeed, &log), offline_bot()),
        "nope",
        stop_signal().1,
    )
    .await
    .expect_err("unknown");
    assert!(
        matches!(&error, WorkerError::Exit(Some(message)) if message.starts_with("Unknown --trigger target: 'nope'. Valid targets: all, ")),
        "{error:?}"
    );
    let error = run_trigger(
        options(&database, nikke_settings(false), registry(Outcome::Succeed, &log), offline_bot()),
        "jandan",
        stop_signal().1,
    )
    .await
    .expect_err("unported");
    assert!(
        matches!(&error, WorkerError::Exit(Some(message)) if message == "Runner unavailable for job: jandan"),
        "{error:?}"
    );

    assert!(recorded(&log).is_empty());
    database.drop_database().await;
}

#[test]
fn a_missing_command_ends_a_trigger_before_anything_runs() {
    let mut needs_tool = job("bilibili", false, &[]);
    needs_tool.required_commands = vec!["fav-no-such-command".to_owned(), "sh".to_owned()];
    let records = logging::capture(|| assert!(matches!(validate_commands(&[needs_tool]), Err(WorkerError::Exit(None)))));
    let messages: Vec<&str> = records.iter().map(|record| record.message.as_str()).collect();
    // Disabled jobs are checked too: a trigger runs them anyway.
    assert_eq!(messages, ["Bilibili requires command 'fav-no-such-command' in PATH"]);
    assert!(validate_commands(&[job("nikke", true, &[])]).is_ok());
}

#[test]
fn the_scheduler_timezone_falls_back_to_utc() {
    assert_eq!(resolve_scheduler_timezone(None), Tz::UTC);
    assert_eq!(resolve_scheduler_timezone(Some("  ")), Tz::UTC);
    assert_eq!(resolve_scheduler_timezone(Some(" Asia/Shanghai ")), Tz::Asia__Shanghai);
    let records = logging::capture(|| assert_eq!(resolve_scheduler_timezone(Some("Mars/Base")), Tz::UTC));
    assert_eq!(records[0].message, "Invalid TZ 'Mars/Base', fallback to UTC");
}

#[test]
fn failure_payload_times_are_whole_utc_seconds() {
    let when = DateTime::parse_from_rfc3339("2026-03-02T01:02:03.987654+08:00")
        .expect("time")
        .to_utc();
    assert_eq!(serialize_datetime(when), "2026-03-01T17:02:03Z");
    assert!(matches!(round3(1.234_56), Value::Float(value) if (value - 1.235).abs() < 1e-12));
}
