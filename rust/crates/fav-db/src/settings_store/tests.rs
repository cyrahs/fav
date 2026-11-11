use super::*;
use crate::testdb::TestDatabase;
use fav_core::pyjson::{Dict, loads};

fn object(json: &str) -> Value {
    loads(json).expect("test JSON")
}

// py: tests/test_schedule_config.py::test_notify_enabled_follows_the_source_toggle
#[tokio::test]
async fn notify_enabled_follows_the_source_toggle() {
    let mut settings = default_settings();
    let store = SettingsStore::pinned(settings.clone());
    assert!(store.notify_enabled("bilibili").await.expect("pinned"));

    settings.job_schedule_mut("bilibili").expect("bilibili is a job").notify = false;
    store.pin(Some(settings));

    assert!(!store.notify_enabled("bilibili").await.expect("pinned"));
    // 'worker' is the job-failure source, not a section; it has no toggle to read.
    assert!(store.notify_enabled("worker").await.expect("pinned"));
}

#[tokio::test]
async fn a_pinned_store_has_no_database() {
    let store = SettingsStore::pinned(default_settings());
    assert!(matches!(
        store.load_section("web.jandan").await,
        Err(SettingsStoreError::Db(DbError::NoDatabase))
    ));
    store.pin(None);
    assert!(matches!(store.load().await, Err(SettingsStoreError::Db(DbError::NoDatabase))));
}

async fn store_with_schema(ttl: Duration) -> (TestDatabase, SettingsStore) {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema");
    let store = SettingsStore::with_ttl(Some(database.db().clone()), ttl);
    (database, store)
}

#[tokio::test]
async fn an_empty_table_gives_the_defaults() {
    let (database, store) = store_with_schema(CACHE_TTL).await;
    assert_eq!(*store.load().await.expect("load"), default_settings());
    assert_eq!(store.settings_version().await.expect("version"), "");
    let section = store.load_section("web.jandan").await.expect("defaults");
    assert_eq!(
        section,
        SectionValue::validate("web.jandan", &Value::Dict(Dict::new())).expect("defaults")
    );
    database.drop_database().await;
}

#[tokio::test]
async fn saved_sections_round_trip_through_jsonb() {
    let (database, store) = store_with_schema(CACHE_TTL).await;
    let payload =
        object(r#"{"enabled": true, "cron": " 0 3 * * * ", "runtime_capture_timeout_seconds": 5, "path": "./collection//nikke/"}"#);
    let saved = store.save_section("web.nikke", &payload).await.expect("save");
    let stored: String = sqlx::query_scalar("SELECT value::text FROM app_settings WHERE section = 'web.nikke'")
        .fetch_one(database.db().pool())
        .await
        .expect("row");
    // What Python's json.dumps(model_dump) stored, after JSONB's own key ordering.
    let stored = loads(&stored).expect("JSON");
    let sorted = |value: &Value| fav_core::pyjson::dumps(value, &DumpOptions::sorted_plain()).expect("dump");
    assert_eq!(sorted(&stored), sorted(&saved.dump()));
    assert_eq!(stored.get("runtime_capture_timeout_seconds"), Some(&Value::Float(5.0)));
    assert_eq!(stored.get("path"), Some(&Value::from("collection/nikke")));
    assert_eq!(store.load_section("web.nikke").await.expect("load"), saved);
    let loaded = store.load().await.expect("load");
    assert!(loaded.job_schedule("nikke").expect("nikke").enabled);

    let version = store.settings_version().await.expect("version");
    assert!(is_isoformat_utc(&version), "{version}");
    let stored_at: DateTime<Utc> = sqlx::query_scalar("SELECT updated_at FROM app_settings WHERE section = 'web.nikke'")
        .fetch_one(database.db().pool())
        .await
        .expect("row");
    assert_eq!(DateTime::parse_from_rfc3339(&version).expect("RFC 3339"), stored_at);
    database.drop_database().await;
}

#[tokio::test]
async fn saving_rejects_unknown_sections_and_invalid_payloads() {
    let (database, store) = store_with_schema(CACHE_TTL).await;
    let unknown = store.save_section("web.unknown", &object("{}")).await.expect_err("unknown");
    assert_eq!(unknown.to_string(), "Unknown settings section: web.unknown");
    let invalid = store
        .save_section("web.jandan", &object(r#"{"cron": "* * *"}"#))
        .await
        .expect_err("invalid");
    let SettingsStoreError::Section(SectionError::Invalid(error)) = invalid else {
        panic!("{invalid:?}")
    };
    assert_eq!(error.errors[0].msg, "Value error, cron must contain exactly 5 fields");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM app_settings")
        .fetch_one(database.db().pool())
        .await
        .expect("count");
    assert_eq!(count, 0);
    assert!(matches!(
        store.load_section("cookiecloud").await,
        Err(SettingsStoreError::Section(SectionError::Unknown(_)))
    ));
    database.drop_database().await;
}

#[tokio::test]
async fn rows_of_unknown_sections_are_ignored() {
    let (database, store) = store_with_schema(CACHE_TTL).await;
    sqlx::raw_sql(
        r#"INSERT INTO app_settings (section, value) VALUES
           ('cookiecloud', '{"configs": [{"name": "main", "password": "p"}]}'),
           ('web.future', '{"enabled": true}'),
           ('web.wechat', '{"token": "t", "nga_cookiecloud": "main"}');"#,
    )
    .execute(database.db().pool())
    .await
    .expect("rows");
    let loaded = store.load().await.expect("load");
    let wechat = loaded.section("web.wechat").expect("known section").dump();
    assert_eq!(wechat.get("token"), Some(&Value::from("t")));
    assert_eq!(wechat.get("nga_cookiecloud"), None);
    database.drop_database().await;
}

#[tokio::test]
async fn snapshots_are_cached_for_the_ttl() {
    let (database, store) = store_with_schema(Duration::from_millis(300)).await;
    let set_enabled = async |enabled: bool| {
        sqlx::query(
            "INSERT INTO app_settings (section, value) VALUES ('web.jandan', $1::jsonb)
             ON CONFLICT (section) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(format!(r#"{{"enabled": {enabled}}}"#))
        .execute(database.db().pool())
        .await
        .expect("write");
    };
    let enabled = async || store.load().await.expect("load").job_schedule("jandan").expect("jandan").enabled;
    assert!(!enabled().await);
    set_enabled(true).await;
    assert!(!enabled().await, "still the cached snapshot");
    assert!(
        store
            .load_force()
            .await
            .expect("forced")
            .job_schedule("jandan")
            .expect("jandan")
            .enabled
    );
    set_enabled(false).await;
    store.invalidate_cache();
    assert!(!enabled().await, "invalidated");
    set_enabled(true).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert!(enabled().await, "expired");
    // A save invalidates the cache too.
    store
        .save_section("web.jandan", &object(r#"{"enabled": false}"#))
        .await
        .expect("save");
    assert!(!enabled().await);
    database.drop_database().await;
}

/// `datetime.isoformat()` of a UTC time: `YYYY-MM-DDTHH:MM:SS[.ffffff]+00:00`.
fn is_isoformat_utc(text: &str) -> bool {
    let shape = |part: &str, pattern: &str| {
        part.len() == pattern.len()
            && part
                .bytes()
                .zip(pattern.bytes())
                .all(|(byte, want)| if want == b'9' { byte.is_ascii_digit() } else { byte == want })
    };
    let Some(body) = text.strip_suffix("+00:00") else { return false };
    match body.split_once('.') {
        Some((whole, fraction)) => shape(whole, "9999-99-99T99:99:99") && shape(fraction, "999999"),
        None => shape(body, "9999-99-99T99:99:99"),
    }
}

#[test]
fn isoformat_shape_check() {
    assert!(is_isoformat_utc("2026-11-10T01:02:03.456789+00:00"));
    assert!(is_isoformat_utc("2026-11-10T01:02:03+00:00"));
    assert!(!is_isoformat_utc("2026-11-10 01:02:03+00:00"));
    assert!(!is_isoformat_utc("2026-11-10T01:02:03.4567+00:00"));
    assert!(!is_isoformat_utc("2026-11-10T01:02:03Z"));
}
