//! The fav content sources, one module per entry of `JOB_SPECS`.
//!
//! Scope (filled in by WP5 to WP17, see `rust/docs/PLAN.md`). Every source
//! implements the runtime's `Source` trait and receives its HTTP client,
//! database, browser, Telegram client and subprocess runner by injection.
//!
//! Ported so far: [`bilibili`] and [`twitter`] (WP7); [`stellasora`] and [`pixiv`] (WP6); [`jandan`] and [`kemono`] (with
//! [`kemono_creator`], which the API's creator-resolve route uses), WP5; [`hanime1`] with
//! [`hanime1_series`], [`hanime1_author`] and [`runtime_config`] (the API's seed, author and
//! readiness routes use the first two) and the t2s conversion [`opencc`], WP8; [`nikke`] with
//! [`nikke_layer_metadata`] and [`content_discovery`], WP9; [`bd2`] with the
//! `bd2-l2d-viewer` command's [`bd2_l2d_viewer`], WP10.

use std::sync::Arc;

use fav_db::{Db, SettingsStore};
use fav_runtime::source::{JobError, Registry};

pub mod bd2;
pub mod bd2_l2d_viewer;
pub mod bilibili;
pub mod content_discovery;
pub mod hanime1;
pub mod hanime1_author;
#[cfg(test)]
mod hanime1_golden;
pub mod hanime1_series;
pub mod jandan;
pub mod kemono;
pub mod kemono_creator;
mod kit;
pub mod nikke;
pub mod nikke_layer_metadata;
pub mod opencc;
pub mod pixiv;
mod pydecode;
mod pyerr;
mod pyre;
pub mod runtime_config;
pub mod stellasora;
#[cfg(test)]
mod testkit;
#[cfg(test)]
mod testutil;
pub mod twitter;

pub use kit::{HttpError, Notifier, OutboxNotifier};
pub use pydecode::PyError;

/// What every source's factory may use: the database and the settings store. Each run
/// reads its own section of the settings when it starts, as Python's constructors do.
#[derive(Debug, Clone)]
pub struct SourceContext {
    pub db: Db,
    pub settings: Arc<SettingsStore>,
}

/// The sources that have a Rust implementation, for the worker.
///
/// Each source's work package registers it here. A job without an entry has no runner:
/// the worker does not schedule it and refuses to trigger it (DIVERGENCES.md D17).
pub fn registry(ctx: &SourceContext) -> Registry {
    let mut registry = Registry::new();
    bilibili::register(&mut registry, ctx);
    stellasora::register(&mut registry, ctx);
    pixiv::register(&mut registry, ctx);
    twitter::register(&mut registry, ctx);
    jandan::register(&mut registry, ctx);
    kemono::register(&mut registry, ctx);
    hanime1::register(&mut registry, ctx);
    nikke::register(&mut registry, ctx);
    bd2::register(&mut registry, ctx);
    registry
}

/// A database failure as the job error the runner reports.
pub(crate) fn db_job_error(error: &fav_db::DbError) -> JobError {
    pyerr::db_error(error)
}

/// One statement on a pooled connection; its rows, or the error as the job reports it.
pub(crate) async fn execute(db: &fav_db::Db, sql: &str, params: Vec<fav_db::Param>) -> Result<Vec<sqlx::postgres::PgRow>, JobError> {
    let mut rows = db
        .query_batch(
            vec![fav_db::Statement::new(sql, params)],
            fav_db::batch::DEFAULT_QUERY_CHUNK_SIZE,
            fav_db::batch::DEFAULT_MAX_BIND_PARAMS,
        )
        .await
        .map_err(|error| db_job_error(&error))?;
    Ok(rows.pop().unwrap_or_default())
}

/// A settings-store failure as the job error the runner reports.
pub(crate) fn settings_job_error(error: &fav_db::settings_store::SettingsStoreError) -> JobError {
    JobError::new("RuntimeError", error.to_string())
}
