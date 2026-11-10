//! The `app_settings` table: read, cached, pinned for tests, written one section at a time.
//!
//! Python origin: the second half of `src/core/settings.py` (`load`, `use`,
//! `invalidate_cache`, `notify_enabled`, `load_section`, `save_section`,
//! `settings_version_sync`). Rows are `section TEXT, value JSONB`; values are read as text
//! and parsed with fav-core's Python-compatible reader, so `1` and `1.0` stay apart, and
//! written as `json.dumps(value, ensure_ascii=False)` would write them. The table itself is
//! created by [`Db::ensure_schema`]; the `CookieCloud` and tikhub start-up migrations are
//! not ported (DIVERGENCES.md).

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use fav_core::pyjson::{self, DumpOptions, Value};
use fav_core::settings::{self as models, SectionError, SectionValue, Settings};
use fav_core::validate::{Model as _, ValidationError};

use crate::{Db, DbError};

/// How long a loaded snapshot is reused: one job run does not re-query per field access,
/// and UI edits still apply within a couple of seconds.
pub const CACHE_TTL: Duration = Duration::from_secs(2);

/// Errors from the settings store.
#[derive(Debug, thiserror::Error)]
pub enum SettingsStoreError {
    #[error(transparent)]
    Db(#[from] DbError),
    /// An unknown section, or a payload that does not validate.
    #[error(transparent)]
    Section(#[from] SectionError),
    /// The stored rows do not validate as a whole.
    #[error(transparent)]
    Stored(#[from] ValidationError),
}

impl From<sqlx::Error> for SettingsStoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Db(DbError::Sqlx(error))
    }
}

/// The settings snapshot source shared by the worker and the API.
#[derive(Debug)]
pub struct SettingsStore {
    db: Option<Db>,
    ttl: Duration,
    cache: Mutex<Option<(Instant, Arc<Settings>)>>,
    pinned: Mutex<Option<Arc<Settings>>>,
}

impl SettingsStore {
    pub fn new(db: Db) -> Self {
        Self::with_ttl(Some(db), CACHE_TTL)
    }

    /// A store that always returns `settings` and has no database (`settings.use(...)` in
    /// Python's tests). Calls that need the table fail with [`DbError::NoDatabase`].
    pub fn pinned(settings: Settings) -> Self {
        let store = Self::with_ttl(None, CACHE_TTL);
        store.pin(Some(settings));
        store
    }

    pub fn with_ttl(db: Option<Db>, ttl: Duration) -> Self {
        Self {
            db,
            ttl,
            cache: Mutex::new(None),
            pinned: Mutex::new(None),
        }
    }

    /// Pin a snapshot and bypass the database for [`load`](Self::load); `None` restores
    /// database-backed loading.
    pub fn pin(&self, settings: Option<Settings>) {
        *self.pinned.lock().unwrap_or_else(PoisonError::into_inner) = settings.map(Arc::new);
        self.invalidate_cache();
    }

    pub fn invalidate_cache(&self) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn db(&self) -> Result<&Db, DbError> {
        self.db.as_ref().ok_or(DbError::NoDatabase)
    }

    /// The current snapshot, from the cache while it is younger than the TTL.
    pub async fn load(&self) -> Result<Arc<Settings>, SettingsStoreError> {
        self.load_inner(false).await
    }

    /// The current snapshot, read from the table even if a cached one is fresh.
    pub async fn load_force(&self) -> Result<Arc<Settings>, SettingsStoreError> {
        self.load_inner(true).await
    }

    async fn load_inner(&self, force: bool) -> Result<Arc<Settings>, SettingsStoreError> {
        if let Some(pinned) = self.pinned.lock().unwrap_or_else(PoisonError::into_inner).clone() {
            return Ok(pinned);
        }
        if !force
            && let Some((loaded_at, settings)) = self.cache.lock().unwrap_or_else(PoisonError::into_inner).clone()
            && loaded_at.elapsed() < self.ttl
        {
            return Ok(settings);
        }
        let rows = self.fetch_rows().await?;
        let settings = Arc::new(models::build_settings(
            rows.iter().map(|(section, value)| (section.as_str(), value)),
        )?);
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = Some((Instant::now(), Arc::clone(&settings)));
        Ok(settings)
    }

    async fn fetch_rows(&self) -> Result<Vec<(String, Value)>, DbError> {
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT section, value::text FROM app_settings;")
            .fetch_all(self.db()?.pool())
            .await?;
        rows.into_iter()
            .map(|(section, text)| match pyjson::loads(&text) {
                Ok(value) => Ok((section, value)),
                Err(source) => Err(DbError::StoredJson {
                    what: format!("app_settings section {section}"),
                    source,
                }),
            })
            .collect()
    }

    /// Whether `source`'s notifications should be queued at all (`notify_enabled`).
    pub async fn notify_enabled(&self, source: &str) -> Result<bool, SettingsStoreError> {
        let settings = self.load().await?;
        Ok(models::notify_enabled(&settings, source))
    }

    /// One section as stored, validated (`load_section`); a missing row gives its defaults.
    pub async fn load_section(&self, section: &str) -> Result<SectionValue, SettingsStoreError> {
        if !models::SECTIONS.contains(&section) {
            return Err(SectionError::Unknown(models::UnknownSectionError {
                section: section.to_owned(),
            })
            .into());
        }
        let rows = self.fetch_rows().await?;
        let empty = Value::Dict(pyjson::Dict::new());
        let value = rows.iter().find(|(name, _)| name == section).map_or(&empty, |(_, value)| value);
        Ok(SectionValue::validate(section, value)?)
    }

    /// Validate `payload` as `section` and replace the stored row (`save_section`).
    pub async fn save_section(&self, section: &str, payload: &Value) -> Result<SectionValue, SettingsStoreError> {
        let validated = SectionValue::validate(section, payload)?;
        let text = pyjson::dumps(&validated.dump(), &DumpOptions::plain()).map_err(|error| DbError::Invalid(error.to_string()))?;
        sqlx::query(
            "INSERT INTO app_settings (section, value, updated_at)
             VALUES ($1, $2::jsonb, CURRENT_TIMESTAMP)
             ON CONFLICT (section) DO UPDATE
             SET value = EXCLUDED.value,
                 updated_at = CURRENT_TIMESTAMP;",
        )
        .bind(section)
        .bind(text)
        .execute(self.db()?.pool())
        .await?;
        self.invalidate_cache();
        Ok(validated)
    }

    /// The latest `updated_at` as Python's `isoformat()` (UTC), or `""` for an empty table.
    pub async fn settings_version(&self) -> Result<String, SettingsStoreError> {
        let version: Option<DateTime<Utc>> = sqlx::query_scalar("SELECT MAX(updated_at) AS version FROM app_settings;")
            .fetch_one(self.db()?.pool())
            .await?;
        Ok(version
            .map(|time| pyjson::py_isoformat_datetime(&time.fixed_offset()))
            .unwrap_or_default())
    }
}

/// The defaults-only snapshot (`Settings()`), for pinning in tests.
pub fn default_settings() -> Settings {
    Settings::validate(&Value::Dict(pyjson::Dict::new())).expect("the defaults validate")
}

#[cfg(test)]
mod tests;
