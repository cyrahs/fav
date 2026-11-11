//! Temporary databases for tests (rust/docs/CONVENTIONS.md, "Tests").
//!
//! Each test that needs PostgreSQL creates its own database through the server named by
//! `FAV_TEST_DATABASE_URL` and drops it at the end. A test that needs one and does not
//! find the variable fails; it never skips. Nothing here may point at a real fav database.

use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, ConnectOptions as _, Connection as _, PgConnection};

use crate::Db;

pub const URL_VARIABLE: &str = "FAV_TEST_DATABASE_URL";

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A database created for one test.
#[derive(Debug)]
pub struct TestDatabase {
    name: String,
    admin: PgConnectOptions,
    db: Db,
}

impl TestDatabase {
    /// Create an empty database named `fav_test_<pid>_<n>_<nanos>`.
    pub async fn create() -> Self {
        let url = std::env::var(URL_VARIABLE).unwrap_or_else(|_| {
            panic!("{URL_VARIABLE} is not set; database tests need a disposable PostgreSQL server (see rust/docs/CONVENTIONS.md)")
        });
        let admin = PgConnectOptions::from_str(&url).unwrap_or_else(|error| panic!("{URL_VARIABLE} is not a PostgreSQL URL: {error}"));
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.subsec_nanos());
        let name = format!(
            "fav_test_{}_{}_{nanos}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let mut connection = admin
            .connect()
            .await
            .unwrap_or_else(|error| panic!("cannot reach {URL_VARIABLE}: {error}"));
        // The name is generated above from digits only.
        sqlx::raw_sql(AssertSqlSafe(format!("CREATE DATABASE \"{name}\";")))
            .execute(&mut connection)
            .await
            .expect("create the test database");
        let _ = connection.close().await;
        let options = admin.clone().database(&name);
        let db = Db::from_options(options);
        Self { name, admin, db }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Close the pool and drop the database.
    pub async fn drop_database(self) {
        self.db.close().await;
        let mut connection: PgConnection = self.admin.connect().await.expect("reconnect to drop the test database");
        sqlx::raw_sql(AssertSqlSafe(format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE);", self.name)))
            .execute(&mut connection)
            .await
            .expect("drop the test database");
        let _ = connection.close().await;
    }
}
