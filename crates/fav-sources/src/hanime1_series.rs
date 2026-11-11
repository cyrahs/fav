//! Hanime1 series seeds: the service the API adds, lists and deletes `hanime1_series`
//! rows with, and the watch-page probe behind `/readyz`.
//!
//! Python origin: `src/tool/hanime1_series.py`. Adding a seed resolves it on the watch
//! page (the playlist's title and member ids), stores the series under its smallest id
//! and claims every member in `hanime1_series_video`, in one transaction. The service
//! reads `web.hanime1.host` and `user_lang` on every call, where Python's API read them
//! once at startup (DIVERGENCES.md D78).

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use fav_core::pytext::{py_isdecimal, py_strip};
use fav_core::validate::LiteralStr;
use fav_db::{Db, SettingsStore};
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::source::JobError;
use sqlx::Row;

use crate::opencc::to_simplified_chinese;
use crate::pyerr;
use crate::pyre::int_text;
use crate::runtime_config::{
    RuntimeSeriesSeed, build_hanime1_seed, extract_hanime1_seed_id, is_parser_incompatible, normalize_hanime1_seed, parse_hanime1_playlist,
    parser_incompatible, select_hanime1_canonical_id,
};

/// The browser `User-Agent` the API's Hanime1 requests send.
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";
/// A page holding every marker is Cloudflare's block page.
pub const CF_BLOCK_MARKERS: [&str; 2] = ["Attention Required! | Cloudflare", "cf-error-details"];
const ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";

/// Why a seed or author call failed, as the Python exception classes the API maps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// `ValueError`: the input names no seed or author.
    Invalid,
    /// `FileExistsError`: already subscribed.
    Duplicate,
    /// `LookupError`: the site gave no usable answer.
    ResolveFailed,
    /// Anything else (the database, the settings); the API logs it and answers 500.
    Other(String),
}

/// Why a page fetch failed.
#[derive(Debug, Clone)]
pub enum PageError {
    /// A transport error or a non-2xx status.
    Http(JobError),
    /// Cloudflare's block page.
    Blocked,
}

impl PageError {
    /// The exception Python raises for it.
    pub fn into_job_error(self) -> JobError {
        match self {
            Self::Http(error) => error,
            Self::Blocked => JobError::new("RuntimeError", "Hanime1 watch page was blocked by Cloudflare"),
        }
    }
}

pub(crate) fn value_error(message: impl Into<String>) -> JobError {
    JobError::new("ValueError", message)
}

/// `httpx.Client(timeout=40)`, following redirects or not.
pub(crate) fn client_options(follow_redirects: bool) -> ClientOptions {
    ClientOptions {
        timeout: Duration::from_secs(40),
        follow_redirects,
        ..ClientOptions::default()
    }
}

/// Whether a page is Cloudflare's block page.
pub fn is_cf_blocked(page: &str) -> bool {
    CF_BLOCK_MARKERS.iter().all(|marker| page.contains(marker))
}

/// httpx's `response.text` for a page without a usable charset: UTF-8 with replacement.
pub(crate) fn page_text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// GET a page with the browser headers and `user_lang`; its text unless the status is not
/// 2xx or it is the block page.
pub(crate) async fn fetch_page(http: &HttpClient, url: &str, host: &str, user_lang: &str) -> Result<String, PageError> {
    let request = Request::get(url)
        .header("Referer", format!("{host}/"))
        .header("User-Agent", USER_AGENT)
        .header("Accept", ACCEPT)
        .header("Accept-Language", ACCEPT_LANGUAGE)
        .header("Cookie", format!("user_lang={user_lang}"));
    let response = http
        .send(request)
        .await
        .map_err(|error| PageError::Http(pyerr::transport_error(&error)))?;
    response
        .error_for_status()
        .map_err(|error| PageError::Http(pyerr::status_error(&error)))?;
    let body = response
        .bytes()
        .await
        .map_err(|error| PageError::Http(pyerr::transport_error(&error)))?;
    let page = page_text(&body);
    if is_cf_blocked(&page) {
        return Err(PageError::Blocked);
    }
    Ok(page)
}

/// `{host}/watch?v={id}`.
pub fn watch_url(host: &str, video_id: &str) -> String {
    format!("{}/watch?v={video_id}", host.trim_end_matches('/'))
}

/// One row of `list_seeds`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeedDetail {
    pub video_id: String,
    pub title: String,
    pub label: String,
    pub added_from_video_id: String,
    pub video_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub last_scan_error: String,
    pub watch_url: String,
}

/// Why a readiness probe failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeError {
    /// The probe ran past its time (`TimeoutError`).
    Timeout,
    /// `Hanime1ParserIncompatibleError`.
    ParserIncompatible,
    /// Anything else: the exception's `Class: message`.
    Other(String),
}

/// What the API calls; tests put a fake behind it.
pub trait SeriesSubscriptions: Send + Sync {
    fn add<'a>(&'a self, raw_seed: &'a str) -> BoxFuture<'a, Result<RuntimeSeriesSeed, ServiceError>>;
    fn list(&self) -> BoxFuture<'_, Result<Vec<SeedDetail>, ServiceError>>;
    fn delete<'a>(&'a self, canonical_video_id: &'a str) -> BoxFuture<'a, Result<bool, ServiceError>>;
    /// `readiness_seed_ids(limit=...)`: the most recently scanned series first.
    fn readiness_seed_ids(&self, limit: i64) -> BoxFuture<'_, Result<Vec<String>, String>>;
    /// `probe_seed(seed_id, timeout_seconds=...)`.
    fn probe_seed<'a>(&'a self, seed_id: &'a str, timeout: Duration) -> BoxFuture<'a, Result<(), ProbeError>>;
}

/// `Hanime1SeriesService`.
pub struct Hanime1SeriesService {
    db: Db,
    settings: Arc<SettingsStore>,
    http: HttpClient,
}

impl std::fmt::Debug for Hanime1SeriesService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hanime1SeriesService").finish_non_exhaustive()
    }
}

fn other(error: impl std::fmt::Display) -> ServiceError {
    ServiceError::Other(error.to_string())
}

impl Hanime1SeriesService {
    /// `httpx.Client(timeout=40)`, redirects not followed.
    pub fn new(db: Db, settings: Arc<SettingsStore>) -> Result<Self, String> {
        let http = HttpClient::new(&client_options(false)).map_err(|error| error.to_string())?;
        Ok(Self::with_client(db, settings, http))
    }

    pub fn with_client(db: Db, settings: Arc<SettingsStore>, http: HttpClient) -> Self {
        Self { db, settings, http }
    }

    async fn host_and_lang(&self) -> Result<(String, String), String> {
        let settings = self.settings.load().await.map_err(|error| error.to_string())?;
        let section = &settings.web.hanime1;
        Ok((section.host.trim_end_matches('/').to_owned(), section.user_lang.as_str().to_owned()))
    }

    /// `_fetch_watch_page_html(seed_id)`.
    async fn fetch_watch_page(&self, host: &str, user_lang: &str, seed_id: &str) -> Result<String, PageError> {
        fetch_page(&self.http, &watch_url(host, seed_id), host, user_lang).await
    }

    /// `add_seed(raw_seed)`: the stored series.
    pub async fn add_seed(&self, raw_seed: &str) -> Result<RuntimeSeriesSeed, ServiceError> {
        let Ok(Some(normalized)) = normalize_hanime1_seed(raw_seed, true) else {
            return Err(ServiceError::Invalid);
        };
        let Ok(Some(seed_id)) = extract_hanime1_seed_id(&normalized) else {
            return Err(ServiceError::Invalid);
        };
        let (host, user_lang) = self.host_and_lang().await.map_err(ServiceError::Other)?;
        // `_resolve_hanime1_series`: any fetch failure or an incompatible page is "unresolved".
        let resolved = match self.fetch_watch_page(&host, &user_lang, &seed_id).await {
            Ok(page) => match parse_hanime1_playlist(&page, Some(&seed_id), true) {
                Ok(resolved) => resolved,
                Err(error) if is_parser_incompatible(&error) => None,
                // A malformed link (`urlsplit`, `int()`) is Python's `ValueError`: 422.
                Err(error) if error.class_name() == "ValueError" => return Err(ServiceError::Invalid),
                Err(error) => return Err(ServiceError::Other(error.formatted())),
            },
            Err(_) => None,
        };
        let Some((title, series_ids)) = resolved else {
            return Err(ServiceError::ResolveFailed);
        };
        let Some(canonical_id) = select_hanime1_canonical_id(&series_ids) else {
            return Err(ServiceError::ResolveFailed);
        };
        let seed = RuntimeSeriesSeed {
            video_id: canonical_id,
            title: to_simplified_chinese(Some(&title)).unwrap_or(title),
        };
        self.insert_series_seed(&seed, &seed_id, &series_ids).await?;
        Ok(seed)
    }

    /// `_insert_series_seed`: refused when any member already belongs to a series or the
    /// canonical id is taken; otherwise the series and its members, in one transaction.
    async fn insert_series_seed(
        &self,
        seed: &RuntimeSeriesSeed,
        added_from_video_id: &str,
        series_ids: &[String],
    ) -> Result<(), ServiceError> {
        let mut transaction = self.db.pool().begin().await.map_err(other)?;
        let claimed =
            sqlx::query("SELECT canonical_video_id FROM hanime1_series_video WHERE video_id = ANY($1) ORDER BY canonical_video_id LIMIT 1")
                .bind(series_ids)
                .fetch_optional(&mut *transaction)
                .await
                .map_err(other)?;
        if claimed.is_some() {
            return Err(ServiceError::Duplicate);
        }
        let inserted = sqlx::query(
            "INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id) VALUES ($1, $2, $3) \
             ON CONFLICT (canonical_video_id) DO NOTHING",
        )
        .bind(&seed.video_id)
        .bind(&seed.title)
        .bind(added_from_video_id)
        .execute(&mut *transaction)
        .await
        .map_err(other)?;
        if inserted.rows_affected() == 0 {
            return Err(ServiceError::Duplicate);
        }
        for video_id in series_ids {
            sqlx::query(
                "INSERT INTO hanime1_series_video (video_id, canonical_video_id) VALUES ($1, $2) ON CONFLICT (video_id) DO NOTHING",
            )
            .bind(video_id)
            .bind(&seed.video_id)
            .execute(&mut *transaction)
            .await
            .map_err(other)?;
        }
        transaction.commit().await.map_err(other)
    }

    /// `list_seeds()`.
    pub async fn list_seeds(&self) -> Result<Vec<SeedDetail>, ServiceError> {
        let (host, _) = self.host_and_lang().await.map_err(ServiceError::Other)?;
        let rows = sqlx::query(
            "SELECT series.canonical_video_id, series.title, series.added_from_video_id, series.created_at, series.updated_at, \
                    series.last_scanned_at, series.last_scan_error, COUNT(video.video_id) AS video_count \
             FROM hanime1_series AS series \
             LEFT JOIN hanime1_series_video AS video ON video.canonical_video_id = series.canonical_video_id \
             GROUP BY series.canonical_video_id \
             ORDER BY series.created_at, series.canonical_video_id",
        )
        .fetch_all(self.db.pool())
        .await
        .map_err(other)?;
        let mut seeds = Vec::with_capacity(rows.len());
        for row in rows {
            let video_id: String = row.try_get("canonical_video_id").map_err(other)?;
            let title: String = row.try_get("title").map_err(other)?;
            seeds.push(SeedDetail {
                label: build_hanime1_seed(&video_id, Some(&title)).map_err(|error| ServiceError::Other(error.formatted()))?,
                added_from_video_id: row.try_get("added_from_video_id").map_err(other)?,
                video_count: row.try_get("video_count").map_err(other)?,
                created_at: row.try_get("created_at").map_err(other)?,
                updated_at: row.try_get("updated_at").map_err(other)?,
                last_scanned_at: row.try_get("last_scanned_at").map_err(other)?,
                last_scan_error: row.try_get("last_scan_error").map_err(other)?,
                watch_url: watch_url(&host, &video_id),
                video_id,
                title,
            });
        }
        Ok(seeds)
    }

    /// `delete_seed(canonical_video_id)`: whether a series was deleted (its members
    /// cascade).
    pub async fn delete_seed(&self, canonical_video_id: &str) -> Result<bool, ServiceError> {
        let normalized = py_strip(canonical_video_id);
        if !py_isdecimal(normalized) {
            return Err(ServiceError::Invalid);
        }
        let normalized = int_text(normalized).map_err(|_| ServiceError::Invalid)?;
        if normalized == "0" {
            return Err(ServiceError::Invalid);
        }
        let deleted = sqlx::query("DELETE FROM hanime1_series WHERE canonical_video_id = $1 RETURNING canonical_video_id")
            .bind(normalized)
            .fetch_optional(self.db.pool())
            .await
            .map_err(other)?;
        Ok(deleted.is_some())
    }

    /// `readiness_seed_ids(limit=...)`.
    pub async fn load_readiness_seed_ids(&self, limit: i64) -> Result<Vec<String>, String> {
        // A connection of its own, as Python's `psycopg.connect`: a database that refuses
        // connections fails the check at once (`database_unavailable`), where the pool
        // would keep retrying until the readiness budget ran out.
        let mut connection = self.db.dedicated_connection().await.map_err(|error| error.to_string())?;
        let rows = sqlx::query(
            "SELECT canonical_video_id FROM hanime1_series \
             ORDER BY last_scanned_at DESC NULLS LAST, updated_at DESC, canonical_video_id LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&mut connection)
        .await
        .map_err(|error| error.to_string())?;
        drop(connection);
        rows.iter()
            .map(|row| row.try_get("canonical_video_id").map_err(|error| error.to_string()))
            .collect()
    }

    /// `probe_seed(seed_id, timeout_seconds=...)`: the watch page must still parse as a
    /// playlist with current item titles.
    pub async fn probe(&self, seed_id: &str, timeout: Duration) -> Result<(), ProbeError> {
        let (host, user_lang) = self.host_and_lang().await.map_err(ProbeError::Other)?;
        let probe = async {
            let page = self
                .fetch_watch_page(&host, &user_lang, seed_id)
                .await
                .map_err(PageError::into_job_error)?;
            match parse_hanime1_playlist(&page, Some(seed_id), true)? {
                Some(_) => Ok(()),
                None => Err(parser_incompatible(
                    "Hanime1 watch page did not expose a compatible playlist structure",
                )),
            }
        };
        match tokio::time::timeout(timeout, probe).await {
            Err(_) => Err(ProbeError::Timeout),
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) if is_parser_incompatible(&error) => Err(ProbeError::ParserIncompatible),
            Ok(Err(error)) => Err(ProbeError::Other(error.formatted())),
        }
    }
}

impl SeriesSubscriptions for Hanime1SeriesService {
    fn add<'a>(&'a self, raw_seed: &'a str) -> BoxFuture<'a, Result<RuntimeSeriesSeed, ServiceError>> {
        Box::pin(self.add_seed(raw_seed))
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<SeedDetail>, ServiceError>> {
        Box::pin(self.list_seeds())
    }

    fn delete<'a>(&'a self, canonical_video_id: &'a str) -> BoxFuture<'a, Result<bool, ServiceError>> {
        Box::pin(self.delete_seed(canonical_video_id))
    }

    fn readiness_seed_ids(&self, limit: i64) -> BoxFuture<'_, Result<Vec<String>, String>> {
        Box::pin(self.load_readiness_seed_ids(limit))
    }

    fn probe_seed<'a>(&'a self, seed_id: &'a str, timeout: Duration) -> BoxFuture<'a, Result<(), ProbeError>> {
        Box::pin(self.probe(seed_id, timeout))
    }
}

#[cfg(test)]
mod tests;
