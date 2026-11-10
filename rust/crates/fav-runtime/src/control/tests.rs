use fav_core::pyjson::{Dict, Value};
use fav_db::Db;

use super::*;
use crate::testutil::{database, execute};

// py: tests/test_control_queue.py::test_record_scheduled_run_start_inserts_running_row
#[tokio::test]
async fn record_scheduled_run_start_inserts_running_row() {
    let database = database().await;
    let db = database.db();
    let request_id = record_scheduled_run_start(db, "bilibili").await.expect("recorded");
    let row = get(db, request_id).await.expect("query").expect("row exists");
    assert_eq!(row.kind, KIND_SCHEDULED_JOB);
    assert_eq!(row.target, "bilibili");
    assert_eq!(row.status, STATUS_RUNNING);
    assert!(row.started_at.is_some());
    assert_eq!(row.finished_at, None);
    // Born running, so the claim loop never runs it again.
    assert_eq!(claim_next(db).await.expect("claim"), None);
    database.drop_database().await;
}

// py: tests/test_control_queue.py::test_record_scheduled_run_start_raises_when_insert_returns_nothing
#[tokio::test]
async fn record_scheduled_run_start_raises_when_insert_returns_nothing() {
    let database = database().await;
    // A trigger that swallows the insert makes RETURNING come back empty.
    execute(
        &database,
        "CREATE FUNCTION swallow() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END $$;
         CREATE TRIGGER swallow BEFORE INSERT ON control_requests FOR EACH ROW EXECUTE FUNCTION swallow();",
    )
    .await;
    let error = record_scheduled_run_start(database.db(), "bilibili").await.expect_err("nothing returned");
    assert!(matches!(&error, Error::Failed(message) if message == "Failed to record scheduled run"), "{error:?}");
    database.drop_database().await;
}

// py: tests/test_control_queue.py::test_list_control_requests_sync_rejects_unknown_status
#[tokio::test]
async fn list_control_requests_rejects_unknown_status() {
    // Validation fires before any database connection is attempted: this pool points nowhere.
    let db = Db::connect_lazy("postgresql://nobody@127.0.0.1:1/never").expect("lazy pool");
    let error = list(&db, Some(&["succeeded", "bogus"]), 50).await.expect_err("refused");
    assert!(matches!(&error, Error::Invalid(message) if message == "Unsupported control request status: bogus"), "{error:?}");
    let error = list(&db, Some(&["zz", "bogus", "zz"]), 50).await.expect_err("refused");
    assert!(matches!(&error, Error::Invalid(message) if message == "Unsupported control request status: bogus, zz"), "{error:?}");
}

// py: tests/test_control_queue.py::test_interrupt_running_control_requests_marks_rows
#[tokio::test]
async fn interrupt_running_control_requests_marks_rows() {
    let database = database().await;
    let db = database.db();
    let fresh = record_scheduled_run_start(db, "bilibili").await.expect("row");
    let old = record_scheduled_run_start(db, "kemono").await.expect("row");
    // Every running row, however recent: at startup none of them can still be running.
    execute(
        &database,
        "UPDATE control_requests SET started_at = CURRENT_TIMESTAMP - INTERVAL '3 days', error = 'earlier note' WHERE target = 'kemono';",
    )
    .await;
    let pending = create(db, KIND_TRIGGER_JOB, "jandan", None).await.expect("row");
    let done = record_scheduled_run_start(db, "nikke").await.expect("row");
    update(db, done, STATUS_SUCCEEDED, "Completed nikke.", "").await.expect("update");

    assert_eq!(interrupt_running(db).await.expect("interrupt"), 2);

    let fresh = get(db, fresh).await.expect("query").expect("row");
    assert_eq!(fresh.status, STATUS_INTERRUPTED);
    assert_eq!(fresh.error, INTERRUPTED_BY_RESTART_ERROR);
    assert!(fresh.finished_at.is_some());
    let old = get(db, old).await.expect("query").expect("row");
    assert_eq!(old.status, STATUS_INTERRUPTED);
    assert_eq!(old.error, "earlier note");
    assert_eq!(get(db, pending.request_id).await.expect("query").expect("row").status, STATUS_PENDING);
    assert_eq!(get(db, done).await.expect("query").expect("row").status, STATUS_SUCCEEDED);
    database.drop_database().await;
}

#[tokio::test]
async fn requests_are_claimed_oldest_first_and_listed_newest_first() {
    let database = database().await;
    let db = database.db();
    let mut payload = Dict::new();
    payload.insert("note", Value::from("中文"));
    let first = create(db, KIND_TRIGGER_JOB, "jandan", Some(&payload)).await.expect("row");
    assert_eq!(first.payload, r#"{"note":"中文"}"#);
    assert_eq!(first.payload_json(), payload);
    let second = create(db, KIND_TRIGGER_JOB, "all", None).await.expect("row");
    assert_eq!(second.payload, "{}");

    let claimed = claim_next(db).await.expect("claim").expect("a pending row");
    assert_eq!(claimed.request_id, first.request_id);
    assert_eq!(claimed.status, STATUS_RUNNING);
    assert!(claimed.started_at.is_some());

    let listed = list(db, None, 50).await.expect("list");
    assert_eq!(listed.iter().map(|row| row.request_id).collect::<Vec<_>>(), [second.request_id, first.request_id]);
    let running = list(db, Some(&[STATUS_RUNNING]), 0).await.expect("list");
    assert_eq!(running.iter().map(|row| row.request_id).collect::<Vec<_>>(), [first.request_id]);

    let error = create(db, "bogus", "all", None).await.expect_err("refused");
    assert!(matches!(&error, Error::Invalid(message) if message == "Unsupported control request kind: bogus"), "{error:?}");
    let error = update(db, first.request_id, "done", "", "").await.expect_err("refused");
    assert!(matches!(&error, Error::Invalid(message) if message == "Unsupported control request status: done"), "{error:?}");
    database.drop_database().await;
}

#[test]
fn payload_json_reads_only_objects() {
    let mut request = ControlRequest {
        request_id: 1,
        kind: KIND_TRIGGER_JOB.to_owned(),
        target: "all".to_owned(),
        payload: "not json".to_owned(),
        status: STATUS_PENDING.to_owned(),
        requested_at: chrono::Utc::now(),
        started_at: None,
        finished_at: None,
        result: String::new(),
        error: String::new(),
    };
    assert!(request.payload_json().is_empty());
    request.payload = "[1]".to_owned();
    assert!(request.payload_json().is_empty());
}
