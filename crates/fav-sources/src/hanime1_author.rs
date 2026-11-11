//! Hanime1 author subscriptions: the parsers for an author's "uploaded" page and the
//! service the API manages `hanime1_author` rows with.
//!
//! Python origin: `src/tool/hanime1_author.py`. The worker's enumeration of a subscribed
//! author's pages is in [`crate::hanime1`]. The service reads `web.hanime1.host` and
//! `user_lang` on every call, where Python's API read them once at startup
//! (DIVERGENCES.md D78).

use std::sync::{Arc, LazyLock};

use chrono::{DateTime, Utc};
use fav_core::pyhtml::unescape;
use fav_core::pytext::{py_isdecimal, py_strip};
use fav_core::pyurl::urlsplit;
use fav_core::validate::LiteralStr;
use fav_db::{Db, Param, SettingsStore};
use fav_net::HttpClient;
use fav_runtime::BoxFuture;
use fav_runtime::source::JobError;
use sqlx::Row;

use crate::hanime1_series::{PageError, ServiceError, client_options, fetch_page, value_error};
use crate::opencc::to_simplified_chinese;
use crate::pyre::{self, compile, int_text};

static DISPLAY_NAME: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<h1[^>]+class=["'][^"']*profile-display-name[^"']*["'][^>]*>(?P<name>.*?)</h1>"#,
        true,
        true,
    )
});
static PAGE_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| compile(r"<title[^>]*>(?P<name>.*?)\s*-\s*影片\s*-", true, true));
static VIDEO_LINK: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<a\b(?P<attrs>[^>]*\bclass=["'][^"']*\bvideo-link\b[^"']*["'][^>]*)>(?P<body>.*?)</a>"#,
        true,
        true,
    )
});
static HREF: LazyLock<regex::Regex> = LazyLock::new(|| compile(r#"href=["'](?P<url>[^"']+)["']"#, true, false));
static ENTRY_TITLE: LazyLock<regex::Regex> = LazyLock::new(|| {
    compile(
        r#"<div[^>]+class=["'][^"']*\btitle\b[^"']*["'][^>]*>(?P<title>.*?)</div>"#,
        true,
        true,
    )
});

/// One video of an author's listing: `Hanime1AuthorVideoEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hanime1AuthorVideoEntry {
    pub video_id: String,
    pub title: Option<String>,
}

/// `_clean_fragment_text(fragment)`: entities, then tags, then whitespace runs.
fn clean_fragment_text(fragment: &str) -> Option<String> {
    let text = unescape(fragment);
    let text = pyre::strip_tags(&text);
    let text = py_strip(&pyre::collapse_whitespace(&text)).to_owned();
    (!text.is_empty()).then_some(text)
}

/// `extract_author_display_name(page_html)`: the profile heading, else the page title.
pub fn extract_author_display_name(page_html: &str) -> Option<String> {
    if let Some(captures) = DISPLAY_NAME.captures(page_html)
        && let Some(name) = clean_fragment_text(&captures["name"])
    {
        return Some(name);
    }
    clean_fragment_text(&PAGE_TITLE.captures(page_html)?["name"])
}

/// `_extract_watch_video_id(url)`: the first `v` of a watch link, as written, when it is
/// a positive decimal.
fn watch_video_id(url: &str) -> Result<Option<String>, JobError> {
    let query = urlsplit(&unescape(url), "").map_err(|error| value_error(error.0))?.query;
    let Some(value) = pyre::query_first(&query, "v") else {
        return Ok(None);
    };
    let video_id = py_strip(&value);
    if !py_isdecimal(video_id) || !pyre::is_positive_decimal(video_id)? {
        return Ok(None);
    }
    Ok(Some(video_id.to_owned()))
}

/// `extract_author_video_entries(page_html)`: the listing's `video-link` cards in page
/// order, each id once.
pub fn extract_author_video_entries(page_html: &str) -> Result<Vec<Hanime1AuthorVideoEntry>, JobError> {
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for link in VIDEO_LINK.captures_iter(page_html) {
        let Some(href) = HREF.captures(&link["attrs"]) else {
            continue;
        };
        let Some(video_id) = watch_video_id(&href["url"])? else {
            continue;
        };
        if !seen.insert(video_id.clone()) {
            continue;
        }
        let title = ENTRY_TITLE
            .captures(&link["body"])
            .and_then(|title| clean_fragment_text(&title["title"]));
        entries.push(Hanime1AuthorVideoEntry { video_id, title });
    }
    Ok(entries)
}

/// `extract_hanime1_author_id(raw)`: a bare positive id (as written) or the segment after
/// `user` in a URL path.
pub fn extract_hanime1_author_id(raw: &str) -> Result<Option<String>, JobError> {
    let text = py_strip(raw);
    if text.is_empty() {
        return Ok(None);
    }
    if py_isdecimal(text) {
        return Ok(pyre::is_positive_decimal(text)?.then(|| text.to_owned()));
    }
    let path = urlsplit(text, "").map_err(|error| value_error(error.0))?.path;
    let segments: Vec<&str> = path.split('/').filter(|segment| !segment.is_empty()).collect();
    for (index, segment) in segments.iter().enumerate() {
        if *segment != "user" || index + 1 >= segments.len() {
            continue;
        }
        let candidate = segments[index + 1];
        if py_isdecimal(candidate) && pyre::is_positive_decimal(candidate)? {
            return Ok(Some(candidate.to_owned()));
        }
    }
    Ok(None)
}

/// `{host}/user/{id}/uploaded`.
pub fn author_page_url(host: &str, author_id: &str) -> String {
    format!("{}/user/{author_id}/uploaded", host.trim_end_matches('/'))
}

/// A subscription `add_author` created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedAuthor {
    pub author_id: String,
    pub name: String,
    pub author_url: String,
}

/// One row of `list_authors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorDetail {
    pub author_id: String,
    pub name: String,
    pub video_count: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_scanned_at: Option<DateTime<Utc>>,
    pub last_scan_error: String,
    pub author_url: String,
}

/// What the API calls; tests put a fake behind it.
pub trait AuthorSubscriptions: Send + Sync {
    fn add<'a>(&'a self, raw_author: &'a str) -> BoxFuture<'a, Result<AddedAuthor, ServiceError>>;
    fn list(&self) -> BoxFuture<'_, Result<Vec<AuthorDetail>, ServiceError>>;
    fn delete<'a>(&'a self, author_id: &'a str) -> BoxFuture<'a, Result<bool, ServiceError>>;
}

/// `Hanime1AuthorService`.
pub struct Hanime1AuthorService {
    db: Db,
    settings: Arc<SettingsStore>,
    http: HttpClient,
}

impl std::fmt::Debug for Hanime1AuthorService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hanime1AuthorService").finish_non_exhaustive()
    }
}

impl Hanime1AuthorService {
    /// `httpx.Client(timeout=40, follow_redirects=True)`.
    pub fn new(db: Db, settings: Arc<SettingsStore>) -> Result<Self, String> {
        let http = HttpClient::new(&client_options(true)).map_err(|error| error.to_string())?;
        Ok(Self::with_client(db, settings, http))
    }

    pub fn with_client(db: Db, settings: Arc<SettingsStore>, http: HttpClient) -> Self {
        Self { db, settings, http }
    }

    async fn host_and_lang(&self) -> Result<(String, String), ServiceError> {
        let settings = self.settings.load().await.map_err(|error| ServiceError::Other(error.to_string()))?;
        let section = &settings.web.hanime1;
        Ok((section.host.trim_end_matches('/').to_owned(), section.user_lang.as_str().to_owned()))
    }

    /// `add_author(raw_author)`.
    pub async fn add_author(&self, raw_author: &str) -> Result<AddedAuthor, ServiceError> {
        let Ok(Some(author_id)) = extract_hanime1_author_id(raw_author) else {
            return Err(ServiceError::Invalid);
        };
        let (host, user_lang) = self.host_and_lang().await?;
        let author_url = author_page_url(&host, &author_id);
        let display_name = match fetch_page(&self.http, &author_url, &host, &user_lang).await {
            Ok(page) => extract_author_display_name(&page),
            Err(PageError::Http(_) | PageError::Blocked) => None,
        };
        let Some(display_name) = display_name else {
            return Err(ServiceError::ResolveFailed);
        };
        let name = to_simplified_chinese(Some(&display_name)).unwrap_or(display_name);
        let rows = crate::execute(
            &self.db,
            "INSERT INTO hanime1_author (author_id, name) VALUES ($1, $2) ON CONFLICT (author_id) DO NOTHING RETURNING author_id",
            vec![Param::from(author_id.as_str()), Param::from(name.as_str())],
        )
        .await
        .map_err(|error| ServiceError::Other(error.formatted()))?;
        if rows.is_empty() {
            return Err(ServiceError::Duplicate);
        }
        Ok(AddedAuthor {
            author_id,
            name,
            author_url,
        })
    }

    /// `list_authors()`.
    pub async fn list_authors(&self) -> Result<Vec<AuthorDetail>, ServiceError> {
        let (host, _) = self.host_and_lang().await?;
        let rows = crate::execute(
            &self.db,
            "SELECT author_id, name, video_count, created_at, updated_at, last_scanned_at, last_scan_error \
             FROM hanime1_author ORDER BY created_at, author_id",
            Vec::new(),
        )
        .await
        .map_err(|error| ServiceError::Other(error.formatted()))?;
        rows.iter()
            .map(|row| {
                let author_id: String = row.try_get("author_id")?;
                Ok(AuthorDetail {
                    author_url: author_page_url(&host, &author_id),
                    author_id,
                    name: row.try_get("name")?,
                    video_count: i64::from(row.try_get::<i32, _>("video_count")?),
                    created_at: row.try_get("created_at")?,
                    updated_at: row.try_get("updated_at")?,
                    last_scanned_at: row.try_get("last_scanned_at")?,
                    last_scan_error: row.try_get("last_scan_error")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(|error| ServiceError::Other(error.to_string()))
    }

    /// `delete_author(author_id)`: whether a row was deleted.
    pub async fn delete_author(&self, author_id: &str) -> Result<bool, ServiceError> {
        let normalized = py_strip(author_id);
        if !py_isdecimal(normalized) {
            return Err(ServiceError::Invalid);
        }
        let Ok(normalized) = int_text(normalized) else {
            return Err(ServiceError::Invalid);
        };
        if normalized == "0" {
            return Err(ServiceError::Invalid);
        }
        let rows = crate::execute(
            &self.db,
            "DELETE FROM hanime1_author WHERE author_id = $1 RETURNING author_id",
            vec![Param::from(normalized)],
        )
        .await
        .map_err(|error| ServiceError::Other(error.formatted()))?;
        Ok(!rows.is_empty())
    }
}

impl AuthorSubscriptions for Hanime1AuthorService {
    fn add<'a>(&'a self, raw_author: &'a str) -> BoxFuture<'a, Result<AddedAuthor, ServiceError>> {
        Box::pin(self.add_author(raw_author))
    }

    fn list(&self) -> BoxFuture<'_, Result<Vec<AuthorDetail>, ServiceError>> {
        Box::pin(self.list_authors())
    }

    fn delete<'a>(&'a self, author_id: &'a str) -> BoxFuture<'a, Result<bool, ServiceError>> {
        Box::pin(self.delete_author(author_id))
    }
}

#[cfg(test)]
mod tests;
