//! Replays `rust/goldens/hanime1/parsing.json`: every pure Hanime1 function against the
//! Python baseline's answers, errors included (`{"error": "Class: message"}`).

use fav_core::golden;
use fav_core::pyjson::{Dict, Value};
use fav_runtime::source::JobError;

use crate::hanime1::{self, HanimeRecord, WatchSeries, WatchSeriesVideo};
use crate::{hanime1_author, runtime_config};

fn arg_str(args: &[Value], index: usize) -> String {
    golden::string(&args[index])
}

fn arg_opt_str(args: &[Value], index: usize) -> Option<String> {
    match &args[index] {
        Value::Null => None,
        value => Some(golden::string(value)),
    }
}

fn arg_bool(args: &[Value], index: usize) -> bool {
    match &args[index] {
        Value::Bool(value) => *value,
        other => panic!("expected a bool, got {other:?}"),
    }
}

fn arg_strings(args: &[Value], index: usize) -> Vec<String> {
    golden::list(&args[index]).iter().map(golden::string).collect()
}

const HOST: &str = "https://hanime1.me";
const ROOT: &str = "collection/hanime1";

fn arg_int(args: &[Value], index: usize) -> i64 {
    i64::try_from(golden::int(&args[index])).expect("small int")
}

fn arg_record(args: &[Value], index: usize) -> HanimeRecord {
    let item = &args[index];
    let opt = |key: &str| match golden::get(item, key) {
        Value::Null => None,
        value => Some(golden::string(value)),
    };
    HanimeRecord {
        id: golden::string(golden::get(item, "id")),
        title: golden::string(golden::get(item, "title")),
        page_url: opt("page_url"),
        stream_url: opt("stream_url"),
        ..HanimeRecord::default()
    }
}

fn videos(titles: &[Value]) -> Vec<WatchSeriesVideo> {
    titles
        .iter()
        .zip(1..)
        .map(|(title, position)| WatchSeriesVideo {
            video_id: position.to_string(),
            title: match title {
                Value::Null => None,
                value => Some(golden::string(value)),
            },
            position,
        })
        .collect()
}

fn dict(pairs: Vec<(&str, Value)>) -> Value {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key, value);
    }
    Value::Dict(dict)
}

fn strings(values: Vec<String>) -> Value {
    Value::List(values.into_iter().map(Value::Str).collect())
}

fn result<T>(value: Result<T, JobError>, encode: impl FnOnce(T) -> Value) -> Value {
    match value {
        Ok(value) => encode(value),
        Err(error) => {
            let mut dict = Dict::new();
            dict.insert("error", Value::Str(format!("{}: {}", error.class_name(), error.message())));
            Value::Dict(dict)
        }
    }
}

fn entries(entries: Vec<runtime_config::Hanime1PlaylistEntry>) -> Value {
    Value::List(
        entries
            .into_iter()
            .map(|entry| {
                let mut dict = Dict::new();
                dict.insert("video_id", Value::Str(entry.video_id));
                dict.insert("title", entry.title.into());
                Value::Dict(dict)
            })
            .collect(),
    )
}

/// The Rust port's answer for one case, or `None` for a function it does not know.
#[allow(clippy::too_many_lines)] // One arm per baseline function the golden replays.
fn call(name: &str, args: &[Value]) -> Option<Value> {
    Some(match name {
        "normalize_hanime1_watch_title" => Value::Str(runtime_config::normalize_hanime1_watch_title(&arg_str(args, 0))),
        "extract_hanime1_playlist_entries" => result(
            runtime_config::extract_hanime1_playlist_entries(&arg_str(args, 0), arg_bool(args, 1)),
            entries,
        ),
        "parse_hanime1_playlist" => result(
            runtime_config::parse_hanime1_playlist(&arg_str(args, 0), arg_opt_str(args, 1).as_deref(), arg_bool(args, 2)),
            |parsed| match parsed {
                None => Value::Null,
                Some((title, ids)) => Value::List(vec![Value::Str(title), strings(ids)]),
            },
        ),
        "select_hanime1_canonical_id" => runtime_config::select_hanime1_canonical_id(&arg_strings(args, 0)).into(),
        "build_hanime1_seed" => result(
            runtime_config::build_hanime1_seed(&arg_str(args, 0), arg_opt_str(args, 1).as_deref()),
            Value::Str,
        ),
        "parse_runtime_series_seed" => match &args[0] {
            Value::Str(raw) => result(runtime_config::parse_runtime_series_seed(raw), |seed| match seed {
                None => Value::Null,
                Some(seed) => {
                    let mut dict = Dict::new();
                    dict.insert("video_id", Value::Str(seed.video_id));
                    dict.insert("title", Value::Str(seed.title));
                    Value::Dict(dict)
                }
            }),
            // Not a string: Python returns `None` before looking at it. The Rust function
            // takes `&str`, so these cases only record Python's answer; they test nothing.
            _ => Value::Null,
        },
        "normalize_hanime1_seed" => result(runtime_config::normalize_hanime1_seed(&arg_str(args, 0), true), Into::into),
        "extract_hanime1_seed_id" => result(runtime_config::extract_hanime1_seed_id(&arg_str(args, 0)), Into::into),
        "extract_author_display_name" => hanime1_author::extract_author_display_name(&arg_str(args, 0)).into(),
        "extract_author_video_entries" => result(hanime1_author::extract_author_video_entries(&arg_str(args, 0)), |entries| {
            Value::List(
                entries
                    .into_iter()
                    .map(|entry| {
                        let mut dict = Dict::new();
                        dict.insert("video_id", Value::Str(entry.video_id));
                        dict.insert("title", entry.title.into());
                        Value::Dict(dict)
                    })
                    .collect(),
            )
        }),
        "extract_hanime1_author_id" => result(hanime1_author::extract_hanime1_author_id(&arg_str(args, 0)), Into::into),
        "extract_stream_urls" => result(hanime1::extract_stream_urls(&arg_str(args, 0)), strings),
        "parse_title" => hanime1::parse_title(&arg_str(args, 0)).into(),
        "_normalize_download_title" => hanime1::normalize_download_title(arg_opt_str(args, 0).as_deref()).into(),
        "_normalize_watch_title" => hanime1::normalize_watch_title(arg_opt_str(args, 0).as_deref()).into(),
        "_stream_quality" => Value::Int(i128::from(hanime1::stream_quality(&arg_str(args, 0)))),
        "_stream_resolution_label" => hanime1::stream_resolution_label(&arg_str(args, 0)).into(),
        "_format_file_size" => hanime1::format_file_size(match &args[0] {
            Value::Null => None,
            value => Some(golden::int(value)),
        })
        .into(),
        "extract_download_urls" => strings(hanime1::extract_download_urls(&arg_str(args, 0))),
        "_extract_video_code" => result(hanime1::extract_video_code(arg_opt_str(args, 0).as_deref()), Into::into),
        "_build_download_page_url" => result(hanime1::build_download_page_url(HOST, &arg_record(args, 0)), Value::Str),
        "_build_watch_page_url" => result(hanime1::build_watch_page_url(HOST, &arg_record(args, 0)), Value::Str),
        "_derive_item_id" => result(hanime1::derive_item_id(&arg_record(args, 0)), Value::Str),
        "_normalize_page_url" => Value::Str(hanime1::normalize_page_url(HOST, &arg_str(args, 0))),
        "extract_search_video_ids" => result(hanime1::extract_search_video_ids(HOST, &arg_str(args, 0)), strings),
        "extract_watch_series" => result(hanime1::extract_watch_series(&arg_str(args, 0)), |series| match series {
            None => Value::Null,
            Some(WatchSeries { name, video_ids, videos }) => dict(vec![
                ("name", name.into()),
                ("video_ids", strings(video_ids)),
                (
                    "videos",
                    Value::List(
                        videos
                            .into_iter()
                            .map(|video| {
                                dict(vec![
                                    ("video_id", Value::Str(video.video_id)),
                                    ("title", video.title.into()),
                                    ("position", Value::Int(i128::from(video.position))),
                                ])
                            })
                            .collect(),
                    ),
                ),
            ]),
        }),
        "_playlist_item_titles_are_distinct" => Value::Bool(hanime1::playlist_item_titles_are_distinct(&videos(golden::list(&args[0])))),
        "_format_playlist_archive_title" => {
            let video = WatchSeriesVideo {
                video_id: "vid".to_owned(),
                title: arg_opt_str(args, 1),
                position: arg_int(args, 2),
            };
            Value::Str(hanime1::format_playlist_archive_title(
                arg_opt_str(args, 0).as_deref(),
                &video,
                arg_int(args, 3),
                arg_bool(args, 4),
            ))
        }
        "_ignored_title_marker" => hanime1::ignored_title_marker(arg_opt_str(args, 0).as_deref()).into(),
        "extract_watch_metadata" => {
            let metadata = hanime1::extract_watch_metadata(HOST, &arg_str(args, 0));
            dict(vec![
                ("title", metadata.title.into()),
                ("uploader", metadata.uploader.into()),
                ("release_date", metadata.release_date.into()),
                ("plot", metadata.plot.into()),
                ("cover_url", metadata.cover_url.into()),
                ("genre", metadata.genre.into()),
            ])
        }
        "_extract_genre_from_watch_html" => hanime1::extract_genre_from_watch_html(&arg_str(args, 0)).into(),
        "_strip_uploader_prefix" => Value::Str(hanime1::strip_uploader_prefix(&arg_str(args, 0), arg_opt_str(args, 1).as_deref())),
        "_resolve_output_dir" => {
            Value::Str(hanime1::resolve_output_dir(ROOT, arg_opt_str(args, 0).as_deref(), arg_opt_str(args, 1).as_deref()).0)
        }
        "_build_ranking_page_url" => result(
            hanime1::build_ranking_page_url(HOST, &arg_str(args, 0), arg_int(args, 1)),
            Value::Str,
        ),
        "_normalize_runtime_video_id" => match &args[0] {
            Value::Str(raw) => result(hanime1::normalize_runtime_video_id(raw), Into::into),
            // `int`: the same digits as a string (never a URL); `bool`, `None`, floats: `None`.
            // The Rust function takes `&str` (ids are read from text columns), so the last arm
            // only records Python's answer for types Rust cannot be given; it tests nothing.
            Value::Int(raw) if *raw > 0 => result(hanime1::normalize_runtime_video_id(&raw.to_string()), Into::into),
            _ => Value::Null,
        },
        "_series_member_ids_from_watch_series" => {
            let series = WatchSeries {
                video_ids: arg_strings(args, 1),
                ..WatchSeries::default()
            };
            result(hanime1::series_member_ids_from_watch_series(&arg_str(args, 0), &series), strings)
        }
        "_build_download_command" => strings(hanime1::build_download_command(
            "https://video.example.com/master.m3u8",
            "/tmp/cache/videos/12488.%(ext)s",
            arg_opt_str(args, 0).as_deref(),
            arg_opt_str(args, 1).as_deref(),
        )),
        "_format_scan_error" => Value::Str(hanime1::format_scan_error(&JobError::new("RuntimeError", arg_str(args, 0)))),
        _ => return None,
    })
}

#[test]
fn parsing_matches_the_baseline() {
    let document = golden::load("hanime1/parsing.json");
    let mut unknown = std::collections::BTreeSet::new();
    let mut mismatches = Vec::new();
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "fn"));
        let args = golden::list(golden::get(case, "args"));
        let expected = golden::get(case, "result");
        match call(&name, args) {
            None => {
                unknown.insert(name);
            }
            Some(actual) if &actual != expected => {
                mismatches.push(format!("{name}{args:?}\n  expected {expected:?}\n  actual   {actual:?}"));
            }
            Some(_) => {}
        }
    }
    assert!(mismatches.is_empty(), "{} mismatches:\n{}", mismatches.len(), mismatches.join("\n"));
    assert!(unknown.is_empty(), "functions without a port: {unknown:?}");
}

#[test]
fn constants_match_the_baseline() {
    let document = golden::load("hanime1/constants.json");
    let constants = &golden::cases(&document)[0];
    assert_eq!(golden::string(golden::get(constants, "user_agent")), hanime1::USER_AGENT);
    assert_eq!(
        golden::string(golden::get(constants, "series_user_agent")),
        crate::hanime1_series::USER_AGENT
    );
    let markers: Vec<String> = golden::list(golden::get(constants, "cf_block_markers"))
        .iter()
        .map(golden::string)
        .collect();
    assert_eq!(markers, crate::hanime1_series::CF_BLOCK_MARKERS);
    assert_eq!(
        golden::string(golden::get(constants, "parser_failure_dedupe_key")),
        hanime1::parser_failure_dedupe_key()
    );
    let ignored: Vec<String> = golden::list(golden::get(constants, "ignored_title_markers"))
        .iter()
        .map(golden::string)
        .collect();
    assert_eq!(ignored, hanime1::IGNORED_TITLE_MARKERS);
}
