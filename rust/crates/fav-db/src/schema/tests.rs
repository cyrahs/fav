use super::*;
use crate::testdb::TestDatabase;

/// The catalog query `rust/tools/schema/dump_schema.py` runs against the Python schema.
const DESCRIBE_SQL: &str = include_str!("../../../../tools/schema/describe.sql");
/// What the Python baseline creates on an empty database.
const PYTHON_SCHEMA: &str = include_str!("../../../../parity/schema.txt");

async fn describe(db: &Db) -> Vec<String> {
    sqlx::query_scalar::<_, String>(DESCRIBE_SQL)
        .fetch_all(db.pool())
        .await
        .expect("describe the schema")
}

fn python_schema() -> Vec<String> {
    PYTHON_SCHEMA
        .lines()
        .filter(|line| !line.starts_with('#') && !line.is_empty())
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn creates_exactly_what_python_creates() {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("apply the schema");
    let have = describe(database.db()).await;
    let want = python_schema();
    let missing: Vec<&String> = want.iter().filter(|line| !have.contains(line)).collect();
    let extra: Vec<&String> = have.iter().filter(|line| !want.contains(line)).collect();
    assert!(missing.is_empty() && extra.is_empty(), "missing: {missing:#?}\nextra: {extra:#?}");
    assert_eq!(have, want);
    assert_eq!(want.iter().filter(|line| line.starts_with("table ")).count(), 43);
    database.drop_database().await;
}

#[tokio::test]
async fn is_idempotent_and_safe_to_run_concurrently() {
    let database = TestDatabase::create().await;
    let db = database.db();
    let (first, second) = tokio::join!(db.ensure_schema(), db.ensure_schema());
    first.expect("first run");
    second.expect("concurrent run");
    db.ensure_schema().await.expect("third run");
    assert_eq!(describe(db).await, python_schema());
    database.drop_database().await;
}

#[tokio::test]
async fn adds_columns_a_database_from_before_them_lacks() {
    let database = TestDatabase::create().await;
    let db = database.db();
    // The first versions of these tables, before Python's later ALTERs.
    sqlx::raw_sql(
        "CREATE TABLE jandan (id TEXT PRIMARY KEY, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE hanime1 (id TEXT PRIMARY KEY, created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);
         CREATE TABLE notifications (id BIGSERIAL PRIMARY KEY, source TEXT NOT NULL, title TEXT NOT NULL);
         INSERT INTO jandan (id) VALUES ('old');
         CREATE TABLE bilibili (bvid TEXT NOT NULL, fav_id BIGINT NOT NULL, title TEXT NOT NULL, upper TEXT NOT NULL,
                                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP);",
    )
    .execute(db.pool())
    .await
    .expect("create old tables");
    db.ensure_schema().await.expect("apply the schema over old tables");
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT table_name || '.' || column_name FROM information_schema.columns
         WHERE table_schema = 'public' AND table_name IN ('jandan', 'hanime1', 'notifications') ORDER BY 1",
    )
    .fetch_all(db.pool())
    .await
    .expect("columns");
    for wanted in [
        "jandan.failed_count",
        "jandan.last_error",
        "jandan.unavailable",
        "hanime1.release_date",
        "hanime1.plot",
        "hanime1.title",
        "hanime1.uploader",
        "notifications.dedupe_key",
        "notifications.telegram_pinned_message_id",
        "notifications.scope",
    ] {
        assert!(columns.iter().any(|column| column == wanted), "{wanted} missing from {columns:?}");
    }
    let failed_count: i32 = sqlx::query_scalar("SELECT failed_count FROM jandan WHERE id = 'old'")
        .fetch_one(db.pool())
        .await
        .expect("row");
    assert_eq!(failed_count, 0);
    let constraint: Option<String> =
        sqlx::query_scalar("SELECT conname::text FROM pg_constraint WHERE conrelid = 'bilibili'::regclass AND contype = 'u'")
            .fetch_optional(db.pool())
            .await
            .expect("constraint");
    assert_eq!(constraint.as_deref(), Some("bilibili_bvid_unique"));
    database.drop_database().await;
}

/// `(table, column)` for each `ALTER TABLE t ADD COLUMN IF NOT EXISTS c`, and
/// `(table, columns)` for each `CREATE INDEX ... ON t (...)`, with their offsets.
fn added_columns(script: &str) -> Vec<(usize, String, String)> {
    let marker = "ALTER TABLE ";
    script
        .match_indices(marker)
        .filter_map(|(offset, _)| {
            let rest = &script[offset + marker.len()..];
            let mut words = rest.split_whitespace();
            let table = words.next()?;
            let (add, column, if_, not, exists) = (words.next()?, words.next()?, words.next()?, words.next()?, words.next()?);
            (add.eq_ignore_ascii_case("ADD") && column.eq_ignore_ascii_case("COLUMN") && (if_, not, exists) == ("IF", "NOT", "EXISTS"))
                .then(|| (offset, table.to_owned(), words.next().unwrap_or_default().to_owned()))
        })
        .collect()
}

fn created_indexes(script: &str) -> Vec<(usize, String, String)> {
    // Python's regex only sees `CREATE INDEX`; unique indexes are checked too.
    let mut starts: Vec<usize> = ["CREATE INDEX", "CREATE UNIQUE INDEX"]
        .iter()
        .flat_map(|marker| script.match_indices(marker).map(|(offset, _)| offset))
        .collect();
    starts.sort_unstable();
    starts
        .into_iter()
        .filter_map(|offset| {
            let statement = script[offset..offset + script[offset..].find(';')?].replace('\n', " ");
            let on = statement.find(" ON ")?;
            let after = statement[on + 4..].trim_start();
            let open = after.find('(')?;
            let table = after[..open].trim().to_owned();
            let close = after.find(')')?;
            Some((offset, table, after[open + 1..close].to_owned()))
        })
        .collect()
}

fn names_column(columns: &str, name: &str) -> bool {
    columns
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|word| word == name)
}

// py: tests/test_database.py::test_schema_scripts_add_columns_before_indexing_them
#[test]
fn schema_script_adds_columns_before_indexing_them() {
    // Statement order is the only migration there is: on an existing database a column
    // added later arrives through its ALTER alone, so an index naming it must come after.
    let mut offenders = Vec::new();
    let alters = added_columns(SCHEMA_SQL);
    assert!(alters.len() > 40, "{alters:?}");
    let indexes = created_indexes(SCHEMA_SQL);
    assert_eq!(indexes.len(), 13, "{indexes:?}");
    assert!(
        indexes
            .iter()
            .any(|(_, table, columns)| table == "notifications" && columns == "dedupe_key")
    );
    for (alter_at, table, column) in &alters {
        for (index_at, index_table, columns) in &indexes {
            if index_table == table && index_at < alter_at && names_column(columns, column) {
                offenders.push(format!("index on {table} ({}) precedes ADD COLUMN {column}", columns.trim()));
            }
        }
    }
    assert_eq!(offenders, Vec::<String>::new());
}

#[test]
fn every_statement_is_idempotent() {
    let statements = split_statements(SCHEMA_SQL);
    assert_eq!(statements.len(), 43 + 41 + 13, "tables, added columns, indexes");
    for statement in statements {
        let code: Vec<&str> = statement
            .lines()
            .filter(|line| !line.trim().is_empty() && !line.trim_start().starts_with("--"))
            .collect();
        let first = code[0];
        let ok = first.starts_with("CREATE TABLE IF NOT EXISTS ")
            || first.starts_with("CREATE INDEX IF NOT EXISTS ")
            || first.starts_with("CREATE UNIQUE INDEX IF NOT EXISTS ")
            || first.starts_with("ALTER TABLE ") && first.contains(" ADD COLUMN IF NOT EXISTS ");
        assert!(ok, "not idempotent: {statement}");
        assert!(statement.ends_with(';'), "{statement}");
    }
}

#[test]
fn statements_split_outside_quotes_and_comments() {
    let script = "-- a; comment\nCREATE TABLE a (x TEXT DEFAULT ';', \"we;ird\" INT);\n/* b; */ SELECT 'it''s; fine';\n-- trailing only\n";
    assert_eq!(
        split_statements(script),
        [
            "-- a; comment\nCREATE TABLE a (x TEXT DEFAULT ';', \"we;ird\" INT);",
            "/* b; */ SELECT 'it''s; fine';"
        ]
    );
}

#[tokio::test]
async fn a_failing_statement_does_not_stop_the_others() {
    let database = TestDatabase::create().await;
    let db = database.db();
    // A view where a table belongs: its CREATE TABLE IF NOT EXISTS is skipped and its ALTERs fail.
    sqlx::raw_sql("CREATE VIEW jandan AS SELECT 'x'::text AS id;")
        .execute(db.pool())
        .await
        .expect("view");
    let Err(DbError::Schema(failures)) = db.ensure_schema().await else {
        panic!("expected schema failures")
    };
    assert_eq!(failures.len(), 3, "{failures:#?}");
    assert!(
        failures
            .iter()
            .all(|failure| failure.starts_with("ALTER TABLE jandan ADD COLUMN IF NOT EXISTS")),
        "{failures:#?}"
    );
    let have = describe(db).await;
    let want: Vec<String> = python_schema()
        .into_iter()
        .filter(|line| {
            line.split_whitespace()
                .nth(1)
                .is_none_or(|object| object != "jandan" && !object.starts_with("jandan."))
        })
        .collect();
    let missing: Vec<&String> = want.iter().filter(|line| !have.contains(line)).collect();
    assert!(missing.is_empty(), "{missing:#?}");
    database.drop_database().await;
}
