//! Replays `goldens/wechat/`: every pure function of the WeChat source, its sticker
//! and 视频号 helpers against Python's answers (`parsing.json`), the ISAAC-64 keystream
//! (`keystream.json`) and the constants (`constants.json`). Errors are
//! `{"error": "Class: message"}` and bytes `{"hex": ...}`.

use std::fmt::Write as _;

use fav_core::golden;
use fav_core::pyjson::{Dict, Value};
use sha2::{Digest as _, Sha256};

use crate::pydecode::PyError;
use crate::wechat::{self, sent_at};
use crate::wechat_channels::{self, ChannelVideo, VideoSource};
use crate::wechat_emoji::{self, Sticker};

fn dict(pairs: Vec<(&str, Value)>) -> Value {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key, value);
    }
    Value::Dict(dict)
}

fn text(value: &str) -> Value {
    Value::from(value)
}

fn hex_text(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut digits, byte| {
        let _ = write!(digits, "{byte:02x}");
        digits
    })
}

fn hex(bytes: &[u8]) -> Value {
    dict(vec![("hex", Value::Str(hex_text(bytes)))])
}

fn unhex(value: &Value) -> Vec<u8> {
    let digits = golden::string(golden::get(value, "hex"));
    (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&digits[at..at + 2], 16).expect("hex"))
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    hex_text(&Sha256::digest(bytes))
}

fn answer<T>(result: Result<T, PyError>, plain: impl FnOnce(T) -> Value) -> Value {
    result.map_or_else(
        |failure| dict(vec![("error", Value::Str(format!("{}: {}", failure.class, failure.message)))]),
        plain,
    )
}

fn optional<T>(value: Option<T>, plain: impl FnOnce(T) -> Value) -> Value {
    value.map_or(Value::Null, plain)
}

fn arg<'a>(args: &'a Value, name: &str) -> &'a Value {
    golden::get(args, name)
}

fn arg_dict<'a>(args: &'a Value, name: &str) -> &'a Dict {
    arg(args, name).as_dict().expect("a dict argument")
}

fn sticker(sticker: &Sticker) -> Value {
    dict(vec![
        ("md5", text(&sticker.md5)),
        ("cdn_url", text(&sticker.cdn_url)),
        ("encrypted_url", text(&sticker.encrypted_url)),
        ("aes_key", text(&sticker.aes_key)),
    ])
}

fn sticker_of(value: &Value) -> Sticker {
    let field = |name: &str| golden::string(golden::get(value, name));
    Sticker {
        md5: field("md5"),
        cdn_url: field("cdn_url"),
        encrypted_url: field("encrypted_url"),
        aes_key: field("aes_key"),
    }
}

fn video(video: &ChannelVideo) -> Value {
    dict(vec![
        ("object_id", text(&video.object_id)),
        ("nonce_id", text(&video.nonce_id)),
        ("author", text(&video.author)),
        ("description", text(&video.description)),
        ("duration", Value::Int(video.duration)),
        ("width", Value::Int(video.width)),
        ("height", Value::Int(video.height)),
        ("cover_url", text(&video.cover_url)),
    ])
}

fn video_of(value: &Value) -> ChannelVideo {
    let field = |name: &str| golden::string(golden::get(value, name));
    let number = |name: &str| golden::int(golden::get(value, name));
    ChannelVideo {
        object_id: field("object_id"),
        nonce_id: field("nonce_id"),
        author: field("author"),
        description: field("description"),
        duration: number("duration"),
        width: number("width"),
        height: number("height"),
        cover_url: field("cover_url"),
    }
}

fn source(source: &VideoSource) -> Value {
    dict(vec![
        ("url", text(&source.url)),
        ("decode_key", Value::Int(i128::from(source.decode_key))),
    ])
}

fn texts(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

fn parse(name: &str, args: &Value) -> Value {
    match name {
        "parse_sticker" => optional(wechat_emoji::parse_sticker(&golden::string(arg(args, "content"))), |found| {
            sticker(&found)
        }),
        "decrypt_sticker" => answer(
            wechat_emoji::decrypt_sticker(&unhex(arg(args, "content")), &golden::string(arg(args, "aes_key"))),
            |plain| hex(&plain),
        ),
        "matches" => Value::Bool(wechat_emoji::matches(
            &sticker_of(arg(args, "sticker")),
            &unhex(arg(args, "content")),
        )),
        "parse_channel_video" => answer(
            wechat_channels::parse_channel_video(&golden::string(arg(args, "content"))),
            |found| optional(found, |found| video(&found)),
        ),
        "tikhub_lookup" => Value::Dict(wechat_channels::tikhub_lookup(&video_of(arg(args, "video")))),
        "video_title" => text(&wechat_channels::video_title(&video_of(arg(args, "video")))),
        "best_quality" => answer(wechat_channels::best_quality(&golden::string(arg(args, "url"))), |url| text(&url)),
        "parse_video_source" => answer(wechat_channels::parse_video_source(arg(args, "payload")), |found| {
            optional(found, |found| source(&found))
        }),
        "pending_images" => Value::List(
            wechat::pending_images(golden::list(arg(args, "items")))
                .into_iter()
                .map(Value::Dict)
                .collect(),
        ),
        "work_for" => answer(wechat::work_for(arg(args, "event")), |work| optional(work, text)),
        "unsupported_reason" => answer(wechat::unsupported_reason(arg_dict(args, "message")), |reason| text(&reason)),
        "queued_message" => Value::Dict(wechat::queued_message(
            arg_dict(args, "message"),
            &golden::string(arg(args, "work")),
        )),
        "image_item" => answer(wechat::image_item(arg_dict(args, "message")), Value::Dict),
        "sniff_extension" => optional(wechat::sniff_extension(&unhex(arg(args, "content"))), text),
        "file_extension" => text(&wechat::file_extension(
            arg_dict(args, "item"),
            &golden::string(arg(args, "content_type")),
        )),
        "relative_path" => answer(
            wechat::relative_path(arg_dict(args, "item"), &golden::string(arg(args, "ext"))),
            |path| text(&path),
        ),
        "sent_at" => answer(sent_at(golden::int(arg(args, "create_time_ms"))), |at| {
            text(&fav_core::pyjson::py_isoformat_datetime(&at.fixed_offset()))
        }),
        "article_directory" => answer(
            wechat::article_directory(arg_dict(args, "message"), &golden::string(arg(args, "title"))),
            |path| text(&path),
        ),
        "video_path" => answer(
            wechat::video_path(arg_dict(args, "message"), &video_of(arg(args, "video"))),
            |path| text(&path),
        ),
        "open_target" => {
            let texts = texts(arg(args, "texts"));
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            answer(wechat::open_target(arg_dict(args, "message"), &texts), Value::Dict)
        }
        "post_video_name" => {
            let number = |name: &str| usize::try_from(golden::int(arg(args, name))).expect("a count");
            text(&wechat::post_video_name(number("index"), number("count")))
        }
        other => panic!("no replay for {other}"),
    }
}

#[test]
fn every_parser_answers_as_python_does() {
    let document = golden::load("wechat/parsing.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "fn"));
        let args = golden::get(case, "args");
        assert_eq!(&parse(&name, args), golden::get(case, "result"), "{name} {args:?}");
    }
}

/// The inputs `_keystreams` decrypts, by what the golden shows of them.
fn keystream_input(shown: &Value) -> Vec<u8> {
    if shown.get("hex").is_some() {
        return unhex(shown);
    }
    let plain: Vec<u8> = (0..600).flat_map(|_| 0..=255_u8).collect();
    let size = usize::try_from(golden::int(golden::get(shown, "size"))).expect("a size");
    let input = plain[..size].to_vec();
    assert_eq!(sha256(&input), golden::string(golden::get(shown, "sha256")));
    input
}

#[test]
fn the_keystream_is_pythons() {
    let document = golden::load("wechat/keystream.json");
    for case in golden::cases(&document) {
        let key = u64::try_from(golden::int(golden::get(case, "decode_key"))).expect("a 64-bit key");
        if let Some(shown) = case.get("decrypt_head") {
            let result = wechat_channels::decrypt_head(&keystream_input(shown), key);
            assert_eq!(sha256(&result), golden::string(golden::get(case, "result_sha256")), "{case:?}");
            assert_eq!(
                hex_text(&result[..result.len().min(64)]),
                golden::string(golden::get(case, "result_head"))
            );
            assert_eq!(
                i128::try_from(result.len()).ok(),
                Some(golden::int(golden::get(case, "result_size")))
            );
            continue;
        }
        let size = usize::try_from(golden::int(golden::get(case, "size"))).expect("a size");
        let stream = wechat_channels::keystream(key, size);
        assert_eq!(stream.len(), size);
        assert_eq!(sha256(&stream), golden::string(golden::get(case, "sha256")), "{key} {size}");
        assert_eq!(hex_text(&stream[..size.min(64)]), golden::string(golden::get(case, "head")));
        if let Some(tail) = case.get("tail") {
            assert_eq!(hex_text(&stream[size - 64..]), golden::string(tail));
        }
    }
}

#[test]
fn the_constants_are_pythons() {
    let document = golden::load("wechat/constants.json");
    let constants = &golden::cases(&document)[0];
    let expect = |key: &str, value: Value| assert_eq!(golden::get(constants, key), &value, "{key}");
    let list = |values: &[&str]| Value::List(values.iter().map(|value| text(value)).collect());
    let int = |value: u64| Value::Int(i128::from(value));
    #[allow(clippy::cast_precision_loss)] // reason: small whole numbers of seconds
    let seconds = |value: u64| Value::Float(value as f64);
    expect("wechat._KNOWN_EXTENSIONS", list(&wechat::KNOWN_EXTENSIONS));
    let mut types = Dict::new();
    for (content_type, extension) in wechat::CONTENT_TYPE_EXTENSIONS {
        types.insert(content_type, text(extension));
    }
    expect("wechat._CONTENT_TYPE_EXTENSIONS", Value::Dict(types));
    expect("wechat._REQUEST_TIMEOUT_SECONDS", seconds(wechat::REQUEST_TIMEOUT_SECONDS));
    expect(
        "wechat._EVENT_PAGE_SIZE",
        int(u64::try_from(wechat::EVENT_PAGE_SIZE).expect("small")),
    );
    expect("wechat._MAX_ATTEMPTS", Value::Int(i128::from(wechat::MAX_ATTEMPTS)));
    expect("wechat._OPEN_WAIT_SECONDS", int(wechat::OPEN_WAIT_SECONDS));
    expect("wechat._LINK_WAIT_SECONDS", int(wechat::LINK_WAIT_SECONDS));
    expect("wechat._TAP_OPEN_SECONDS", int(wechat::TAP_OPEN_SECONDS));
    expect("wechat._CURSOR_KEY", text(wechat::CURSOR_KEY));
    expect("wechat._WORK_VERSION", text(wechat::WORK_VERSION));
    expect("wechat._WORK_VERSION_KEY", text(wechat::WORK_VERSION_KEY));
    expect("wechat._UNSUPPORTED_SINCE_KEY", text(wechat::UNSUPPORTED_SINCE_KEY));
    expect("wechat._NGA_LOGIN_KEY", text(wechat::NGA_LOGIN_KEY));
    expect("wechat._TIKHUB_KEY", text(wechat::TIKHUB_KEY));
    let mut needs = Dict::new();
    needs.insert("nga-login", text(wechat::NGA_LOGIN_KEY));
    needs.insert("tikhub", text(wechat::TIKHUB_KEY));
    expect("wechat._NEEDS_KEYS", Value::Dict(needs));
    expect("wechat._TIKHUB_VIDEO_DETAIL", text(wechat::TIKHUB_VIDEO_DETAIL));
    expect("wechat._TIKHUB_READ_SECONDS", int(crate::post_links::REQUEST_TIMEOUT_SECONDS));
    expect(
        "wechat._TIKHUB_ACCOUNT_STATUSES",
        Value::List(
            wechat::TIKHUB_ACCOUNT_STATUSES
                .iter()
                .map(|status| int(u64::from(*status)))
                .collect(),
        ),
    );
    expect("wechat._POST_VIDEO_READ_SECONDS", seconds(crate::post_links::VIDEO_READ_SECONDS));
    expect("wechat._SILENT_KINDS", list(&wechat::SILENT_KINDS));
    expect(
        "wechat.WORK",
        list(&[
            wechat::WORK_IMAGE,
            wechat::WORK_STICKER,
            wechat::WORK_ARTICLE,
            wechat::WORK_CHANNEL_VIDEO,
            wechat::WORK_UNSUPPORTED,
        ]),
    );
    expect(
        "wechat_channels.ENCRYPTED_BYTES",
        int(u64::try_from(wechat_channels::ENCRYPTED_BYTES).expect("small")),
    );
}
