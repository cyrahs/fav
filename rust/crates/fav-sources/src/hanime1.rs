//! Hanime1: subscribed series and authors, plus series discovered from the ranking pages,
//! downloaded with `yt-dlp`.
//!
//! Python origin: `src/web/hanime1.py`. A run:
//!
//! 1. ranking discovery: the weekly/monthly ranking pages' videos whose series is not yet
//!    known become new `hanime1_series` rows, and the optional deep scan walks extra pages
//!    to keep a fractional quota of new series per run (the debt in
//!    `hanime1_ranking_scan_state`);
//! 2. every series' watch page lists its videos; new ones (not in `hanime1`) whose watch
//!    title carries no placeholder marker are downloaded under `<path>/<genre>/<series>`
//!    and named after the playlist item (or `Series NN`); a parser that no longer
//!    understands any playlist fails the run with its own notification key, resolved by
//!    the next run that reads a playlist;
//! 3. every subscribed author's "uploaded" pages, the same way under `<genre>/<author>`;
//!    a failure here never fails the run.
//!
//! The pure parsers are in [`parse`]; the API's seed and author services in
//! [`crate::hanime1_series`] and [`crate::hanime1_author`]. `yt-dlp` runs through
//! [`CommandRunner`], a private seam until WP7's shared subprocess runner lands (cleanup
//! note in `rust/docs/PLAN.md`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fav_core::filename::{MAX_TOTAL_BYTES, ensure_unique_path, format_video_filename};
use fav_core::logging::{self, Logger};
use fav_core::pycasefold::casefold;
use fav_core::pyjson::{Dict, Value};
use fav_core::pypath;
use fav_core::pytext::py_strip;
use fav_core::settings;
use fav_core::validate::LiteralStr;
use fav_db::{Db, Param};
use fav_net::retry::{Sleeper, tokio_sleeper};
use fav_net::{ClientOptions, HttpClient, Request};
use fav_runtime::BoxFuture;
use fav_runtime::notifications::{NewNotification, Outbox, Resolution, format_job_failure_dedupe_key};
use fav_runtime::pystr::repr_str;
use fav_runtime::source::{JobError, Registry, Source};
use sqlx::Row as _;

use crate::hanime1_author::{Hanime1AuthorVideoEntry, author_page_url, extract_author_video_entries};
use crate::hanime1_series::{is_cf_blocked, page_text, value_error, watch_url};
use crate::kit::{OutboxNotifier, SharedNotifier, move_file};
use crate::opencc::to_simplified_chinese;
use crate::runtime_config::{
    PARSER_INCOMPATIBLE_DEDUPE_KEY, RuntimeSeriesSeed, is_parser_incompatible, parser_incompatible, select_hanime1_canonical_id,
};
use crate::{SourceContext, execute, pyerr, settings_job_error};

mod parse;
pub use parse::*;

const RANKING_DEBT_EPSILON: f64 = 1e-9;
const AUTHOR_MAX_PAGES: i64 = 50;
const DOWNLOAD_MAX_ATTEMPTS: u32 = 3;
const DOWNLOAD_BASE_DELAY: f64 = 5.0;
const HTTP_NOT_FOUND: u16 = 404;
const PAGE_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8";
const PAGE_ACCEPT_LANGUAGE: &str = "zh-CN,zh;q=0.9,en;q=0.8";

fn log() -> Logger {
    logging::get("hanime1")
}

/// `_PARSER_FAILURE_DEDUPE_KEY`: the run failure of an incompatible playlist parser.
pub fn parser_failure_dedupe_key() -> String {
    format_job_failure_dedupe_key("hanime1", PARSER_INCOMPATIBLE_DEDUPE_KEY)
}

// ---------- yt-dlp ----------

/// What a finished command left: its exit code (`None` when a signal ended it) and output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// `subprocess.run(command, capture_output=True)`; tests put a fake behind it.
pub trait CommandRunner: Send + Sync {
    fn run(&self, argv: Vec<String>) -> BoxFuture<'_, std::io::Result<CommandOutput>>;
}

/// The real runner. The child is killed if the run is cancelled.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessRunner;

impl CommandRunner for ProcessRunner {
    fn run(&self, argv: Vec<String>) -> BoxFuture<'_, std::io::Result<CommandOutput>> {
        Box::pin(async move {
            let (program, rest) = argv.split_first().ok_or_else(|| std::io::Error::other("empty command"))?;
            let output = tokio::process::Command::new(program)
                .args(rest)
                .stdin(std::process::Stdio::null())
                .kill_on_drop(true)
                .output()
                .await?;
            Ok(CommandOutput {
                code: output.status.code(),
                stdout: output.stdout,
                stderr: output.stderr,
            })
        })
    }
}

/// `subprocess.run(command, text=True)` output: decoded with universal newlines (`\r\n`
/// and `\r` read as `\n`, which matters for yt-dlp's progress lines); bad bytes are
/// replaced where Python would raise (DIVERGENCES.md D76).
fn output_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).replace("\r\n", "\n").replace('\r', "\n")
}

fn os_error(error: &std::io::Error, path: Option<&Path>) -> JobError {
    let os = fav_net::download::PyOsError::from_io(error, path);
    JobError::new(os.class, os.message)
}

// ---------- the job ----------

/// The Hanime1 job (`JOB_SPECS['hanime1']`).
pub struct Hanime1 {
    ctx: SourceContext,
}

impl std::fmt::Debug for Hanime1 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hanime1").finish_non_exhaustive()
    }
}

impl Hanime1 {
    pub fn new(ctx: SourceContext) -> Self {
        Self { ctx }
    }
}

/// Register the job.
pub fn register(registry: &mut Registry, ctx: &SourceContext) {
    let ctx = ctx.clone();
    registry.per_run("hanime1", move || Box::new(Hanime1::new(ctx.clone())));
}

/// `httpx.AsyncClient(...)` as `Hanime1.__init__` builds it.
pub fn client_options() -> ClientOptions {
    ClientOptions {
        headers: vec![
            ("User-Agent".to_owned(), USER_AGENT.to_owned()),
            ("Accept-Language".to_owned(), "en-US,en;q=0.9".to_owned()),
        ],
        timeout: Duration::from_mins(1),
        follow_redirects: true,
        max_keepalive_connections: 10,
        ..ClientOptions::default()
    }
}

impl Source for Hanime1 {
    fn update(&mut self) -> BoxFuture<'_, Result<(), JobError>> {
        Box::pin(async move {
            let settings = self.ctx.settings.load().await.map_err(|error| settings_job_error(&error))?;
            let http = HttpClient::new(&client_options()).map_err(|error| value_error(error.to_string()))?;
            let cache = tokio::task::spawn_blocking(|| tempfile::Builder::new().prefix("fav-hanime1-").tempdir())
                .await
                .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
                .map_err(|error| os_error(&error, None))?;
            let outbox = Outbox::new(self.ctx.db.clone(), Arc::clone(&self.ctx.settings));
            let run = Run::new(
                settings.web.hanime1.clone(),
                http,
                self.ctx.db.clone(),
                outbox.clone(),
                Arc::new(OutboxNotifier(outbox)),
                Arc::new(ProcessRunner),
                tokio_sleeper(),
                cache.path().to_path_buf(),
            );
            let result = run.update().await;
            // Removing the cache directory walks it; keep that off the async threads.
            let _ = tokio::task::spawn_blocking(move || drop(cache)).await;
            result
        })
    }
}

/// A series or author video to consider: `HanimeCandidate`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    video_id: String,
    source_name: String,
    archive_title: String,
}

/// Candidates in first-seen order, keyed by the case-folded id.
#[derive(Debug, Default)]
struct Candidates {
    keys: HashSet<String>,
    items: Vec<(String, Candidate)>,
}

impl Candidates {
    /// Whether it was new.
    fn insert(&mut self, candidate: Candidate) -> bool {
        let key = casefold(&candidate.video_id);
        if !self.keys.insert(key.clone()) {
            return false;
        }
        self.items.push((key, candidate));
        true
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// `DownloadResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadResult {
    pub item_id: String,
    pub title: String,
    pub stream_url: String,
    pub final_path: PathBuf,
    pub archive_title: String,
    pub resolution: Option<String>,
    pub uploader: Option<String>,
    pub release_date: Option<String>,
    pub plot: Option<String>,
    pub cover_url: Option<String>,
}

/// Why an item was not downloaded.
#[derive(Debug, Clone)]
pub enum ItemError {
    /// `IgnoredVideoError`: a placeholder title; logged at info.
    Ignored(String),
    /// Anything else.
    Failed(JobError),
}

impl From<JobError> for ItemError {
    fn from(error: JobError) -> Self {
        Self::Failed(error)
    }
}

/// One run: the settings snapshot and everything the run talks to.
pub(crate) struct Run {
    cfg: settings::Hanime1,
    host: String,
    http: HttpClient,
    db: Db,
    outbox: Outbox,
    notifier: SharedNotifier,
    runner: Arc<dyn CommandRunner>,
    sleeper: Sleeper,
    cache_dir: PathBuf,
}

impl Run {
    #[allow(clippy::too_many_arguments)] // Everything a run talks to, injected.
    pub(crate) fn new(
        cfg: settings::Hanime1,
        http: HttpClient,
        db: Db,
        outbox: Outbox,
        notifier: SharedNotifier,
        runner: Arc<dyn CommandRunner>,
        sleeper: Sleeper,
        cache_dir: PathBuf,
    ) -> Self {
        let host = cfg.host.trim_end_matches('/').to_owned();
        Self {
            cfg,
            host,
            http,
            db,
            outbox,
            notifier,
            runner,
            sleeper,
            cache_dir,
        }
    }

    fn home(&self) -> String {
        format!("{}/", self.host)
    }

    /// `_get_cookie_header()`: the subtitle language.
    fn cookie_header(&self) -> String {
        format!("user_lang={}", self.cfg.user_lang.as_str())
    }

    /// GET a page with `_build_page_headers(referer)`; `None` for a 404 when `allow_404`.
    async fn get_page(&self, url: &str, referer: &str, allow_404: bool) -> Result<Option<String>, JobError> {
        let request = Request::get(url)
            .header("Referer", referer)
            .header("User-Agent", USER_AGENT)
            .header("Accept", PAGE_ACCEPT)
            .header("Accept-Language", PAGE_ACCEPT_LANGUAGE)
            .header("Cookie", self.cookie_header());
        self.send_page(request, allow_404).await
    }

    async fn send_page(&self, request: Request, allow_404: bool) -> Result<Option<String>, JobError> {
        let response = self.http.send(request).await.map_err(|error| pyerr::transport_error(&error))?;
        if allow_404 && response.status() == HTTP_NOT_FOUND {
            return Ok(None);
        }
        response.error_for_status().map_err(|error| pyerr::status_error(&error))?;
        let body = response.bytes().await.map_err(|error| pyerr::transport_error(&error))?;
        Ok(Some(page_text(&body)))
    }

    async fn page(&self, url: &str, referer: &str) -> Result<String, JobError> {
        Ok(self.get_page(url, referer, false).await?.unwrap_or_default())
    }

    // ----- pages -----

    /// `resolve_stream_from_download_page(item)`: the best download URL and the page's
    /// title.
    async fn resolve_stream_from_download_page(&self, item: &HanimeRecord) -> Result<(String, Option<String>), JobError> {
        let download_page_url = build_download_page_url(&self.host, item)?;
        let referer = item.page_url.clone().filter(|url| !url.is_empty()).unwrap_or_else(|| self.home());
        let page = self.page(&download_page_url, &referer).await?;
        if is_cf_blocked(&page) {
            return Err(value_error("Blocked by Cloudflare while resolving Hanime1 download page."));
        }
        let Some(stream_url) = extract_download_urls(&page).into_iter().next() else {
            return Err(value_error(format!("Could not find download URL from page: {download_page_url}")));
        };
        let title = normalize_download_title(parse_title(&page).as_deref());
        Ok((stream_url, to_simplified_chinese(title.as_deref())))
    }

    /// `_request_watch_page_html(item)`.
    async fn request_watch_page_html(&self, item: &HanimeRecord) -> Result<String, JobError> {
        let watch_page_url = build_watch_page_url(&self.host, item)?;
        let page = self.page(&watch_page_url, &self.home()).await?;
        if is_cf_blocked(&page) {
            return Err(value_error("Blocked by Cloudflare while resolving Hanime1 watch page."));
        }
        Ok(page)
    }

    /// `fetch_ranking_video_ids(period=..., page=...)`.
    pub(crate) async fn fetch_ranking_video_ids(&self, period: &str, page: i64) -> Result<Vec<String>, JobError> {
        let ranking_url = build_ranking_page_url(&self.host, period, page)?;
        let html = self.page(&ranking_url, &self.home()).await?;
        if is_cf_blocked(&html) {
            return Err(value_error(format!(
                "Blocked by Cloudflare while fetching Hanime1 ranking page: {ranking_url}"
            )));
        }
        extract_search_video_ids(&self.host, &html)
    }

    async fn resolve_metadata_from_watch_page(&self, item: &HanimeRecord) -> Result<WatchMetadata, JobError> {
        let page = self.request_watch_page_html(item).await?;
        Ok(extract_watch_metadata(&self.host, &page))
    }

    async fn resolve_series_from_watch_page(&self, item: &HanimeRecord) -> Result<Option<WatchSeries>, JobError> {
        let page = self.request_watch_page_html(item).await?;
        extract_watch_series(&page)
    }

    /// `resolve_stream_from_page(page_url)`: a stream URL from the watch page itself.
    async fn resolve_stream_from_page(&self, page_url: &str) -> Result<(String, Option<String>), JobError> {
        let request = Request::get(page_url)
            .header("Referer", self.home())
            .header("User-Agent", USER_AGENT)
            .header("Cookie", self.cookie_header());
        let page = self.send_page(request, false).await?.unwrap_or_default();
        if is_cf_blocked(&page) {
            return Err(value_error(
                "Blocked by Cloudflare while resolving Hanime1 page. Provide direct stream_url or try again later.",
            ));
        }
        let Some(stream_url) = extract_stream_urls(&page)?.into_iter().next() else {
            return Err(value_error(format!("Could not find stream URL from page: {page_url}")));
        };
        Ok((stream_url, to_simplified_chinese(parse_title(&page).as_deref())))
    }

    // ----- yt-dlp -----

    /// `download_stream(task=..., dirpath=...)`: up to three `yt-dlp` attempts (waits 5 s,
    /// 10 s), each in an emptied directory; the largest file it left.
    async fn download_stream(
        &self,
        stream_url: &str,
        video_id: &str,
        referer: &str,
        cookie_header: &str,
        dirpath: &Path,
    ) -> Result<PathBuf, JobError> {
        let output_template = pypath::join(&dirpath.to_string_lossy(), &format!("{video_id}.%(ext)s"));
        let command = build_download_command(stream_url, &output_template, Some(referer), Some(cookie_header));
        let mut attempt = 1;
        loop {
            match self.run_download_once(&command, dirpath).await {
                Ok(()) => break,
                Err(DownloadAttempt::Retryable(message)) if attempt < DOWNLOAD_MAX_ATTEMPTS => {
                    // tenacity's wait_exponential(multiplier=5, min=5, max=30).
                    let wait =
                        (DOWNLOAD_BASE_DELAY * f64::from(2_u32.pow(attempt - 1))).clamp(DOWNLOAD_BASE_DELAY, DOWNLOAD_BASE_DELAY * 6.0);
                    log().debug(format!(
                        "Retrying Hanime1 download for {video_id} ({attempt}/{DOWNLOAD_MAX_ATTEMPTS}), next wait {wait:.1}s: {message}"
                    ));
                    (self.sleeper)(wait).await;
                    attempt += 1;
                }
                Err(DownloadAttempt::Retryable(message)) => return Err(JobError::new("DownloadError", message)),
                Err(DownloadAttempt::Fatal(error)) => return Err(error),
            }
        }
        let mut candidates = Vec::new();
        let mut entries = tokio::fs::read_dir(dirpath)
            .await
            .map_err(|error| os_error(&error, Some(dirpath)))?;
        while let Some(entry) = entries.next_entry().await.map_err(|error| os_error(&error, Some(dirpath)))? {
            let metadata = entry.metadata().await.map_err(|error| os_error(&error, Some(&entry.path())))?;
            if metadata.is_file() {
                candidates.push((metadata.len(), entry.path()));
            }
        }
        // `sort(key=size, reverse=True)` keeps directory order among equal sizes.
        candidates.sort_by_key(|(size, _)| std::cmp::Reverse(*size));
        candidates
            .into_iter()
            .next()
            .map(|(_, path)| path)
            .ok_or_else(|| JobError::new("DownloadError", format!("No output file generated for {video_id}")))
    }

    async fn run_download_once(&self, command: &[String], dirpath: &Path) -> Result<(), DownloadAttempt> {
        cleanup_dir(dirpath).await.map_err(DownloadAttempt::Fatal)?;
        let output = self
            .runner
            .run(command.to_vec())
            .await
            .map_err(|error| DownloadAttempt::Fatal(os_error(&error, Some(Path::new(&command[0])))))?;
        let stderr = output_text(&output.stderr);
        if output.code == Some(0) {
            if !stderr.is_empty() {
                log().debug(format!("yt-dlp stderr: {}", py_strip(&stderr)));
            }
            return Ok(());
        }
        let stdout = output_text(&output.stdout);
        let message = [py_strip(&stderr), py_strip(&stdout)]
            .into_iter()
            .find(|text| !text.is_empty())
            .map_or_else(
                // `returncode` is negative for a signal in Python.
                || format!("yt-dlp exited with code {}", output.code.unwrap_or(-1)),
                str::to_owned,
            );
        Err(DownloadAttempt::Retryable(message))
    }

    // ----- notifications -----

    /// `_notify_download(...)`: a failure to queue is logged, never raised; an item without
    /// a watch URL raises, as in Python (unreachable for a downloaded item).
    async fn notify_download(
        &self,
        item: &HanimeRecord,
        resolution: Option<&str>,
        file_size_bytes: Option<u64>,
        release_date: Option<&str>,
        cover_url: Option<&str>,
    ) -> Result<(), JobError> {
        let title = match py_strip(&item.title) {
            "" => item.id.clone(),
            title => title.to_owned(),
        };
        let watch_url = build_watch_page_url(&self.host, item)?;
        let file_size_label = format_file_size(file_size_bytes.map(i128::from)).unwrap_or_else(|| "unknown".to_owned());
        let release_date_label = match py_strip(release_date.unwrap_or("unknown")) {
            "" => "unknown",
            label => label,
        };
        let body = format!("{} | {file_size_label} | {release_date_label}", resolution.unwrap_or("unknown"));
        let mut payload = Dict::new();
        payload.insert("video_id", Value::Str(item.id.clone()));
        payload.insert("resolution", resolution.into());
        payload.insert(
            "file_size_bytes",
            file_size_bytes.map_or(Value::Null, |size| Value::Int(i128::from(size))),
        );
        payload.insert("release_date", release_date.into());
        let notification = NewNotification {
            kind: "download_completed".to_owned(),
            source: "hanime1".to_owned(),
            header: "Hanime1".to_owned(),
            title,
            body,
            link_url: watch_url,
            image_url: cover_url.unwrap_or_default().to_owned(),
            payload: Some(payload),
            ..NewNotification::default()
        };
        if let Err(error) = self.notifier.enqueue(notification).await {
            log().warning(format!("Failed to enqueue hanime1 download notification for {}: {error}", item.id));
        }
        Ok(())
    }

    /// `_resolve_parser_failure_notification()`: logged, never raised.
    async fn resolve_parser_failure_notification(&self) {
        let dedupe_key = parser_failure_dedupe_key();
        let mut payload = Dict::new();
        payload.insert("job", Value::from("hanime1"));
        payload.insert("dedupe_key", Value::Str(dedupe_key.clone()));
        let resolution = Resolution {
            kind: "job_failed".to_owned(),
            source: "worker".to_owned(),
            header: "Hanime1".to_owned(),
            title: "Job recovered".to_owned(),
            body: "Hanime1 playlist parsing is healthy again.".to_owned(),
            payload: Some(payload),
            ..Resolution::default()
        };
        if let Err(error) = self.outbox.resolve(&dedupe_key, &resolution).await {
            log().warning(format!("Failed to resolve Hanime1 parser failure notification: {error}"));
        }
    }

    // ----- series seeds -----

    /// `_load_runtime_series_seeds()`.
    pub(crate) async fn load_runtime_series_seeds(&self) -> Result<Vec<RuntimeSeriesSeed>, JobError> {
        let rows = execute(
            &self.db,
            "SELECT canonical_video_id, title FROM hanime1_series ORDER BY created_at, canonical_video_id",
            Vec::new(),
        )
        .await?;
        let mut seeds = Vec::new();
        for row in &rows {
            let video_id = text(row, "canonical_video_id")?;
            let title = text(row, "title")?;
            if video_id.is_empty() || title.is_empty() {
                continue;
            }
            seeds.push(RuntimeSeriesSeed { video_id, title });
        }
        Ok(seeds)
    }

    /// `_collect_ranking_video_ids()`: every ranking page; a failed page is skipped.
    async fn collect_ranking_video_ids(&self) -> Vec<String> {
        let ranking = &self.cfg.ranking;
        let mut collected = Vec::new();
        let mut seen = HashSet::new();
        for period in &ranking.periods {
            let period = period.as_str();
            for page in 1..=ranking.pages {
                let ranking_ids = match self.fetch_ranking_video_ids(period, page).await {
                    Ok(ids) => ids,
                    Err(error) => {
                        log().warning(format!("Failed to fetch Hanime1 {period} ranking page {page}: {}", error.message()));
                        continue;
                    }
                };
                log().info(format!(
                    "Hanime1 {period} ranking page {page} returned {} videos",
                    ranking_ids.len()
                ));
                for video_id in ranking_ids {
                    if seen.insert(casefold(&video_id)) {
                        collected.push(video_id);
                    }
                }
            }
        }
        collected
    }

    /// `_get_known_series_video_ids(video_ids)`: the case-folded ids some series claims.
    async fn known_series_video_ids(&self, video_ids: &[String]) -> Result<HashSet<String>, JobError> {
        if video_ids.is_empty() {
            return Ok(HashSet::new());
        }
        let ids = sqlx::query("SELECT video_id FROM hanime1_series_video WHERE video_id = ANY($1)")
            .bind(video_ids)
            .fetch_all(self.db.pool())
            .await
            .map_err(|error| pyerr::db_error(&fav_db::DbError::Sqlx(error)))?;
        let mut known = HashSet::new();
        for row in &ids {
            let video_id = text(row, "video_id")?;
            if !video_id.is_empty() {
                known.insert(casefold(&video_id));
            }
        }
        Ok(known)
    }

    /// `_insert_ranking_series_seed(...)`: the new series (none when it has no title or a
    /// member is already claimed) and its members.
    async fn insert_ranking_series_seed(
        &self,
        added_from_video_id: &str,
        series: &WatchSeries,
    ) -> Result<(Option<RuntimeSeriesSeed>, Vec<String>), JobError> {
        let member_ids = series_member_ids_from_watch_series(added_from_video_id, series)?;
        let canonical_id = select_hanime1_canonical_id(&member_ids);
        let title = to_simplified_chinese(series.name.as_deref()).or_else(|| series.name.clone());
        let (Some(canonical_id), Some(title)) = (canonical_id, title.filter(|title| !title.is_empty())) else {
            return Ok((None, member_ids));
        };
        if !self.known_series_video_ids(&member_ids).await?.is_empty() {
            return Ok((None, member_ids));
        }
        let seed = RuntimeSeriesSeed {
            video_id: canonical_id,
            title,
        };
        execute(
            &self.db,
            "INSERT INTO hanime1_series (canonical_video_id, title, added_from_video_id) VALUES ($1, $2, $3) \
             ON CONFLICT (canonical_video_id) DO NOTHING",
            vec![
                Param::from(seed.video_id.as_str()),
                Param::from(seed.title.as_str()),
                Param::from(added_from_video_id),
            ],
        )
        .await?;
        self.upsert_series_members(&seed.video_id, &member_ids).await?;
        Ok((Some(seed), member_ids))
    }

    /// `_discover_series_from_ranking_ids(...)`: new series from ranking videos no series
    /// claims yet (at most `limit`), and how many were skipped as known.
    async fn discover_series_from_ranking_ids(
        &self,
        video_ids: &[String],
        known_video_ids: &mut HashSet<String>,
        limit: Option<usize>,
    ) -> (Vec<RuntimeSeriesSeed>, usize) {
        let mut added = Vec::new();
        let mut skipped_known = 0;
        for video_id in video_ids {
            if limit.is_some_and(|limit| added.len() >= limit) {
                break;
            }
            if known_video_ids.contains(&casefold(video_id)) {
                skipped_known += 1;
                continue;
            }
            let item = HanimeRecord {
                id: video_id.clone(),
                page_url: Some(watch_url(&self.host, video_id)),
                ..HanimeRecord::default()
            };
            let outcome = match self.resolve_series_from_watch_page(&item).await {
                Ok(Some(series)) if series.name.as_deref().is_some_and(|name| !name.is_empty()) => {
                    self.insert_ranking_series_seed(video_id, &series).await
                }
                Ok(_) => {
                    log().warning(format!("Hanime1 ranking video {video_id} did not expose a resolvable watch series"));
                    continue;
                }
                Err(error) => Err(error),
            };
            let (seed, member_ids) = match outcome {
                Ok(outcome) => outcome,
                Err(error) => {
                    log().error(format!(
                        "Failed to add Hanime1 ranking video {video_id} as a series target: {}",
                        error.formatted()
                    ));
                    continue;
                }
            };
            known_video_ids.extend(member_ids.iter().map(|id| casefold(id)));
            let Some(seed) = seed else {
                skipped_known += 1;
                continue;
            };
            log().info(format!(
                "Added Hanime1 ranking series target {} from ranking video {video_id}: {}",
                seed.video_id, seed.title
            ));
            added.push(seed);
        }
        (added, skipped_known)
    }

    /// `_load_ranking_scan_debt()`: 0 for a missing, negative or non-finite value.
    async fn load_ranking_scan_debt(&self) -> Result<f64, JobError> {
        let rows = execute(
            &self.db,
            "SELECT quota_debt FROM hanime1_ranking_scan_state WHERE id = 1",
            Vec::new(),
        )
        .await?;
        let Some(row) = rows.first() else {
            return Ok(0.0);
        };
        let debt: Option<f64> = row
            .try_get("quota_debt")
            .map_err(|error| pyerr::db_error(&fav_db::DbError::Sqlx(error)))?;
        let debt = debt.unwrap_or(0.0);
        Ok(if debt.is_finite() && debt >= 0.0 { debt } else { 0.0 })
    }

    async fn save_ranking_scan_debt(&self, debt: f64) -> Result<(), JobError> {
        execute(
            &self.db,
            "INSERT INTO hanime1_ranking_scan_state (id, quota_debt, updated_at) VALUES (1, $1, CURRENT_TIMESTAMP) \
             ON CONFLICT (id) DO UPDATE SET quota_debt = EXCLUDED.quota_debt, updated_at = EXCLUDED.updated_at",
            vec![Param::from(debt)],
        )
        .await
        .map(drop)
    }

    /// `_run_ranking_deep_scan(...)`: the quota adds to a debt (capped at the quota rounded
    /// up, at least 1) that the base pages' new series pay first; whole units left are
    /// found on extra ranking pages, page by page across the periods.
    async fn run_ranking_deep_scan(
        &self,
        base_added_count: usize,
        known_video_ids: &mut HashSet<String>,
    ) -> Result<Vec<RuntimeSeriesSeed>, JobError> {
        let ranking = &self.cfg.ranking;
        let deep = &ranking.deep_scan;
        let debt_cap = deep.quota.ceil().max(1.0);
        let mut debt = (self.load_ranking_scan_debt().await? + deep.quota).min(debt_cap);
        debt = (debt - count_f64(base_added_count)).max(0.0);
        let needed_float = (debt + RANKING_DEBT_EPSILON).floor();
        if needed_float < 1.0 {
            self.save_ranking_scan_debt(debt).await?;
            return Ok(Vec::new());
        }
        // The debt is capped by the quota, a small setting.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // a positive whole float well inside usize
        let needed = needed_float as usize;
        log().info(format!("Hanime1 ranking deep scan needs {needed} new series (debt {debt:.2})"));
        let mut added: Vec<RuntimeSeriesSeed> = Vec::new();
        let mut active_periods: Vec<&'static str> = ranking.periods.iter().map(|period| period.as_str()).collect();
        let first_extra_page = ranking.pages + 1;
        for page in first_extra_page..first_extra_page + deep.max_extra_pages {
            if active_periods.is_empty() || added.len() >= needed {
                break;
            }
            for period in active_periods.clone() {
                let ranking_ids = match self.fetch_ranking_video_ids(period, page).await {
                    Ok(ids) => ids,
                    Err(error) => {
                        log().warning(format!(
                            "Failed to fetch Hanime1 {period} ranking page {page} during deep scan: {}",
                            error.message()
                        ));
                        remove_first(&mut active_periods, period);
                        continue;
                    }
                };
                if ranking_ids.is_empty() {
                    log().info(format!(
                        "Hanime1 {period} ranking page {page} is empty, deep scan exhausted this period"
                    ));
                    remove_first(&mut active_periods, period);
                    continue;
                }
                log().info(format!(
                    "Hanime1 deep scan {period} ranking page {page} returned {} videos",
                    ranking_ids.len()
                ));
                let unknown_ids: Vec<String> = ranking_ids
                    .iter()
                    .filter(|video_id| !known_video_ids.contains(&casefold(video_id)))
                    .cloned()
                    .collect();
                let known = self.known_series_video_ids(&unknown_ids).await?;
                known_video_ids.extend(known);
                let (page_seeds, _) = self
                    .discover_series_from_ranking_ids(&ranking_ids, known_video_ids, Some(needed - added.len()))
                    .await;
                added.extend(page_seeds);
                if added.len() >= needed {
                    break;
                }
            }
        }
        debt = (debt - count_f64(added.len())).max(0.0);
        self.save_ranking_scan_debt(debt).await?;
        log().info(format!(
            "Hanime1 ranking deep scan added {} of {needed} required new series (remaining debt {debt:.2})",
            added.len()
        ));
        Ok(added)
    }

    /// `discover_ranking_series()`.
    pub(crate) async fn discover_ranking_series(&self) -> Result<Vec<RuntimeSeriesSeed>, JobError> {
        let ranking = &self.cfg.ranking;
        if !ranking.enabled {
            return Ok(Vec::new());
        }
        let ranking_video_ids = self.collect_ranking_video_ids().await;
        let mut known_video_ids = HashSet::new();
        let mut added = Vec::new();
        if ranking_video_ids.is_empty() {
            log().info("Hanime1 ranking discovery found no ranking videos");
        } else {
            known_video_ids = self.known_series_video_ids(&ranking_video_ids).await?;
            let (seeds, skipped_known) = self
                .discover_series_from_ranking_ids(&ranking_video_ids, &mut known_video_ids, None)
                .await;
            log().info(format!(
                "Hanime1 ranking discovery added {} new series from {} ranking videos ({skipped_known} already known)",
                seeds.len(),
                ranking_video_ids.len()
            ));
            added = seeds;
        }
        if ranking.deep_scan.enabled {
            let deep = self.run_ranking_deep_scan(added.len(), &mut known_video_ids).await?;
            added.extend(deep);
        }
        Ok(added)
    }

    /// `_upsert_series_members(...)`.
    async fn upsert_series_members(&self, canonical_video_id: &str, member_ids: &[String]) -> Result<(), JobError> {
        let mut seen = HashSet::new();
        for video_id in member_ids {
            let normalized = py_strip(video_id);
            if normalized.is_empty() || !seen.insert(normalized.to_owned()) {
                continue;
            }
            execute(
                &self.db,
                "INSERT INTO hanime1_series_video (video_id, canonical_video_id) VALUES ($1, $2) ON CONFLICT (video_id) DO NOTHING",
                vec![Param::from(normalized), Param::from(canonical_video_id)],
            )
            .await?;
        }
        Ok(())
    }

    async fn mark_series_scan_success(&self, canonical_video_id: &str) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE hanime1_series SET last_scanned_at = CURRENT_TIMESTAMP, last_scan_error = '', updated_at = CURRENT_TIMESTAMP \
             WHERE canonical_video_id = $1",
            vec![Param::from(canonical_video_id)],
        )
        .await
        .map(drop)
    }

    async fn mark_series_scan_failure(&self, canonical_video_id: &str, error: &JobError) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE hanime1_series SET last_scan_error = $1, updated_at = CURRENT_TIMESTAMP WHERE canonical_video_id = $2",
            vec![Param::from(format_scan_error(error)), Param::from(canonical_video_id)],
        )
        .await
        .map(drop)
    }

    /// `_collect_ids_from_watch_series(seeds)`: every series' videos with their archive
    /// titles. Fails when no series resolved: with the parser's own error when every one
    /// was incompatible.
    async fn collect_ids_from_watch_series(&self, seeds: &[RuntimeSeriesSeed]) -> Result<Candidates, JobError> {
        let mut collected = Candidates::default();
        let mut resolved_any = false;
        let mut incompatible = 0;
        for seed in seeds {
            let seed_item = HanimeRecord {
                id: seed.video_id.clone(),
                page_url: Some(watch_url(&self.host, &seed.video_id)),
                ..HanimeRecord::default()
            };
            let series = match self.resolve_series_from_watch_page(&seed_item).await {
                Ok(Some(series)) => series,
                Ok(None) => {
                    incompatible += 1;
                    let error = parser_incompatible("Hanime1 watch page did not expose a compatible playlist structure");
                    log().warning(format!(
                        "Hanime1 seed {} did not expose a compatible playlist structure",
                        seed.video_id
                    ));
                    self.mark_series_scan_failure(&seed.video_id, &error).await?;
                    continue;
                }
                Err(error) if is_parser_incompatible(&error) => {
                    incompatible += 1;
                    log().warning(format!(
                        "Hanime1 seed {} exposed an incompatible playlist structure: {}",
                        seed.video_id,
                        error.message()
                    ));
                    self.mark_series_scan_failure(&seed.video_id, &error).await?;
                    continue;
                }
                Err(error) => {
                    log().error(format!(
                        "Failed to resolve Hanime1 watch series from seed id {}: {}",
                        seed.video_id,
                        error.formatted()
                    ));
                    self.mark_series_scan_failure(&seed.video_id, &error).await?;
                    continue;
                }
            };
            resolved_any = true;
            let series_name = series
                .name
                .clone()
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| seed.title.clone());
            let candidate_ids = if series.video_ids.is_empty() {
                vec![seed.video_id.clone()]
            } else {
                series.video_ids.clone()
            };
            let source_name = to_simplified_chinese(Some(&series_name)).unwrap_or_else(|| format!("id-{}", seed.video_id));
            let mut members = vec![seed.video_id.clone()];
            members.extend(candidate_ids.iter().cloned());
            self.upsert_series_members(&seed.video_id, &members).await?;
            self.mark_series_scan_success(&seed.video_id).await?;
            let (total, new_count) = add_series_candidates(&mut collected, &source_name, &candidate_ids, series.videos);
            log().info(format!(
                "Seed {} resolved {total} series videos ({new_count} unique) as {}",
                seed.video_id,
                repr_str(&source_name)
            ));
        }
        if !seeds.is_empty() && !resolved_any {
            if incompatible == seeds.len() {
                return Err(parser_incompatible(format!(
                    "Hanime1 playlist parsing failed for all {} sampled series",
                    seeds.len()
                )));
            }
            return Err(JobError::new(
                "RuntimeError",
                format!("Failed to resolve all {} Hanime1 series targets", seeds.len()),
            ));
        }
        Ok(collected)
    }

    /// `_get_downloaded_ids()`: case-folded.
    async fn downloaded_ids(&self) -> Result<HashSet<String>, JobError> {
        let rows = execute(&self.db, "SELECT id FROM hanime1", Vec::new()).await?;
        let mut ids = HashSet::new();
        for row in &rows {
            let id = text(row, "id")?;
            if !id.is_empty() {
                ids.insert(casefold(&id));
            }
        }
        Ok(ids)
    }

    /// `get_items(seeds=...)`: the series' new videos, without placeholder titles.
    pub(crate) async fn get_items(&self, seeds: &[RuntimeSeriesSeed]) -> Result<Vec<HanimeRecord>, JobError> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.collect_ids_from_watch_series(seeds).await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = self.downloaded_ids().await?;
        let mut records = Vec::new();
        for (key, candidate) in candidates.items {
            if seen.contains(&key) {
                continue;
            }
            let mut item = HanimeRecord {
                id: candidate.video_id.clone(),
                title: candidate.archive_title.clone(),
                keyword: Some(candidate.source_name.clone()),
                uploader: None,
                page_url: Some(watch_url(&self.host, &candidate.video_id)),
                stream_url: None,
            };
            let metadata = match self.resolve_metadata_from_watch_page(&item).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    log().debug(format!(
                        "Failed to resolve Hanime1 watch metadata for discovery item {}: {}",
                        candidate.video_id,
                        error.message()
                    ));
                    WatchMetadata::default()
                }
            };
            if let Some(marker) = ignored_title_marker(metadata.title.as_deref()) {
                let normalized = to_simplified_chinese(metadata.title.as_deref())
                    .or_else(|| metadata.title.clone())
                    .unwrap_or_else(|| candidate.video_id.clone());
                log().info(format!(
                    "Skipping Hanime1 discovery item {} because watch title contains ignored marker {marker}: {normalized}",
                    candidate.video_id
                ));
                continue;
            }
            seen.insert(key);
            if item.title.is_empty()
                && let Some(title) = metadata.title
            {
                item.title = title;
            }
            records.push(item);
        }
        log().info(format!(
            "Hanime1 discovery path watch-series(ids) produced {} new videos",
            records.len()
        ));
        Ok(records)
    }

    // ----- authors -----

    /// `_load_runtime_authors()`: `(author_id, name)`.
    async fn load_runtime_authors(&self) -> Result<Vec<(String, String)>, JobError> {
        let rows = execute(
            &self.db,
            "SELECT author_id, name FROM hanime1_author ORDER BY created_at, author_id",
            Vec::new(),
        )
        .await?;
        let mut authors = Vec::new();
        for row in &rows {
            let author_id = text(row, "author_id")?;
            let name = text(row, "name")?;
            if author_id.is_empty() || name.is_empty() {
                continue;
            }
            authors.push((author_id, name));
        }
        Ok(authors)
    }

    async fn mark_author_scan_success(&self, author_id: &str, video_count: usize) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE hanime1_author SET last_scanned_at = CURRENT_TIMESTAMP, last_scan_error = '', video_count = $1, \
             updated_at = CURRENT_TIMESTAMP WHERE author_id = $2",
            vec![Param::from(i64::try_from(video_count).unwrap_or(i64::MAX)), Param::from(author_id)],
        )
        .await
        .map(drop)
    }

    async fn mark_author_scan_failure(&self, author_id: &str, error: &JobError) -> Result<(), JobError> {
        execute(
            &self.db,
            "UPDATE hanime1_author SET last_scan_error = $1, updated_at = CURRENT_TIMESTAMP WHERE author_id = $2",
            vec![Param::from(format_scan_error(error)), Param::from(author_id)],
        )
        .await
        .map(drop)
    }

    /// `fetch_author_page_entries(author_id=..., page=...)`: a 404 reads as an empty page.
    pub(crate) async fn fetch_author_page_entries(&self, author_id: &str, page: i64) -> Result<Vec<Hanime1AuthorVideoEntry>, JobError> {
        let mut url = author_page_url(&self.host, author_id);
        if page > 1 {
            url = format!("{url}?page={page}");
        }
        let Some(html) = self.get_page(&url, &self.home(), true).await? else {
            return Ok(Vec::new());
        };
        if is_cf_blocked(&html) {
            return Err(value_error("Blocked by Cloudflare while resolving Hanime1 author page."));
        }
        extract_author_video_entries(&html)
    }

    /// `_fetch_all_author_entries(author_id)`: pages until one adds nothing new (empty, or
    /// the site echoing a listing), at most 50.
    pub(crate) async fn fetch_all_author_entries(&self, author_id: &str) -> Result<Vec<Hanime1AuthorVideoEntry>, JobError> {
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for page in 1..=AUTHOR_MAX_PAGES {
            let page_entries = self.fetch_author_page_entries(author_id, page).await?;
            let new_entries: Vec<_> = page_entries.into_iter().filter(|entry| !seen.contains(&entry.video_id)).collect();
            if new_entries.is_empty() {
                return Ok(entries);
            }
            for entry in new_entries {
                seen.insert(entry.video_id.clone());
                entries.push(entry);
            }
        }
        log().warning(format!(
            "Hanime1 author {author_id} pagination exceeded {AUTHOR_MAX_PAGES} pages; stopping early"
        ));
        Ok(entries)
    }

    /// `_collect_author_candidates(authors)`: a failing author is recorded and skipped.
    async fn collect_author_candidates(&self, authors: &[(String, String)]) -> Result<Candidates, JobError> {
        let mut collected = Candidates::default();
        for (author_id, name) in authors {
            let entries = match self.fetch_all_author_entries(author_id).await {
                Ok(entries) => entries,
                Err(error) => {
                    log().warning(format!("Failed to scan Hanime1 author {author_id} ({name}): {}", error.message()));
                    self.mark_author_scan_failure(author_id, &error).await?;
                    continue;
                }
            };
            self.mark_author_scan_success(author_id, entries.len()).await?;
            let mut new_count = 0;
            for entry in &entries {
                let candidate = Candidate {
                    video_id: entry.video_id.clone(),
                    source_name: name.clone(),
                    archive_title: to_simplified_chinese(entry.title.as_deref()).unwrap_or_default(),
                };
                if collected.insert(candidate) {
                    new_count += 1;
                }
            }
            log().info(format!(
                "Hanime1 author {author_id} ({name}) listed {} videos ({new_count} unique)",
                entries.len()
            ));
        }
        Ok(collected)
    }

    /// `get_author_items(authors=...)`.
    pub(crate) async fn get_author_items(&self) -> Result<Vec<HanimeRecord>, JobError> {
        let authors = self.load_runtime_authors().await?;
        if authors.is_empty() {
            return Ok(Vec::new());
        }
        let candidates = self.collect_author_candidates(&authors).await?;
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = self.downloaded_ids().await?;
        let mut records = Vec::new();
        for (key, candidate) in candidates.items {
            if seen.contains(&key) {
                continue;
            }
            if let Some(marker) = ignored_title_marker(Some(&candidate.archive_title)) {
                log().info(format!(
                    "Skipping Hanime1 author item {} because listing title contains ignored marker {marker}: {}",
                    candidate.video_id, candidate.archive_title
                ));
                continue;
            }
            seen.insert(key);
            records.push(HanimeRecord {
                page_url: Some(watch_url(&self.host, &candidate.video_id)),
                id: candidate.video_id,
                title: candidate.archive_title,
                keyword: Some(candidate.source_name.clone()),
                uploader: Some(candidate.source_name),
                stream_url: None,
            });
        }
        log().info(format!(
            "Hanime1 discovery path author-uploads produced {} new videos",
            records.len()
        ));
        Ok(records)
    }

    // ----- downloads -----

    /// `_raise_if_ignored_title(...)`.
    fn check_ignored_title(item_id: &str, title: Option<&str>, source: &str) -> Result<(), ItemError> {
        let Some(marker) = ignored_title_marker(title) else {
            return Ok(());
        };
        let normalized = to_simplified_chinese(title)
            .or_else(|| title.filter(|title| !title.is_empty()).map(str::to_owned))
            .unwrap_or_else(|| item_id.to_owned());
        Err(ItemError::Ignored(format!(
            "Skipping Hanime1 item {item_id} because {source} title contains ignored marker {marker}: {normalized}"
        )))
    }

    /// `download_item(item)`.
    pub(crate) async fn download_item(&self, item: &HanimeRecord) -> Result<DownloadResult, ItemError> {
        let item_id = derive_item_id(item)?;
        let item_title = to_simplified_chinese(Some(&item.title));
        let mut archive_title = item_title.clone().unwrap_or_else(|| item_id.clone());
        let mut metadata_title: Option<String> = None;
        let mut uploader = to_simplified_chinese(item.uploader.as_deref());
        let page_url = item.page_url.clone().filter(|url| !url.is_empty());
        Self::check_ignored_title(&item_id, Some(&item.title), "item")?;
        let stream_url = if let Some(stream_url) = item.stream_url.clone().filter(|url| !url.is_empty()) {
            stream_url
        } else {
            let (stream_url, parsed_title) = match self.resolve_stream_from_download_page(item).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    log().debug(format!("Failed to resolve download page for {item_id}: {}", error.message()));
                    let Some(page_url) = &page_url else {
                        return Err(value_error(format!("Item {item_id} has neither valid download page nor page_url")).into());
                    };
                    self.resolve_stream_from_page(page_url).await?
                }
            };
            Self::check_ignored_title(&item_id, parsed_title.as_deref(), "page")?;
            if let Some(parsed_title) = parsed_title {
                if item_title.is_none() {
                    archive_title.clone_from(&parsed_title);
                }
                metadata_title = Some(parsed_title);
            }
            stream_url
        };
        let metadata = match self.resolve_metadata_from_watch_page(item).await {
            Ok(metadata) => metadata,
            Err(error) => {
                log().debug(format!("Failed to resolve watch metadata for {item_id}: {}", error.message()));
                WatchMetadata::default()
            }
        };
        Self::check_ignored_title(&item_id, metadata.title.as_deref(), "watch metadata")?;
        if let Some(title) = &metadata.title {
            metadata_title = Some(title.clone());
            if item_title.is_none() {
                archive_title = strip_uploader_prefix(title, metadata.uploader.as_deref().or(uploader.as_deref()));
            }
        }
        if uploader.is_none() {
            uploader.clone_from(&metadata.uploader);
        }

        let referer = match &page_url {
            Some(page_url) => page_url.clone(),
            None => build_download_page_url(&self.host, item)?,
        };
        let tmp_video_dir = self.cache_dir.join("videos");
        create_dir_all(&tmp_video_dir).await?;
        let downloaded_path = self
            .download_stream(&stream_url, &item_id, &referer, &self.cookie_header(), &tmp_video_dir)
            .await?;

        let ext = pypath::suffix(&downloaded_path.to_string_lossy()).to_owned();
        let filename = format_video_filename(&archive_title, &item_id, None, &ext, MAX_TOTAL_BYTES);
        let (output_dir, missing_genre) = resolve_output_dir(self.cfg.path.as_str(), metadata.genre.as_deref(), item.keyword.as_deref());
        if missing_genre {
            log().warning(format!(
                "Hanime1 {item_id} has no resolvable genre on its watch page; archiving under 未分类"
            ));
        }
        let output_dir = PathBuf::from(output_dir);
        create_dir_all(&output_dir).await?;
        let (source, target) = (downloaded_path.clone(), output_dir.join(&filename));
        let final_path = tokio::task::spawn_blocking(move || {
            let destination = ensure_unique_path(&target);
            move_file(&source, &destination).map(|()| destination)
        })
        .await
        .map_err(|error| JobError::new("RuntimeError", error.to_string()))?
        .map_err(|error| os_error(&error, Some(&downloaded_path)))?;

        Ok(DownloadResult {
            item_id: item.id.clone(),
            title: metadata_title.unwrap_or_else(|| archive_title.clone()),
            resolution: stream_resolution_label(&stream_url),
            stream_url,
            final_path,
            archive_title,
            uploader,
            release_date: metadata.release_date,
            plot: metadata.plot,
            cover_url: metadata.cover_url,
        })
    }

    /// `_download_and_record(items)`: each item downloaded, recorded and announced; a
    /// failing item is logged and skipped.
    async fn download_and_record(&self, items: Vec<HanimeRecord>) -> Result<(), JobError> {
        let total = items.len();
        for (index, mut item) in items.into_iter().enumerate() {
            log().info(format!("Downloading Hanime1 {} ({}/{total})", item.id, index + 1));
            let result = match self.download_item(&item).await {
                Ok(result) => result,
                Err(ItemError::Ignored(message)) => {
                    log().info(message);
                    continue;
                }
                Err(ItemError::Failed(error)) => {
                    log().error(format!("Failed to download Hanime1 item {}: {}", item.id, error.formatted()));
                    continue;
                }
            };
            let uploader = result
                .uploader
                .clone()
                .filter(|uploader| !uploader.is_empty())
                .or_else(|| item.uploader.clone().filter(|uploader| !uploader.is_empty()))
                .unwrap_or_default();
            execute(
                &self.db,
                "INSERT INTO hanime1 (id, title, uploader, release_date, plot) VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
                vec![
                    Param::from(result.item_id.as_str()),
                    Param::from(result.title.as_str()),
                    Param::from(uploader),
                    Param::from(result.release_date.clone().unwrap_or_default()),
                    Param::from(result.plot.clone().unwrap_or_default()),
                ],
            )
            .await?;
            item.title.clone_from(&result.title);
            if result.uploader.as_deref().is_some_and(|uploader| !uploader.is_empty()) {
                item.uploader.clone_from(&result.uploader);
            }
            let file_size_bytes = match tokio::fs::metadata(&result.final_path).await {
                Ok(metadata) => Some(metadata.len()),
                Err(error) => {
                    log().debug(format!(
                        "Failed to read file size for {}: {}",
                        result.final_path.display(),
                        crate::kit::py_os_error_text(&error)
                    ));
                    None
                }
            };
            self.notify_download(
                &item,
                result.resolution.as_deref(),
                file_size_bytes,
                result.release_date.as_deref(),
                result.cover_url.as_deref(),
            )
            .await?;
            log().notice(format!(
                "Saved {}",
                result.final_path.file_name().map(|name| name.to_string_lossy()).unwrap_or_default()
            ));
        }
        Ok(())
    }

    /// `update()`.
    pub(crate) async fn update(&self) -> Result<(), JobError> {
        self.discover_ranking_series().await?;
        let seeds = self.load_runtime_series_seeds().await?;
        let items = self.get_items(&seeds).await?;
        if !seeds.is_empty() {
            self.resolve_parser_failure_notification().await;
        }
        if items.is_empty() {
            log().info(if seeds.is_empty() {
                "No Hanime1 series seeds configured in database"
            } else {
                "No new series videos"
            });
        } else {
            create_dir_all(Path::new(self.cfg.path.as_str())).await?;
            log().info(format!("Found {} new series videos", items.len()));
            self.download_and_record(items).await?;
        }

        // Author enumeration never fails the job after the series route ran; per-author
        // errors land in `last_scan_error`.
        let author_items = match self.get_author_items().await {
            Ok(items) => items,
            Err(error) => {
                log().error(format!("Failed to enumerate Hanime1 author subscriptions: {}", error.formatted()));
                Vec::new()
            }
        };
        if !author_items.is_empty() {
            create_dir_all(Path::new(self.cfg.path.as_str())).await?;
            log().info(format!("Found {} new author videos", author_items.len()));
            self.download_and_record(author_items).await?;
        }
        Ok(())
    }
}

/// One `yt-dlp` attempt's failure: `DownloadError` (retried) or anything else.
enum DownloadAttempt {
    Retryable(String),
    Fatal(JobError),
}

/// A resolved series' videos as candidates, titled for the archive: the playlist's own
/// entries, or the bare ids numbered in order when it listed none. Returns the series
/// size and how many were new.
fn add_series_candidates(
    collected: &mut Candidates,
    source_name: &str,
    candidate_ids: &[String],
    videos: Vec<WatchSeriesVideo>,
) -> (i64, usize) {
    let videos = if videos.is_empty() {
        candidate_ids
            .iter()
            .zip(1..)
            .map(|(video_id, position)| WatchSeriesVideo {
                video_id: video_id.clone(),
                title: None,
                position,
            })
            .collect()
    } else {
        videos
    };
    let total = i64::try_from(videos.len()).unwrap_or(i64::MAX);
    let use_item_titles = playlist_item_titles_are_distinct(&videos);
    let mut new_count = 0;
    for video in &videos {
        let candidate = Candidate {
            video_id: video.video_id.clone(),
            source_name: source_name.to_owned(),
            archive_title: format_playlist_archive_title(Some(source_name), video, total, use_item_titles),
        };
        if collected.insert(candidate) {
            new_count += 1;
        }
    }
    (total, new_count)
}

/// `_cleanup_dir(dirpath)`: empty the directory.
async fn cleanup_dir(dirpath: &Path) -> Result<(), JobError> {
    let mut entries = match tokio::fs::read_dir(dirpath).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(os_error(&error, Some(dirpath))),
    };
    while let Some(entry) = entries.next_entry().await.map_err(|error| os_error(&error, Some(dirpath)))? {
        let path = entry.path();
        let is_dir = entry.file_type().await.map_err(|error| os_error(&error, Some(&path)))?.is_dir();
        let removed = if is_dir {
            tokio::fs::remove_dir_all(&path).await
        } else {
            tokio::fs::remove_file(&path).await
        };
        removed.map_err(|error| os_error(&error, Some(&path)))?;
    }
    Ok(())
}

async fn create_dir_all(dir: &Path) -> Result<(), JobError> {
    tokio::fs::create_dir_all(dir).await.map_err(|error| os_error(&error, Some(dir)))
}

fn remove_first(periods: &mut Vec<&'static str>, period: &str) {
    if let Some(index) = periods.iter().position(|item| *item == period) {
        periods.remove(index);
    }
}

fn count_f64(count: usize) -> f64 {
    // Counts of series found in one run: far below 2^52.
    #[allow(clippy::cast_precision_loss)] // exact for any count a run can reach
    let value = count as f64;
    value
}

/// `str(row.get(name) or '').strip()` for a text column.
fn text(row: &sqlx::postgres::PgRow, name: &str) -> Result<String, JobError> {
    let value: Option<String> = row.try_get(name).map_err(|error| pyerr::db_error(&fav_db::DbError::Sqlx(error)))?;
    Ok(py_strip(value.as_deref().unwrap_or_default()).to_owned())
}

#[cfg(test)]
mod tests;
