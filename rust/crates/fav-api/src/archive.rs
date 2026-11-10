//! Read-only browsing of the per-source archive tables.
//!
//! Python origin: `src/api/archive.py`. Every table, column and sort order comes from the
//! [`ARCHIVE_SOURCES`] whitelist; client input only reaches PostgreSQL as a bound
//! parameter. `rust/goldens/api/archive.json` holds the whitelist and Python's
//! serialization of typed rows.

use chrono::{DateTime, NaiveDateTime, Utc};
use fav_core::pyjson::{Dict, Value};
use fav_core::settings::Settings;
use fav_db::Db;
use sqlx::postgres::PgRow;
use sqlx::{AssertSqlSafe, Column as _, Row as _, TypeInfo as _};

use crate::json;

pub const MAX_LIMIT: i64 = 200;
pub const DEFAULT_LIMIT: i64 = 50;

/// One browsable source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveSource {
    pub key: &'static str,
    pub name: &'static str,
    pub table: &'static str,
    pub id_columns: &'static [&'static str],
    pub title_column: &'static str,
    pub columns: &'static [&'static str],
    pub search_columns: &'static [&'static str],
    pub time_column: &'static str,
    pub subtitle_columns: &'static [&'static str],
}

impl ArchiveSource {
    /// Id columns, title, the other columns and the time column, each once, in that order.
    pub fn selected_columns(&self) -> Vec<&'static str> {
        let mut seen: Vec<&'static str> = Vec::new();
        let all = self
            .id_columns
            .iter()
            .chain(std::iter::once(&self.title_column))
            .chain(self.columns.iter())
            .chain(std::iter::once(&self.time_column));
        for column in all {
            if !column.is_empty() && !seen.contains(column) {
                seen.push(column);
            }
        }
        seen
    }
}

/// `ARCHIVE_SOURCES`, in Python's order.
pub const ARCHIVE_SOURCES: [ArchiveSource; 11] = [
    ArchiveSource {
        key: "bilibili",
        name: "Bilibili",
        table: "bilibili",
        id_columns: &["bvid"],
        title_column: "title",
        columns: &["upper", "fav_id"],
        search_columns: &["bvid", "title", "upper"],
        time_column: "created_at",
        subtitle_columns: &["upper"],
    },
    ArchiveSource {
        key: "telegram",
        name: "Telegram",
        table: "telegram",
        id_columns: &["account_name", "channel_id", "message_id"],
        title_column: "title",
        columns: &["channel_name", "media_type"],
        search_columns: &["title", "channel_name"],
        time_column: "created_at",
        subtitle_columns: &["channel_name", "media_type"],
    },
    ArchiveSource {
        key: "hanime1",
        name: "Hanime1",
        table: "hanime1",
        id_columns: &["id"],
        title_column: "title",
        columns: &["uploader", "release_date"],
        search_columns: &["id", "title", "uploader"],
        time_column: "created_at",
        subtitle_columns: &["uploader", "release_date"],
    },
    ArchiveSource {
        key: "jandan",
        name: "Jandan",
        table: "jandan",
        id_columns: &["fav_type", "pic_id", "content_index"],
        title_column: "author",
        columns: &["content_url", "post_date", "local_path", "downloaded", "unavailable", "last_error"],
        search_columns: &["author", "content_url"],
        time_column: "created_at",
        subtitle_columns: &["post_date"],
    },
    ArchiveSource {
        key: "twitter",
        name: "X",
        table: "twitter",
        id_columns: &["tweet_id", "num"],
        title_column: "content",
        columns: &["author", "author_nick", "tweet_date", "media_type", "local_path"],
        search_columns: &["author", "author_nick", "content"],
        time_column: "created_at",
        subtitle_columns: &["author", "tweet_date"],
    },
    ArchiveSource {
        key: "rednote",
        name: "RedNote",
        table: "rednote",
        id_columns: &["note_id", "media_index"],
        title_column: "title",
        // xsec_token is not decoration: without it the note only opens for its author.
        columns: &[
            "author",
            "media_type",
            "published_at",
            "local_path",
            "downloaded",
            "unavailable",
            "last_error",
            "xsec_token",
        ],
        search_columns: &["note_id", "title", "author"],
        time_column: "created_at",
        subtitle_columns: &["author", "published_at"],
    },
    ArchiveSource {
        key: "pixiv",
        name: "Pixiv",
        table: "pixiv",
        id_columns: &["illust_id", "num"],
        title_column: "title",
        columns: &[
            "author",
            "author_id",
            "illust_type",
            "local_path",
            "downloaded",
            "unavailable",
            "last_error",
        ],
        search_columns: &["title", "author"],
        time_column: "created_at",
        subtitle_columns: &["author"],
    },
    ArchiveSource {
        key: "wechat",
        name: "WeChat",
        table: "wechat_inbox",
        id_columns: &["msg_id", "create_time"],
        title_column: "local_path",
        columns: &["sender", "sent_at", "size"],
        search_columns: &["local_path", "sender"],
        time_column: "created_at",
        subtitle_columns: &["sent_at"],
    },
    ArchiveSource {
        key: "wechat_article",
        name: "WeChat articles",
        table: "wechat_article",
        id_columns: &["msg_id", "create_time"],
        title_column: "title",
        columns: &["account", "sender", "sent_at", "published_at", "image_count", "local_path", "url"],
        search_columns: &["title", "account", "url"],
        time_column: "created_at",
        subtitle_columns: &["account", "sent_at"],
    },
    ArchiveSource {
        key: "wechat_channel_video",
        name: "WeChat Channels",
        table: "wechat_channel_video",
        id_columns: &["msg_id", "create_time"],
        title_column: "title",
        columns: &["author", "sender", "sent_at", "duration", "size", "local_path", "object_id"],
        search_columns: &["title", "author", "object_id"],
        time_column: "created_at",
        subtitle_columns: &["author", "sent_at"],
    },
    ArchiveSource {
        key: "kemono",
        name: "Kemono",
        table: "kemono",
        id_columns: &["id", "service", "user_id"],
        title_column: "title",
        columns: &["creator", "service", "published_at"],
        search_columns: &["id", "title", "creator"],
        time_column: "created_at",
        subtitle_columns: &["creator", "service"],
    },
];

/// The whitelist entry for a key.
pub fn source(key: &str) -> Option<&'static ArchiveSource> {
    ARCHIVE_SOURCES.iter().find(|source| source.key == key)
}

/// The keys, sorted (the `allowed_values` of an unknown source).
pub fn sorted_keys() -> Vec<&'static str> {
    let mut keys: Vec<&str> = ARCHIVE_SOURCES.iter().map(|source| source.key).collect();
    keys.sort_unstable();
    keys
}

/// One row as `dict_row` returns it: column names and values, timestamps already as
/// Python's `isoformat()`.
pub type Row = Vec<(String, Value)>;

fn row_get<'a>(row: &'a Row, column: &str) -> Option<&'a Value> {
    row.iter().find(|(name, _)| name == column).map(|(_, value)| value)
}

/// `str(value)`.
fn py_str(value: Option<&Value>) -> String {
    fav_runtime::pystr::str_value(value.unwrap_or(&Value::Null))
}

/// `build_note_url` (`src/web/rednote.py`): the source's own.
pub use fav_sources::rednote::build_note_url;

/// `_EXTERNAL_URL_BUILDERS`: the item's page on its site, where there is one.
fn external_url(source: &ArchiveSource, row: &Row, settings: &Settings) -> Option<String> {
    let get = |column: &str| row_get(row, column);
    let truthy = |column: &str| get(column).is_some_and(Value::is_truthy);
    match source.key {
        "bilibili" => Some(format!("https://www.bilibili.com/video/{}", py_str(get("bvid")))),
        "hanime1" => Some(format!(
            "{}/watch?v={}",
            settings.web.hanime1.host.trim_end_matches('/'),
            py_str(get("id"))
        )),
        "kemono" => Some(format!(
            "{}/{}/user/{}/post/{}",
            settings.web.kemono.base_url.trim_end_matches('/'),
            py_str(get("service")),
            py_str(get("user_id")),
            py_str(get("id"))
        )),
        "jandan" => truthy("content_url").then(|| py_str(get("content_url"))),
        // /i/status resolves without knowing the author's current handle.
        "twitter" => Some(format!("https://x.com/i/status/{}", py_str(get("tweet_id")))),
        "pixiv" => Some(format!("https://www.pixiv.net/artworks/{}", py_str(get("illust_id")))),
        "rednote" => {
            let token = if truthy("xsec_token") {
                py_str(get("xsec_token"))
            } else {
                String::new()
            };
            Some(build_note_url(&py_str(get("note_id")), &token))
        }
        // The canonical link, without the sharer's tracking parameters.
        "wechat_article" => truthy("url").then(|| py_str(get("url"))),
        _ => None,
    }
}

/// `_serialize`: one row as an `ArchiveItem`.
pub fn serialize(source: &ArchiveSource, row: &Row, settings: &Settings) -> Value {
    let id = source
        .id_columns
        .iter()
        .map(|column| py_str(row_get(row, column)))
        .collect::<Vec<_>>()
        .join(":");
    let title = row_get(row, source.title_column)
        .filter(|value| value.is_truthy())
        .map_or_else(String::new, |value| py_str(Some(value)));
    let subtitle = source
        .subtitle_columns
        .iter()
        .filter_map(|column| row_get(row, column).filter(|value| value.is_truthy()))
        .map(|value| fav_core::pytext::py_strip(&py_str(Some(value))).to_owned())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
    let created_at = row_get(row, source.time_column).cloned().unwrap_or(Value::Null);
    let mut fields = Dict::new();
    for (column, value) in row {
        if column != source.time_column {
            fields.insert(column.as_str(), value.clone());
        }
    }
    json::dict([
        ("source", Value::Str(source.key.to_owned())),
        ("id", Value::Str(id)),
        ("title", Value::Str(title)),
        ("subtitle", Value::Str(subtitle)),
        ("created_at", created_at),
        ("url", Value::from(external_url(source, row, settings))),
        ("fields", Value::Dict(fields)),
    ])
}

/// One column of a fetched row, as psycopg would hand it to Python.
fn decode_cell(row: &PgRow, index: usize) -> Result<Value, sqlx::Error> {
    let type_name = row.columns()[index].type_info().name().to_owned();
    Ok(match type_name.as_str() {
        "INT2" => Value::from(row.try_get::<Option<i16>, _>(index)?.map(i64::from)),
        "INT4" => Value::from(row.try_get::<Option<i32>, _>(index)?.map(i64::from)),
        "INT8" => Value::from(row.try_get::<Option<i64>, _>(index)?),
        "BOOL" => Value::from(row.try_get::<Option<bool>, _>(index)?),
        "FLOAT4" => Value::from(row.try_get::<Option<f32>, _>(index)?.map(f64::from)),
        "FLOAT8" => Value::from(row.try_get::<Option<f64>, _>(index)?),
        "TIMESTAMP" => Value::from(
            row.try_get::<Option<NaiveDateTime>, _>(index)?
                .map(|value| json::isoformat_naive(&value)),
        ),
        "TIMESTAMPTZ" => Value::from(
            row.try_get::<Option<DateTime<Utc>>, _>(index)?
                .map(|value| json::isoformat_utc(&value)),
        ),
        _ => Value::from(row.try_get::<Option<String>, _>(index)?),
    })
}

fn decode_row(row: &PgRow) -> Result<Row, sqlx::Error> {
    (0..row.columns().len())
        .map(|index| Ok((row.columns()[index].name().to_owned(), decode_cell(row, index)?)))
        .collect()
}

fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn table_exists(db: &Db, table: &str) -> Result<bool, sqlx::Error> {
    let oid: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text AS table_oid;")
        .bind(table)
        .fetch_one(db.pool())
        .await?;
    Ok(oid.is_some())
}

/// `ArchiveLibrary.list_sources`: each source with its row count and newest row.
pub async fn list_sources(db: &Db) -> Result<Vec<Value>, sqlx::Error> {
    let mut stats = Vec::new();
    for source in &ARCHIVE_SOURCES {
        let (total, latest_at) = if table_exists(db, source.table).await? {
            let row = sqlx::query(AssertSqlSafe(format!(
                "SELECT COUNT(*) AS total, MAX({}) AS latest_at FROM {};",
                ident(source.time_column),
                ident(source.table)
            )))
            .fetch_one(db.pool())
            .await?;
            (row.try_get::<i64, _>("total")?, decode_cell(&row, 1)?)
        } else {
            (0, Value::Null)
        };
        stats.push(json::dict([
            ("source", Value::Str(source.key.to_owned())),
            ("name", Value::Str(source.name.to_owned())),
            ("total", Value::from(total)),
            ("latest_at", latest_at),
        ]));
    }
    Ok(stats)
}

/// `ArchiveLibrary.list_items`: one page of a source, newest first, optionally filtered by
/// a case-insensitive substring of its search columns. `offset` beyond 64 bits is clamped
/// (Python passes it on and PostgreSQL refuses it, DIVERGENCES.md).
pub async fn list_items(
    db: &Db,
    settings: &Settings,
    source: &ArchiveSource,
    query: &str,
    limit: i64,
    offset: i64,
) -> Result<Value, sqlx::Error> {
    let bounded_limit = limit.clamp(1, MAX_LIMIT);
    let bounded_offset = offset.max(0);
    let normalized_query = fav_core::pytext::py_strip(query);
    let page = |items: Vec<Value>, total: i64| {
        json::dict([
            ("items", Value::List(items)),
            ("total", Value::from(total)),
            ("limit", Value::from(bounded_limit)),
            ("offset", Value::from(bounded_offset)),
        ])
    };
    if !table_exists(db, source.table).await? {
        return Ok(page(Vec::new(), 0));
    }

    let mut where_sql = String::new();
    if !normalized_query.is_empty() {
        let clauses: Vec<String> = source
            .search_columns
            .iter()
            .enumerate()
            .map(|(position, column)| format!("{}::text ILIKE ${}", ident(column), position + 1))
            .collect();
        where_sql = format!(" WHERE ({})", clauses.join(" OR "));
    }
    let pattern = format!("%{normalized_query}%");
    let params = if normalized_query.is_empty() {
        0
    } else {
        source.search_columns.len()
    };
    let table = ident(source.table);

    let mut count = sqlx::query_scalar::<_, i64>(AssertSqlSafe(format!("SELECT COUNT(*) AS total FROM {table}{where_sql};")));
    for _ in 0..params {
        count = count.bind(pattern.clone());
    }
    let total = count.fetch_one(db.pool()).await?;

    let columns: Vec<String> = source.selected_columns().iter().map(|column| ident(column)).collect();
    let mut order = vec![format!("{} DESC NULLS LAST", ident(source.time_column))];
    order.extend(source.id_columns.iter().map(|column| format!("{} DESC", ident(column))));
    let sql = format!(
        "SELECT {} FROM {table}{where_sql} ORDER BY {} LIMIT ${} OFFSET ${};",
        columns.join(", "),
        order.join(", "),
        params + 1,
        params + 2
    );
    let mut select = sqlx::query(AssertSqlSafe(sql));
    for _ in 0..params {
        select = select.bind(pattern.clone());
    }
    let rows = select.bind(bounded_limit).bind(bounded_offset).fetch_all(db.pool()).await?;
    let items = rows
        .iter()
        .map(|row| decode_row(row).map(|row| serialize(source, &row, settings)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(page(items, total))
}

#[cfg(test)]
mod tests {
    use fav_core::golden;
    use fav_core::pyjson::Value;

    use super::{ARCHIVE_SOURCES, Row, build_note_url, external_url, serialize, source};
    use crate::json;

    fn row_from_golden(value: &Value) -> Row {
        value
            .as_dict()
            .expect("a row")
            .iter()
            .map(|(column, cell)| {
                let cell = match cell.get("timestamp") {
                    Some(timestamp) => timestamp.clone(),
                    None => cell.clone(),
                };
                (fav_runtime::pystr::str_key(column), cell)
            })
            .collect()
    }

    #[test]
    fn whitelist_and_serialization_match_python() {
        let document = golden::load("api/archive.json");
        let cases = golden::cases(&document);
        let expected_sources = golden::list(golden::get(&cases[0], "sources"));
        assert_eq!(expected_sources.len(), ARCHIVE_SOURCES.len());
        for (expected, actual) in expected_sources.iter().zip(&ARCHIVE_SOURCES) {
            let actual = json::dict([
                ("key", Value::from(actual.key)),
                ("name", Value::from(actual.name)),
                ("table", Value::from(actual.table)),
                ("id_columns", json::str_list(actual.id_columns)),
                ("title_column", Value::from(actual.title_column)),
                ("columns", json::str_list(actual.columns)),
                ("search_columns", json::str_list(actual.search_columns)),
                ("time_column", Value::from(actual.time_column)),
                ("subtitle_columns", json::str_list(actual.subtitle_columns)),
                ("selected_columns", json::str_list(&actual.selected_columns())),
            ]);
            assert_eq!(&actual, expected);
        }
        let settings = fav_db::settings_store::default_settings();
        for case in &cases[1..] {
            if let Some(note_id) = case.get("note_id") {
                let url = build_note_url(&golden::string(note_id), &golden::string(golden::get(case, "xsec_token")));
                assert_eq!(url, golden::string(golden::get(case, "url")));
                continue;
            }
            let source = source(&golden::string(golden::get(case, "source"))).expect("a known source");
            let row = row_from_golden(golden::get(case, "row"));
            assert_eq!(serialize(source, &row, &settings), *golden::get(case, "item"), "{case:?}");
        }
    }

    // py: tests/test_pixiv.py::test_the_archive_links_a_row_back_to_the_artwork
    #[test]
    fn the_archive_links_a_pixiv_row_back_to_the_artwork() {
        let source = source("pixiv").expect("pixiv is archived");
        assert_eq!(source.table, "pixiv");
        let row: Row = vec![
            ("illust_id".to_owned(), Value::from(96_461_036_i64)),
            ("num".to_owned(), Value::from(0_i64)),
        ];
        let settings = fav_db::settings_store::default_settings();
        assert_eq!(
            external_url(source, &row, &settings).as_deref(),
            Some("https://www.pixiv.net/artworks/96461036")
        );
    }

    // py: tests/test_pixiv.py::test_api_job_enum_includes_pixiv
    #[test]
    fn api_job_enum_includes_pixiv() {
        assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"pixiv"));
    }

    // py: tests/test_rednote.py::test_the_archive_links_a_row_to_a_note_that_actually_opens
    #[test]
    fn the_archive_links_a_rednote_row_to_a_note_that_actually_opens() {
        let source = source("rednote").expect("rednote is archived");
        let row: Row = vec![
            ("note_id".to_owned(), Value::from("64f1a2b3000000001e02c0de")),
            ("media_index".to_owned(), Value::from(1_i64)),
            ("xsec_token".to_owned(), Value::from("TOKEN")),
        ];
        let settings = fav_db::settings_store::default_settings();
        // Without the token the note is only readable by its author.
        assert_eq!(
            external_url(source, &row, &settings).as_deref(),
            Some("https://www.xiaohongshu.com/explore/64f1a2b3000000001e02c0de?xsec_token=TOKEN&xsec_source=pc_user")
        );
        assert!(source.columns.contains(&"xsec_token"));
    }

    // py: tests/test_rednote.py::test_api_job_enum_includes_rednote
    #[test]
    fn api_job_enum_includes_rednote() {
        assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"rednote"));
    }

    // py: tests/test_twitter.py::test_the_archive_links_a_row_to_the_tweet_without_needing_the_handle
    #[test]
    fn the_archive_links_a_twitter_row_to_the_tweet_without_needing_the_handle() {
        let source = source("twitter").expect("twitter is archived");
        let row: Row = vec![
            ("tweet_id".to_owned(), Value::from(1234_i64)),
            ("num".to_owned(), Value::from(1_i64)),
        ];
        let settings = fav_db::settings_store::default_settings();
        assert_eq!(
            external_url(source, &row, &settings).as_deref(),
            Some("https://x.com/i/status/1234")
        );
    }

    // py: tests/test_twitter.py::test_api_job_enum_includes_twitter
    #[test]
    fn api_job_enum_includes_twitter() {
        assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"twitter"));
    }

    // py: tests/test_wechat.py::test_the_api_accepts_wechat_as_a_job_target
    #[test]
    fn the_api_accepts_wechat_as_a_job_target() {
        assert!(crate::handlers::JOB_REQUEST_TARGETS.contains(&"wechat"));
    }

    /// `{'msg_id': ..., 'create_time': 2025-10-05 03:13:20 UTC in ms}` and more columns.
    fn wechat_row(msg_id: i64, extra: &[(&str, &str)]) -> Row {
        let mut row: Row = vec![
            ("msg_id".to_owned(), Value::from(msg_id)),
            ("create_time".to_owned(), Value::from(1_759_634_000_000_i64)),
        ];
        row.extend(extra.iter().map(|(column, value)| ((*column).to_owned(), Value::from(*value))));
        row
    }

    // py: tests/test_wechat.py::test_the_archive_lists_wechat_rows_without_an_external_link
    #[test]
    fn the_archive_lists_wechat_rows_without_an_external_link() {
        let source = source("wechat").expect("wechat is archived");

        assert_eq!(source.id_columns, ["msg_id", "create_time"]);
        // The removed iLink source left a `wechat` table with another schema behind.
        assert_eq!(source.table, "wechat_inbox");
        let settings = fav_db::settings_store::default_settings();
        assert_eq!(external_url(source, &wechat_row(1, &[]), &settings), None);
    }

    // py: tests/test_wechat.py::test_the_archive_lists_channels_videos
    #[test]
    fn the_archive_lists_channels_videos() {
        let source = source("wechat_channel_video").expect("channels videos are archived");

        assert_eq!(source.table, "wechat_channel_video");
        assert_eq!(source.id_columns, ["msg_id", "create_time"]);
        let settings = fav_db::settings_store::default_settings();
        assert_eq!(external_url(source, &wechat_row(65, &[]), &settings), None);
    }

    // py: tests/test_wechat.py::test_the_archive_links_articles_to_their_canonical_url
    #[test]
    fn the_archive_links_articles_to_their_canonical_url() {
        let source = source("wechat_article").expect("articles are archived");
        let canonical = "https://mp.weixin.qq.com/s?__biz=MzA5==&mid=2247&idx=1&sn=abc123";

        assert_eq!(source.table, "wechat_article");
        assert_eq!(source.id_columns, ["msg_id", "create_time"]);
        let settings = fav_db::settings_store::default_settings();
        assert_eq!(
            external_url(source, &wechat_row(44, &[("url", canonical)]), &settings).as_deref(),
            Some(canonical)
        );
    }
}
