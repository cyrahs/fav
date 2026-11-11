//! The Telegram goldens (`805f5c0:rust/tools/goldens/areas/telegram.py`): message classification,
//! Telethon's download naming, its error texts and its `.session` file.

use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddrV4, SocketAddrV6};
use std::path::Path;

use fav_core::golden::{self, get, int, list, string};
use fav_core::pyjson::Value;
use grammers_client::session::Session as _;
use grammers_client::session::types::{ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, UpdateState, UpdatesState};
use sqlx::Row as _;

use super::client::{ErrorKind, TgError};
use super::files::{self, Extensions, Location};
use super::message::{Document, DocumentAttribute, Media, Photo, PhotoSize, TgMessage, WebPage, message_media_type, unsupported_reason};
use super::session::TelethonSession;
use crate::telegram_queue::JobType;

const CHANNEL: i64 = 123;

fn text_of(value: &Value, key: &str) -> Option<String> {
    value.get(key).filter(|value| !matches!(value, Value::Null)).map(string)
}

fn flag(value: &Value, key: &str) -> bool {
    matches!(value.get(key), Some(Value::Bool(true)))
}

fn number(value: &Value, key: &str, default: i64) -> i64 {
    value.get(key).map_or(default, |value| i64::try_from(int(value)).expect("fits"))
}

fn hex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("hex"))
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn attribute(spec: &Value) -> DocumentAttribute {
    match string(get(spec, "kind")).as_str() {
        "video" => DocumentAttribute::Video {
            round_message: flag(spec, "round"),
        },
        "sticker" => DocumentAttribute::Sticker,
        "custom_emoji" => DocumentAttribute::CustomEmoji,
        "filename" => DocumentAttribute::Filename(string(get(spec, "name"))),
        "audio" => DocumentAttribute::Audio {
            voice: flag(spec, "voice"),
            performer: text_of(spec, "performer"),
            title: text_of(spec, "title"),
        },
        "animated" => DocumentAttribute::Other,
        other => panic!("unknown attribute {other}"),
    }
}

fn size(spec: &Value) -> PhotoSize {
    let kind = string(get(spec, "type"));
    match string(get(spec, "kind")).as_str() {
        "size" => PhotoSize::Size {
            kind,
            size: number(spec, "size", 0),
        },
        "cached" => PhotoSize::Cached {
            kind,
            bytes: hex(&string(get(spec, "bytes"))),
        },
        "stripped" => PhotoSize::Stripped {
            kind,
            bytes: hex(&string(get(spec, "bytes"))),
        },
        "progressive" => PhotoSize::Progressive {
            kind,
            sizes: list(get(spec, "sizes"))
                .iter()
                .map(|size| i64::try_from(int(size)).expect("fits"))
                .collect(),
        },
        "path" => PhotoSize::Path { kind },
        "empty" => PhotoSize::Empty { kind },
        "video" => PhotoSize::Video {
            kind,
            size: number(spec, "size", 0),
        },
        other => panic!("unknown size {other}"),
    }
}

fn photo(spec: &Value) -> Photo {
    let sizes = spec.get("sizes").map_or_else(
        || {
            vec![PhotoSize::Size {
                kind: "y".to_owned(),
                size: 14,
            }]
        },
        |sizes| list(sizes).iter().map(size).collect(),
    );
    Photo {
        id: 7,
        access_hash: 8,
        file_reference: b"ref".to_vec(),
        dc_id: 2,
        sizes,
        video_sizes: spec
            .get("video_sizes")
            .map(|sizes| list(sizes).iter().map(size).collect())
            .unwrap_or_default(),
    }
}

fn document(spec: &Value) -> Document {
    Document {
        id: 9,
        access_hash: 10,
        file_reference: b"ref".to_vec(),
        dc_id: 2,
        size: number(spec, "size", 14),
        mime_type: text_of(spec, "mime").unwrap_or_default(),
        attributes: spec
            .get("attributes")
            .map(|attrs| list(attrs).iter().map(attribute).collect())
            .unwrap_or_default(),
    }
}

fn media(spec: &Value) -> Option<Media> {
    if matches!(spec, Value::Null) {
        return None;
    }
    Some(match string(get(spec, "kind")).as_str() {
        "photo" => Media::Photo(Some(photo(spec))),
        "photo_empty" => Media::Photo(None),
        "document" => Media::Document(Some(document(spec))),
        "document_empty" => Media::Document(None),
        "webpage" => Media::WebPage(WebPage {
            url: text_of(spec, "url").unwrap_or_default(),
            loaded: spec.get("loaded").is_none_or(|loaded| matches!(loaded, Value::Bool(true))),
            photo: spec.get("photo").map(photo),
            document: spec.get("document").map(document),
        }),
        "geo" => Media::Other("MessageMediaGeo".to_owned()),
        "dice" => Media::Other("MessageMediaDice".to_owned()),
        "unsupported" => Media::Other("MessageMediaUnsupported".to_owned()),
        other => panic!("unknown media {other}"),
    })
}

/// The message `_message(spec)` builds on the Python side.
fn message(spec: &Value) -> TgMessage {
    TgMessage {
        text: text_of(spec, "text").unwrap_or_default(),
        grouped_id: spec
            .get("grouped_id")
            .filter(|id| !matches!(id, Value::Null))
            .map(|id| i64::try_from(int(id)).expect("fits")),
        action: flag(spec, "action"),
        media: spec.get("media").and_then(media),
        ..TgMessage::new(number(spec, "id", 0), CHANNEL)
    }
}

#[test]
fn messages_are_classified_and_reported_as_python_does() {
    let document = golden::load("telegram/classification.json");
    for case in golden::cases(&document) {
        let message = message(get(case, "message"));
        let Value::Dict(expected) = get(case, "media_type") else {
            panic!("a dict")
        };
        for (types, want) in expected.iter() {
            let types = key_text(types);
            let types: Vec<JobType> = types.split(',').map(|kind| JobType::parse(kind).expect("a job type")).collect();
            let got = message_media_type(&message, &types).expect("classified").map(JobType::as_str);
            assert_eq!(got.map(str::to_owned), text_of_value(want), "message {} with {types:?}", message.id);
        }
        assert_eq!(unsupported_reason(&message), string(get(case, "reason")), "message {}", message.id);
    }
}

fn key_text(key: &fav_core::pyjson::Key) -> &str {
    match key {
        fav_core::pyjson::Key::Str(text) => text,
        other => panic!("not a string key: {other:?}"),
    }
}

fn text_of_value(value: &Value) -> Option<String> {
    (!matches!(value, Value::Null)).then(|| string(value))
}

#[test]
fn downloads_are_named_as_telethon_names_them() {
    let document = golden::load("telegram/downloads.json");
    let case = &golden::cases(&document)[0];
    let extensions = Extensions::defaults_with_telethon();
    for download in list(get(case, "downloads")) {
        let spec = get(download, "case");
        let existing: HashSet<String> = spec
            .get("existing")
            .map(|names| list(names).iter().map(string).collect())
            .unwrap_or_default();
        let folder = Path::new("/cache");
        let message = TgMessage {
            media: media(get(spec, "media")),
            ..TgMessage::new(10, CHANNEL)
        };
        let exists = |path: &Path| {
            path.parent() == Some(folder) && existing.contains(&path.file_name().expect("a name").to_string_lossy().into_owned())
        };
        let plan = files::plan(&message, &folder.join("file"), &extensions, &exists);
        let name = text_of_value(get(download, "name"));
        assert_eq!(
            plan.as_ref()
                .map(|plan| plan.path.file_name().expect("a name").to_string_lossy().into_owned()),
            name,
            "{spec:?}"
        );
        let Some(plan) = plan else { continue };
        match (&plan.location, download.get("request")) {
            (Location::Inline(bytes), Some(Value::Null) | None) => {
                assert_eq!(to_hex(bytes), string(get(download, "inline")), "{spec:?}");
            }
            (location, Some(request)) => {
                let (kind, thumb) = match location {
                    Location::Photo { thumb_size, .. } => ("InputPhotoFileLocation", thumb_size.clone()),
                    Location::Document { .. } => ("InputDocumentFileLocation", String::new()),
                    Location::Inline(_) => panic!("inline where Telethon downloads: {spec:?}"),
                };
                assert_eq!(kind, string(get(request, "location")), "{spec:?}");
                assert_eq!(thumb, string(get(request, "thumb")), "{spec:?}");
                assert_eq!(plan.size, Some(i64::try_from(int(get(request, "size"))).expect("fits")), "{spec:?}");
            }
            (location, None) => panic!("{location:?} for {spec:?}"),
        }
    }
    let Value::Dict(expected) = get(case, "extensions") else {
        panic!("a dict")
    };
    for (mime, want) in expected.iter() {
        let mime = key_text(mime);
        assert_eq!(
            extensions.document_extension(&Document::of(mime, Vec::new())),
            string(want),
            "{mime}"
        );
    }
}

#[test]
fn the_builtin_mimetypes_table_is_python_3_12s() {
    let document = golden::load("telegram/mimetypes_defaults.json");
    let expected: Vec<(String, String)> = golden::cases(&document)
        .iter()
        .map(|pair| (string(&list(pair)[0]), string(&list(pair)[1])))
        .collect();
    let ours: Vec<(String, String)> = super::mimetypes_defaults::DEFAULT_TYPES
        .iter()
        .map(|(ext, kind)| ((*ext).to_owned(), (*kind).to_owned()))
        .collect();
    assert_eq!(ours, expected);
}

/// Telegram's error answers, read by grammers, raise Telethon's class with Telethon's text:
/// the message is rebuilt from grammers' parts and looked up in Telethon's table.
#[test]
fn rpc_errors_read_as_telethon_raises_them() {
    let document = golden::load("telegram/errors.json");
    for case in golden::cases(&document) {
        let message = string(get(case, "message"));
        let code = i32::try_from(int(get(case, "code"))).expect("fits");
        let rpc = grammers_client::sender::RpcError::from(grammers_client::tl::types::RpcError {
            error_code: code,
            error_message: message.clone(),
        });
        let request = match (text_of(case, "request"), text_of(case, "inner")) {
            (Some(outer), Some(inner)) => Some(format!("{outer}({inner})")),
            (outer, _) => outer,
        };
        let error = TgError::rpc(rpc.code, &rpc.name, rpc.value, request.as_deref());
        assert_eq!(error.class, string(get(case, "class")), "{message}");
        assert_eq!(error.message, string(get(case, "text")), "{message}");
        let raised = super::client::rpc_message_to_error(code, &super::client::telegram_message(&rpc.name, rpc.value));
        assert_eq!(raised.server, flag(case, "server"), "{message}");
        if let Some(seconds) = case.get("seconds").filter(|value| !matches!(value, Value::Null)) {
            assert_eq!(raised.capture, Some(u64::try_from(int(seconds)).expect("fits")), "{message}");
        }
        match error.class.as_str() {
            // The wait the source reads is at least a second (`_call`).
            "FloodWaitError" => {
                let seconds = u32::try_from(int(get(case, "seconds"))).expect("fits").max(1);
                assert_eq!(error.kind, ErrorKind::FloodWait(seconds));
            }
            "MessageIdInvalidError" => assert_eq!(error.kind, ErrorKind::MessageIdInvalid),
            _ => assert_eq!(error.kind, ErrorKind::Rpc),
        }
    }
}

/// The error table is Telethon 1.43.2's, entry for entry and in its order.
#[test]
fn the_error_table_is_telethons() {
    let document = golden::load("telegram/rpc_errors.json");
    let (mut exact, mut patterns, mut base) = (Vec::new(), Vec::new(), Vec::new());
    for case in golden::cases(&document) {
        let class = string(get(case, "class"));
        let server = flag(case, "server");
        match string(get(case, "kind")).as_str() {
            "exact" => exact.push((string(get(case, "name")), class, string(get(case, "text")), server)),
            "pattern" => {
                let regex = string(get(case, "regex"));
                let (prefix, suffix) = regex.split_once("(\\d+)").expect("one number");
                patterns.push((prefix.to_owned(), suffix.to_owned(), class, string(get(case, "text")), server));
            }
            "base" => base.push((i32::try_from(int(get(case, "code"))).expect("fits"), class, server)),
            other => panic!("unexpected kind {other}"),
        }
    }
    let owned = |text: &str| text.to_owned();
    let ours: Vec<_> = super::rpc_errors::EXACT
        .iter()
        .map(|(name, class, text, server)| (owned(name), owned(class), owned(text), *server))
        .collect();
    assert_eq!(ours, exact);
    let ours: Vec<_> = super::rpc_errors::PATTERNS
        .iter()
        .map(|(prefix, suffix, class, text, server)| (owned(prefix), owned(suffix), owned(class), owned(text), *server))
        .collect();
    assert_eq!(ours, patterns);
    let ours: Vec<_> = super::rpc_errors::BASE
        .iter()
        .map(|(code, class, server)| (*code, owned(class), *server))
        .collect();
    assert_eq!(ours, base);
}

// ---------- the session file ----------

fn session_golden() -> Value {
    let document = golden::load("telegram/session.json");
    golden::cases(&document)[0].clone()
}

fn sqlite_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Int(number) => number.to_string(),
        Value::Str(text) if text.is_empty() => "X''".to_owned(),
        Value::Str(text) if text.len() == 512 && text.chars().all(|ch| ch.is_ascii_hexdigit()) => format!("X'{text}'"),
        Value::Str(text) => format!("'{}'", text.replace('\'', "''")),
        other => panic!("unexpected column {other:?}"),
    }
}

async fn sqlite(path: &Path) -> sqlx::SqlitePool {
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(path).create_if_missing(true);
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .expect("sqlite")
}

/// A file Telethon wrote is read for its home DC, its key, its peers and its update state.
#[tokio::test]
async fn a_session_telethon_wrote_is_read() {
    let golden = session_golden();
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("account.session");
    let pool = sqlite(&path).await;
    for sql in list(get(&golden, "schema")) {
        sqlx::query(sqlx::AssertSqlSafe(string(sql))).execute(&pool).await.expect("table");
    }
    let Value::Dict(tables) = get(&golden, "tables") else {
        panic!("a dict")
    };
    for (table, rows) in tables.iter() {
        let table = key_text(table);
        for row in list(rows) {
            let values: Vec<String> = list(row).iter().map(sqlite_value).collect();
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO {table} VALUES ({})", values.join(", "))))
                .execute(&pool)
                .await
                .expect("row");
        }
    }
    pool.close().await;

    let session = TelethonSession::open(&dir.path().join("account")).await.expect("opens");

    assert_eq!(session.home_dc_id().expect("home"), 4);
    let home = session.dc_option(4).expect("option").expect("the home DC");
    assert_eq!(home.ipv4, SocketAddrV4::new(Ipv4Addr::new(149, 154, 167, 91), 443));
    let key: Vec<u8> = (0..=255).collect();
    assert_eq!(home.auth_key.map(|key| key.to_vec()), Some(key));
    let me = session.peer(PeerId::self_user()).await.expect("peer").expect("the own user");
    assert_eq!((me.id(), me.auth().map(PeerAuth::hash)), (PeerId::user_unchecked(42), Some(999)));
    let channel = session
        .peer(PeerId::channel_unchecked(CHANNEL))
        .await
        .expect("peer")
        .expect("the channel");
    assert_eq!(channel.auth().map(PeerAuth::hash), Some(555));
    assert!(session.peer(PeerId::chat_unchecked(77)).await.expect("peer").is_some());
    let updates = session.updates_state().await.expect("state");
    assert_eq!((updates.pts, updates.qts, updates.date, updates.seq), (100, 3, 1_794_312_000, 7));
    assert_eq!(updates.channels, [ChannelState { id: CHANNEL, pts: 55 }]);
    session.close().await;
}

fn rows_of(rows: &[sqlx::sqlite::SqliteRow], columns: usize) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            (0..columns)
                .map(|index| {
                    if let Ok(Some(number)) = row.try_get::<Option<i64>, _>(index) {
                        number.to_string()
                    } else if let Ok(Some(text)) = row.try_get::<Option<String>, _>(index) {
                        format!("'{text}'")
                    } else if let Ok(Some(bytes)) = row.try_get::<Option<Vec<u8>>, _>(index) {
                        format!("X'{}'", to_hex(&bytes))
                    } else {
                        "NULL".to_owned()
                    }
                })
                .collect()
        })
        .collect()
}

fn golden_rows(golden: &Value, table: &str) -> Vec<Vec<String>> {
    list(get(get(golden, "tables"), table))
        .iter()
        .map(|row| {
            list(row)
                .iter()
                .map(|value| match value {
                    Value::Str(text) if text.len() == 512 || text.is_empty() => format!("X'{text}'"),
                    other => sqlite_value(other),
                })
                .collect()
        })
        .collect()
}

/// A session written here is the file Telethon writes for the same login: the schema
/// exactly, the home DC row, every peer's id and hash (Telethon also keeps a username
/// and name, D149) and the update state (a channel's date is any date, as in Telethon).
#[tokio::test]
async fn a_session_is_written_as_telethon_writes_it() {
    let golden = session_golden();
    let dir = tempfile::tempdir().expect("temp dir");
    let session = TelethonSession::open(&dir.path().join("account")).await.expect("creates");
    let key: [u8; 256] = std::array::from_fn(|index| u8::try_from(index).expect("a byte"));
    session.set_home_dc_id(4).await.expect("home");
    session
        .set_dc_option(&DcOption {
            id: 4,
            ipv4: SocketAddrV4::new(Ipv4Addr::new(149, 154, 167, 91), 443),
            ipv6: SocketAddrV6::new(std::net::Ipv6Addr::UNSPECIFIED, 443, 0, 0),
            auth_key: Some(key),
        })
        .await
        .expect("key");
    for peer in [
        PeerInfo::Channel {
            id: CHANNEL,
            auth: Some(PeerAuth::from_hash(555)),
            kind: None,
        },
        PeerInfo::Chat { id: 77 },
        PeerInfo::User {
            id: 42,
            auth: Some(PeerAuth::from_hash(999)),
            bot: Some(false),
            is_self: Some(true),
        },
    ] {
        session.cache_peer(peer).await.expect("peer");
    }
    session
        .set_update_state(UpdateState::All(UpdatesState {
            pts: 100,
            qts: 3,
            date: 1_794_312_000,
            seq: 7,
            channels: vec![ChannelState { id: CHANNEL, pts: 55 }],
        }))
        .await
        .expect("state");
    assert_eq!(super::session::peers(&session).len(), 3);
    session.close().await;

    let pool = sqlite(&dir.path().join("account.session")).await;
    let schema: Vec<String> = sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' ORDER BY rowid")
        .fetch_all(&pool)
        .await
        .expect("schema");
    let expected_schema: Vec<String> = list(get(&golden, "schema")).iter().map(string).collect();
    assert_eq!(schema, expected_schema);
    let fetch = |sql: &'static str| {
        let pool = pool.clone();
        async move { sqlx::query(sql).fetch_all(&pool).await.expect("rows") }
    };
    assert_eq!(rows_of(&fetch("SELECT * FROM version").await, 1), golden_rows(&golden, "version"));
    assert_eq!(rows_of(&fetch("SELECT * FROM sessions").await, 6), golden_rows(&golden, "sessions"));
    let ids = |rows: Vec<Vec<String>>| rows.into_iter().map(|row| row[..2].to_vec()).collect::<Vec<_>>();
    assert_eq!(
        ids(rows_of(&fetch("SELECT * FROM entities ORDER BY 1").await, 6)),
        ids(golden_rows(&golden, "entities"))
    );
    assert!(fetch("SELECT * FROM sent_files").await.is_empty());
    let mut states = rows_of(&fetch("SELECT id, pts, qts, date, seq FROM update_state ORDER BY 1").await, 5);
    let mut expected = golden_rows(&golden, "update_state");
    // The channel's date: Telethon writes `datetime.now()`.
    states[1][3].clear();
    expected[1][3].clear();
    assert_eq!(states, expected);
    pool.close().await;
}
