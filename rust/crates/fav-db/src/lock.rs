//! Session advisory locks, keyed exactly like Python.
//!
//! Python origin: `advisory_lock_id` and `advisory_lock` in `src/tool/database.py`. The
//! Python worker and the Rust one must exclude each other during a cut-over, so the key is
//! the same blake2b-64 digest of the name, read as a signed big-endian integer.

use blake2::digest::consts::U8;
use blake2::{Blake2b, Digest};
use fav_core::logging;
use sqlx::PgConnection;

use crate::{Db, DbError};

/// The lock key for `name`: blake2b with an 8-byte digest, as a signed big-endian `i64`.
pub fn advisory_lock_id(name: &str) -> i64 {
    let digest = Blake2b::<U8>::digest(name.as_bytes());
    i64::from_be_bytes(digest.into())
}

/// A held session lock. It lives on its own connection, because it is held across a whole
/// job run and would otherwise starve the pool. Dropping it without [`release`] closes
/// that connection, which also makes the server release the lock.
///
/// [`release`]: AdvisoryLock::release
#[derive(Debug)]
pub struct AdvisoryLock {
    name: String,
    id: i64,
    connection: PgConnection,
}

impl Db {
    /// `pg_try_advisory_lock` on a dedicated connection: `None` if another session holds it.
    pub async fn try_advisory_lock(&self, name: &str) -> Result<Option<AdvisoryLock>, DbError> {
        let id = advisory_lock_id(name);
        let mut connection = self.dedicated_connection().await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1) AS acquired;")
            .bind(id)
            .fetch_one(&mut connection)
            .await?;
        if acquired {
            Ok(Some(AdvisoryLock {
                name: name.to_owned(),
                id,
                connection,
            }))
        } else {
            close_quietly(connection).await;
            Ok(None)
        }
    }
}

impl AdvisoryLock {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn id(&self) -> i64 {
        self.id
    }

    /// Unlock and close the connection. A failed unlock is logged, as in Python; closing
    /// the connection releases the lock anyway.
    pub async fn release(mut self) {
        let unlocked = sqlx::query("SELECT pg_advisory_unlock($1);")
            .bind(self.id)
            .execute(&mut self.connection)
            .await;
        if let Err(error) = unlocked {
            logging::get("database").warning(format_args!("Failed to release advisory lock {}: {error}", self.name));
        }
        close_quietly(self.connection).await;
    }
}

async fn close_quietly(connection: PgConnection) {
    use sqlx::Connection as _;
    let _ = connection.close().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testdb;
    use fav_core::golden;

    #[test]
    fn ids_match_python() {
        let document = golden::load("db/advisory_lock_id.json");
        for case in golden::cases(&document) {
            let name = golden::string(golden::get(case, "name"));
            assert_eq!(i128::from(advisory_lock_id(&name)), golden::int(golden::get(case, "id")), "{name}");
        }
    }

    // py: tests/test_database.py::test_advisory_lock_id_is_stable_signed_bigint
    #[test]
    fn advisory_lock_id_is_stable_signed_bigint() {
        let lock_id = advisory_lock_id("telegram:default:data/session");
        assert_eq!(lock_id, advisory_lock_id("telegram:default:data/session"));
        assert_ne!(lock_id, advisory_lock_id("telegram:other:data/session"));
        // An i64 is a signed bigint by construction; the golden pins the exact values.
    }

    #[tokio::test]
    async fn a_held_lock_excludes_other_sessions_until_released() {
        let database = testdb::TestDatabase::create().await;
        let db = database.db();
        let first = db.try_advisory_lock("fav-test:lock").await.expect("query").expect("free");
        assert!(db.try_advisory_lock("fav-test:lock").await.expect("query").is_none());
        assert!(db.try_advisory_lock("fav-test:other").await.expect("query").is_some());
        first.release().await;
        let again = db.try_advisory_lock("fav-test:lock").await.expect("query").expect("free again");
        // Dropping closes the connection, which releases the lock as well.
        drop(again);
        let mut attempts = 0;
        while db.try_advisory_lock("fav-test:lock").await.expect("query").is_none() {
            attempts += 1;
            assert!(attempts < 50, "dropped lock was never released");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        database.drop_database().await;
    }
}
