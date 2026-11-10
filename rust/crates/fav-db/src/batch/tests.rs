use super::*;
use crate::testdb::TestDatabase;

fn text(value: &str) -> Param {
    Param::from(value)
}

fn select_statements(count: usize) -> Vec<Statement> {
    (0..count)
        .map(|index| Statement::new(format!("SELECT $1::text AS c{index}"), vec![text(&index.to_string())]))
        .collect()
}

// py: tests/test_database.py::test_insert_db_batch_respects_max_bind_params
#[test]
fn insert_statements_respect_max_bind_params() {
    let max_bind_params = 7;
    let rows: Vec<Vec<Param>> = (0..10)
        .map(|index: i32| {
            vec![
                text(&index.to_string()),
                text(&(index + 1).to_string()),
                text(&(index + 2).to_string()),
            ]
        })
        .collect();
    let statements = insert_statements("demo", &["a", "b", "c"], rows, None, 10, max_bind_params).expect("valid");
    assert_eq!(statements.len(), 5);
    assert_eq!(
        statements.iter().map(|statement| statement.params.len()).collect::<Vec<_>>(),
        [6; 5]
    );
    assert!(
        statements
            .iter()
            .all(|statement| statement.sql.matches('$').count() <= max_bind_params)
    );
    assert_eq!(statements[0].sql, "INSERT INTO demo (a, b, c) VALUES ($1, $2, $3), ($4, $5, $6);");
    assert_eq!(statements[4].params[0], text("8"));
}

#[test]
fn insert_statements_reject_what_python_rejects() {
    let message = |result: Result<Vec<Statement>, DbError>| result.expect_err("rejected").to_string();
    assert!(
        insert_statements("demo", &["a"], Vec::new(), None, 10, 7)
            .expect("no rows")
            .is_empty()
    );
    assert_eq!(
        message(insert_statements("demo", &[], vec![vec![text("x")]], None, 10, 7)),
        "columns cannot be empty"
    );
    assert_eq!(
        message(insert_statements("demo", &["a"], vec![vec![text("x")]], None, 0, 7)),
        "chunk_size must be greater than 0"
    );
    assert_eq!(
        message(insert_statements("demo", &["a", "b"], vec![vec![text("x")]], None, 10, 7)),
        "all rows must have the same length as columns"
    );
    assert_eq!(
        message(insert_statements(
            "demo",
            &["a", "b", "c"],
            vec![vec![text("x"), text("y"), text("z")]],
            None,
            10,
            2
        )),
        "columns count (3) exceeds max_bind_params=2; cannot build insert statement"
    );
    let with_conflict = insert_statements("demo", &["a"], vec![vec![text("x")]], Some("(a) DO NOTHING"), 10, 7).expect("valid");
    assert_eq!(with_conflict[0].sql, "INSERT INTO demo (a) VALUES ($1) ON CONFLICT (a) DO NOTHING;");
}

// py: tests/test_database.py::test_query_db_batch_chunks_by_max_bind_params
#[tokio::test]
async fn query_batch_chunks_by_max_bind_params() {
    let mut calls = Vec::new();
    let results = query_batch_with(select_statements(5), 50, 2, async |chunk: Vec<Statement>| {
        calls.push(chunk.len());
        Ok(chunk.iter().map(|_| ()).collect())
    })
    .await
    .expect("valid");
    assert_eq!(results.len(), 5);
    assert_eq!(calls, [2, 2, 1]);
}

// py: tests/test_database.py::test_query_db_batch_rejects_statement_with_too_many_bind_params
#[tokio::test]
async fn query_batch_rejects_statement_with_too_many_bind_params() {
    let statements = vec![Statement::new("SELECT $1, $2, $3;", vec![text("1"), text("2"), text("3")])];
    let error = query_batch_with(statements, 50, 2, async |chunk: Vec<Statement>| {
        Ok(chunk.iter().map(|_| ()).collect())
    })
    .await
    .expect_err("rejected");
    assert!(error.to_string().contains("exceeds max_bind_params"), "{error}");
    assert_eq!(error.to_string(), "Statement has 3 bind params, exceeds max_bind_params=2");
}

// py: tests/test_database.py::test_query_db_batch_propagates_result_count_mismatch
#[tokio::test]
async fn query_batch_propagates_result_count_mismatch() {
    let error = query_batch_with(select_statements(5), 50, 2, async |chunk: Vec<Statement>| {
        Ok(if chunk.len() == 2 {
            vec![()]
        } else {
            chunk.iter().map(|_| ()).collect()
        })
    })
    .await
    .expect_err("mismatch");
    assert!(error.to_string().contains("batch result mismatch"), "{error}");
}

#[tokio::test]
async fn query_batch_checks_its_arguments_and_skips_blank_statements() {
    let run = async |chunk: Vec<Statement>| Ok(chunk.iter().map(|_| ()).collect::<Vec<()>>());
    let zero_chunk = query_batch_with(select_statements(1), 0, 2, run).await.expect_err("rejected");
    assert_eq!(zero_chunk.to_string(), "chunk_size must be greater than 0");
    let zero_params = query_batch_with(select_statements(1), 1, 0, run).await.expect_err("rejected");
    assert_eq!(zero_params.to_string(), "max_bind_params must be greater than 0");
    let mut seen = Vec::new();
    let statements = vec![Statement::new("  ", Vec::new()), Statement::new(" SELECT 1; ", Vec::new())];
    query_batch_with(statements, 50, 2, async |chunk: Vec<Statement>| {
        seen.extend(chunk.iter().map(|statement| statement.sql.clone()));
        Ok(chunk.iter().map(|_| ()).collect())
    })
    .await
    .expect("valid");
    assert_eq!(seen, ["SELECT 1"]);
}

#[tokio::test]
async fn batches_run_against_postgres() {
    let database = TestDatabase::create().await;
    let db = database.db();
    sqlx::raw_sql("CREATE TABLE demo (a TEXT PRIMARY KEY, b BIGINT, c TIMESTAMPTZ);")
        .execute(db.pool())
        .await
        .expect("create");
    let rows: Vec<Vec<Param>> = (0..450_i64)
        .map(|index| vec![Param::from(format!("k{index}")), Param::from(index), Param::Timestamp(None)])
        .collect();
    db.insert_batch("demo", &["a", "b", "c"], rows, None, DEFAULT_INSERT_CHUNK_SIZE)
        .await
        .expect("insert");
    let again = vec![vec![Param::from("k1"), Param::Int(None), Param::Timestamp(None)]];
    db.insert_batch("demo", &["a", "b", "c"], again, Some("(a) DO NOTHING"), DEFAULT_INSERT_CHUNK_SIZE)
        .await
        .expect("insert ignored");
    let results = db
        .query_batch(
            vec![
                Statement::new("SELECT count(*)::text AS n FROM demo", Vec::new()),
                Statement::new("SELECT b::text AS n FROM demo WHERE a = $1", vec![Param::from("k1")]),
                Statement::new("UPDATE demo SET b = NULL WHERE a = $1", vec![Param::from("k2")]),
            ],
            DEFAULT_QUERY_CHUNK_SIZE,
            DEFAULT_MAX_BIND_PARAMS,
        )
        .await
        .expect("batch");
    assert_eq!(results.len(), 3);
    assert_eq!(text_column(&results[0][0], "n").expect("column"), Some("450".to_owned()));
    assert_eq!(text_column(&results[1][0], "n").expect("column"), Some("1".to_owned()));
    assert!(results[2].is_empty());
    let failed = db
        .query_transaction(vec![
            Statement::new("DELETE FROM demo", Vec::new()),
            Statement::new("SELECT 1/0", Vec::new()),
        ])
        .await;
    assert!(failed.is_err());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM demo")
        .fetch_one(db.pool())
        .await
        .expect("count");
    assert_eq!(count, 450, "the failed transaction rolled back");
    database.drop_database().await;
}
