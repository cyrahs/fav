//! The shared connection pool.
//!
//! Python origin: `_get_pool`/`connection` in `src/tool/database.py`: up to 16 autocommit
//! connections, at least one kept open, 15 s to get one, each checked before use.
//!
//! sqlx sets every session's `TimeZone` to UTC, where psycopg keeps the server's setting.
//! fav's server runs with `TZ=UTC`, so `CURRENT_TIMESTAMP` defaults and returned times
//! agree (DIVERGENCES.md D11).

use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{ConnectOptions as _, PgConnection, PgPool};

use crate::DbError;

/// Headroom for the archival crawlers, which issue several short statements per asset
/// from many tasks at once; the server allows 100 connections and this pool exists once
/// per process.
pub const POOL_MAX_SIZE: u32 = 16;
pub const POOL_MIN_SIZE: u32 = 1;
pub const POOL_TIMEOUT: Duration = Duration::from_secs(15);

/// A handle on fav's database: the shared pool plus what is needed to open dedicated
/// connections (advisory locks). Cheap to clone.
#[derive(Clone)]
pub struct Db {
    pool: PgPool,
    options: PgConnectOptions,
}

/// The DSN carries a password, so it stays out of debug output.
impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").field("pool_size", &self.pool.size()).finish_non_exhaustive()
    }
}

impl Db {
    /// A pool for `dsn` (`POSTGRES_DSN`); connections are opened when first needed.
    pub fn connect_lazy(dsn: &str) -> Result<Self, DbError> {
        Ok(Self::from_options(crate::dsn::parse_dsn(dsn)?))
    }

    /// A pool for already parsed connect options.
    pub fn from_options(options: PgConnectOptions) -> Self {
        let pool = PgPoolOptions::new()
            .max_connections(POOL_MAX_SIZE)
            .min_connections(POOL_MIN_SIZE)
            .acquire_timeout(POOL_TIMEOUT)
            .test_before_acquire(true)
            .connect_lazy_with(options.clone());
        Self { pool, options }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// A connection outside the pool, for work that holds one for a long time.
    pub async fn dedicated_connection(&self) -> Result<PgConnection, DbError> {
        Ok(self.options.connect().await?)
    }

    /// Close the pool; call once at shutdown.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}
