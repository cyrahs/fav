use std::collections::VecDeque;

use fav_core::golden;
use fav_core::logging::Level;
use fav_core::pyjson::Key;
use fav_core::settings::Settings;
use fav_core::validate::Model;
use fav_db::testdb::TestDatabase;
use fav_net::subprocess::{BoxFuture as RunFuture, Completed, LineSink};

use super::*;
use crate::testkit::{Answer, FakeServer, Received, RecordingNotifier};

const USER_NOT_FOUND: &str = "[twitter][error] NotFoundError: Requested user could not be found";
const KEYS_REFRESHED: &str = "[twitter][info] Initializing client transaction keys";

fn json(text: &str) -> Value {
    pyjson::loads(text).expect("test JSON")
}

fn config(overrides: &str) -> TwitterConfig {
    TwitterConfig::validate(&json(overrides)).expect("valid settings")
}

/// `_runnable_cfg`: the defaults with a username.
fn runnable(overrides: &str) -> TwitterConfig {
    let mut payload = json(overrides);
    if let Value::Dict(dict) = &mut payload
        && dict.get("username").is_none()
    {
        dict.insert("username", Value::from("me"));
    }
    TwitterConfig::validate(&payload).expect("valid settings")
}

fn cookie(name: &str, value: &str, expires: Option<f64>) -> StoredCookie {
    StoredCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: ".x.com".to_owned(),
        path: "/".to_owned(),
        expires,
    }
}

fn signed_in() -> Vec<StoredCookie> {
    vec![
        cookie("auth_token", "tok", Some(1_900_000_000.0)),
        cookie("ct0", "csrf", None),
        cookie("twid", "u%3D4242", None),
    ]
}

/// The value of a `--option key=value` pair.
fn option<'a>(command: &'a [String], key: &str) -> Option<&'a str> {
    let prefix = format!("{key}=");
    command.iter().find_map(|arg| arg.strip_prefix(&prefix))
}

fn after<'a>(command: &'a [String], flag: &str) -> &'a str {
    let index = command.iter().position(|arg| arg == flag).expect("the flag");
    &command[index + 1]
}

fn command_for(config: &TwitterConfig, incremental: bool) -> Vec<String> {
    build_command(config, "/c", "/a", incremental)
}

/// A database nothing listens on, for tests that must never reach it.
fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

async fn database() -> TestDatabase {
    let database = TestDatabase::create().await;
    database.db().ensure_schema().await.expect("schema applies");
    database
}

async fn sign_in(db: &Db, cookies: &[StoredCookie]) {
    site_sessions::save_signed_in(db, SESSION_SITE, cookies, "me", None, &[])
        .await
        .expect("store the login");
}

async fn stored_status(db: &Db) -> (String, String) {
    let session = site_sessions::load(db, SESSION_SITE).await.expect("load").expect("a login");
    (session.status, session.message)
}

fn sidecar_payload(overrides: &str) -> Dict {
    let mut payload = json(
        r#"{"tweet_id": 1234567890123456789, "num": 2, "author": {"id": 42, "name": "artist", "nick": "The Artist"},
            "content": "a caption", "date": "2026-05-01 12:34:56", "type": "photo", "extension": "jpg"}"#,
    );
    if let (Value::Dict(payload), Value::Dict(overrides)) = (&mut payload, json(overrides)) {
        for (key, value) in overrides.iter() {
            payload.insert(key.clone(), value.clone());
        }
    }
    match payload {
        Value::Dict(dict) => dict,
        _ => unreachable!("a dict"),
    }
}

fn write_sidecar(directory: &Path, name: &str, payload: &Dict) -> PathBuf {
    std::fs::create_dir_all(directory).expect("directory");
    std::fs::write(directory.join(name), b"binary").expect("media");
    let sidecar = directory.join(format!("{name}.json"));
    let text = pyjson::dumps(&Value::Dict(payload.clone()), &DumpOptions::python_default()).expect("JSON");
    std::fs::write(&sidecar, text).expect("sidecar");
    sidecar
}

fn utf8(path: &Path) -> String {
    path.to_str().expect("utf-8 path").to_owned()
}

// ---------- the fake gallery-dl ----------

/// One scripted run: exit code, stdout and stderr lines, and files to leave under the
/// destination (relative path, contents).
#[derive(Debug, Clone, Default)]
struct Run {
    returncode: i32,
    stdout: Vec<String>,
    stderr: Vec<String>,
    files: Vec<(String, Vec<u8>)>,
}

fn exits(returncode: i32, stderr: &[&str]) -> Run {
    Run {
        returncode,
        stderr: stderr.iter().map(|line| (*line).to_owned()).collect(),
        ..Run::default()
    }
}

/// gallery-dl, scripted: each run takes the next outcome and records its command and the
/// cookie file it was handed.
#[derive(Debug, Default)]
struct FakeGalleryDl {
    runs: Mutex<VecDeque<Run>>,
    commands: Mutex<Vec<Vec<String>>>,
    cookie_files: Mutex<Vec<String>>,
}

impl FakeGalleryDl {
    fn new(runs: Vec<Run>) -> Self {
        Self {
            runs: Mutex::new(runs.into()),
            ..Self::default()
        }
    }

    fn calls(&self) -> usize {
        self.commands.lock().expect("commands").len()
    }
}

impl Runner for FakeGalleryDl {
    fn run(&self, _argv: Vec<String>) -> RunFuture<'_, std::io::Result<Completed>> {
        unreachable!("gallery-dl output is streamed")
    }

    fn run_lines<'a>(&'a self, argv: Vec<String>, sink: LineSink<'a>) -> RunFuture<'a, std::io::Result<i32>> {
        let run = self.runs.lock().expect("runs").pop_front().expect("a scripted run");
        if let Some(index) = argv.iter().position(|arg| arg == "--cookies") {
            let text = std::fs::read_to_string(&argv[index + 1]).expect("the cookie file");
            self.cookie_files.lock().expect("cookies").push(text);
        }
        if let Some(index) = argv.iter().position(|arg| arg == "--destination") {
            let root = PathBuf::from(&argv[index + 1]);
            for (relative, bytes) in &run.files {
                let path = root.join(relative);
                std::fs::create_dir_all(path.parent().expect("a parent")).expect("directory");
                std::fs::write(path, bytes).expect("a download");
            }
        }
        self.commands.lock().expect("commands").push(argv);
        Box::pin(async move {
            for line in &run.stdout {
                sink(Stream::Stdout, line);
            }
            for line in &run.stderr {
                sink(Stream::Stderr, line);
            }
            Ok(run.returncode)
        })
    }
}

fn crawler(db: Db, config: TwitterConfig, runner: &Arc<FakeGalleryDl>, notifier: &Arc<RecordingNotifier>) -> Twitter {
    let shared: SharedNotifier = notifier.clone();
    Twitter::new(db, shared, runner.clone(), config).with_retry_delay(Duration::ZERO)
}

fn offline(config: TwitterConfig) -> Twitter {
    crawler(unused_db(), config, &Arc::default(), &Arc::default())
}

// ---------- configuration ----------

// py: tests/test_twitter.py::test_a_source_without_a_username_is_not_runnable
#[test]
fn a_source_without_a_username_is_not_runnable() {
    assert_eq!(config(r#"{"username": ""}"#).validate_runnable(), ["username"]);
}

// py: tests/test_twitter.py::test_a_username_is_all_it_takes_since_the_login_lives_in_the_database
#[test]
fn a_username_is_all_it_takes_since_the_login_lives_in_the_database() {
    assert!(runnable("{}").validate_runnable().is_empty());
}

// py: tests/test_twitter.py::test_a_cookiecloud_reference_left_in_an_older_row_is_ignored
#[test]
fn a_cookiecloud_reference_left_in_an_older_row_is_ignored() {
    let cfg = config(r#"{"username": "me", "cookiecloud": "main"}"#);

    assert!(cfg.dump().get("cookiecloud").is_none());
    assert!(cfg.validate_runnable().is_empty());
}

// py: tests/test_twitter.py::test_a_pasted_username_keeps_working_with_its_at_sign
#[test]
fn a_pasted_username_keeps_working_with_its_at_sign() {
    assert_eq!(config(r#"{"username": "  @Me_1 "}"#).username, "Me_1");
}

// py: tests/test_twitter.py::test_a_username_with_a_slash_is_rejected_rather_than_pasted_into_the_url
#[test]
fn a_username_with_a_slash_is_rejected_rather_than_pasted_into_the_url() {
    let error = TwitterConfig::validate(&json(r#"{"username": "me/likes"}"#)).expect_err("rejected");
    assert!(error.to_string().contains("username"), "{error}");
}

// py: tests/test_twitter.py::test_abort_after_must_leave_room_for_at_least_one_file
#[test]
fn abort_after_must_leave_room_for_at_least_one_file() {
    let error = TwitterConfig::validate(&json(r#"{"abort_after": 0}"#)).expect_err("rejected");
    assert!(error.to_string().contains("abort_after"), "{error}");
}

// py: tests/test_twitter.py::test_an_empty_video_path_is_not_read_as_the_working_directory
#[test]
fn an_empty_video_path_is_not_read_as_the_working_directory() {
    assert_eq!(config(r#"{"video_path": ""}"#).video_path, None);
    assert_eq!(config(r#"{"video_path": "   "}"#).video_path, None);
}

// ---------- gallery-dl invocation ----------

#[test]
fn commands_match_the_recorded_ones() {
    let document = golden::load("twitter/commands.json");
    for case in golden::cases(&document) {
        let settings = golden::get(case, "settings");
        let cfg = runnable(&pyjson::dumps(settings, &DumpOptions::python_default()).expect("JSON"));
        let incremental = matches!(golden::get(case, "incremental"), Value::Bool(true));
        let archive = fav_core::pypath::join(cfg.path.as_str(), ARCHIVE_FILENAME);
        let command = build_command(&cfg, "/run/c.txt", &archive, incremental);
        let want: Vec<String> = golden::list(golden::get(case, "command")).iter().map(golden::string).collect();
        assert_eq!(command, want, "{settings:?}");
    }
}

/// How the installed gallery-dl's own parser read one recorded command.
fn gallery_dl_options(case: &Value) -> Vec<(String, String, Value)> {
    golden::list(golden::get(golden::get(case, "gallery_dl"), "options"))
        .iter()
        .map(|entry| {
            let entry = golden::list(entry);
            let path: Vec<String> = golden::list(&entry[0]).iter().map(golden::string).collect();
            (path.join("."), golden::string(&entry[1]), entry[2].clone())
        })
        .collect()
}

fn recorded_case(settings: &str, incremental: bool) -> Value {
    let document = golden::load("twitter/commands.json");
    let wanted = json(settings);
    golden::cases(&document)
        .iter()
        .find(|case| *golden::get(case, "settings") == wanted && *golden::get(case, "incremental") == Value::Bool(incremental))
        .cloned()
        .expect("a recorded command")
}

// py: tests/test_twitter.py::test_the_installed_gallery_dl_still_understands_every_option_we_pass
#[test]
fn the_installed_gallery_dl_still_understands_every_option_we_pass() {
    // gallery-dl's parser ran when the golden was recorded (`generate.py --check` runs it
    // again against the installed version); this command is byte-for-byte that one.
    let case = recorded_case(r#"{"path": "/data/twitter", "abort_after": 7}"#, true);
    let cfg = runnable(r#"{"path": "/data/twitter", "sleep_request_seconds": 2.0, "abort_after": 7}"#);
    let command = build_command(&cfg, "/run/c.txt", "/data/twitter/.gallery-dl-archive.db", true);
    let recorded: Vec<String> = golden::list(golden::get(&case, "command")).iter().map(golden::string).collect();
    assert_eq!(command, recorded);

    let parsed = golden::get(&case, "gallery_dl");
    assert_eq!(*golden::get(parsed, "urls"), json(r#"["https://x.com/me/likes"]"#));
    // --write-metadata is what produces the sidecars the ingest step consumes.
    assert_eq!(*golden::get(parsed, "postprocessors"), json(r#"["metadata"]"#));
    let options = gallery_dl_options(&case);
    let get = |path: &str, key: &str| {
        options
            .iter()
            .find(|(option_path, option_key, _)| option_path == path && option_key == key)
            .map(|(_, _, value)| value.clone())
    };
    assert_eq!(get("", "base-directory"), Some(Value::from("/data/twitter")));
    assert_eq!(get("", "cookies"), Some(Value::from("/run/c.txt")));
    assert_eq!(get("", "archive"), Some(Value::from("/data/twitter/.gallery-dl-archive.db")));
    // A list, not the raw string: gallery-dl parses -o values as JSON when it can.
    assert_eq!(get("extractor.twitter", "directory"), Some(json(r#"["{author[name]}"]"#)));
    assert_eq!(get("extractor.twitter", "filename"), Some(Value::from(FILENAME_FORMAT)));
    assert_eq!(get("extractor.twitter", "sleep-request"), Some(Value::Float(2.0)));
    assert_eq!(get("extractor.twitter", "retweets"), Some(Value::from("original")));
    assert_eq!(get("extractor.twitter", "videos"), Some(Value::Bool(true)));
    assert_eq!(get("extractor.twitter", "skip"), Some(Value::from("abort:7")));
}

// py: tests/test_twitter.py::test_a_backfill_command_leaves_gallery_dl_with_no_skip_override
#[test]
fn a_backfill_command_leaves_gallery_dl_with_no_skip_override() {
    let case = recorded_case(r#"{"path": "/data/twitter", "abort_after": 20}"#, false);

    assert!(gallery_dl_options(&case).iter().all(|(_, key, _)| key != "skip"));
}

// py: tests/test_twitter.py::test_the_backfill_run_walks_the_whole_timeline
#[test]
fn the_backfill_run_walks_the_whole_timeline() {
    let cfg = runnable(r#"{"path": "/data/twitter", "abort_after": 20}"#);

    let command = build_command(&cfg, "/run/cookies/c.txt", "/data/a.db", false);

    // Stopping at the first known file would leave everything below it unreachable.
    assert_eq!(option(&command, "extractor.twitter.skip"), None);
    assert_eq!(command.last().map(String::as_str), Some("https://x.com/me/likes"));
    assert_eq!(after(&command, "--destination"), "/data/twitter");
    assert_eq!(after(&command, "--cookies"), "/run/cookies/c.txt");
    assert_eq!(after(&command, "--download-archive"), "/data/a.db");
    assert!(command.iter().any(|arg| arg == "--write-metadata"));
}

// py: tests/test_twitter.py::test_an_incremental_run_stops_after_the_configured_run_of_known_files
#[test]
fn an_incremental_run_stops_after_the_configured_run_of_known_files() {
    let command = build_command(&runnable(r#"{"abort_after": 7}"#), "/run/cookies/c.txt", "/a.db", true);

    assert_eq!(option(&command, "extractor.twitter.skip"), Some("abort:7"));
}

// py: tests/test_twitter.py::test_the_directory_option_is_json_so_gallery_dl_reads_it_as_a_list
#[test]
fn the_directory_option_is_json_so_gallery_dl_reads_it_as_a_list() {
    let command = command_for(&runnable("{}"), false);

    assert_eq!(
        json(option(&command, "extractor.twitter.directory").expect("set")),
        json(r#"["{author[name]}"]"#)
    );
}

// py: tests/test_twitter.py::test_liked_retweets_are_filed_under_the_original_author_by_default
#[test]
fn liked_retweets_are_filed_under_the_original_author_by_default() {
    assert_eq!(
        option(&command_for(&runnable("{}"), false), "extractor.twitter.retweets"),
        Some("original")
    );
}

// py: tests/test_twitter.py::test_turning_off_retweets_skips_them_entirely
#[test]
fn turning_off_retweets_skips_them_entirely() {
    let command = command_for(&runnable(r#"{"include_retweets": false}"#), false);

    assert_eq!(option(&command, "extractor.twitter.retweets"), Some("false"));
}

// py: tests/test_twitter.py::test_the_proxy_is_passed_through_only_when_one_is_set
#[test]
fn the_proxy_is_passed_through_only_when_one_is_set() {
    let without = command_for(&runnable(r#"{"proxy": ""}"#), false);
    assert!(!without.iter().any(|arg| arg == "--proxy"));

    let with_proxy = command_for(&runnable(r#"{"proxy": "http://proxy.example:8080"}"#), false);
    assert_eq!(after(&with_proxy, "--proxy"), "http://proxy.example:8080");
}

// py: tests/test_twitter.py::test_the_request_interval_reaches_gallery_dl
#[test]
fn the_request_interval_reaches_gallery_dl() {
    let command = command_for(&runnable(r#"{"sleep_request_seconds": 3.5}"#), false);

    assert_eq!(option(&command, "extractor.twitter.sleep-request"), Some("3.5"));
}

// py: tests/test_twitter.py::test_videos_are_requested_by_default_and_can_be_turned_off
#[test]
fn videos_are_requested_by_default_and_can_be_turned_off() {
    assert_eq!(
        option(&command_for(&runnable("{}"), false), "extractor.twitter.videos"),
        Some("true")
    );
    let off = command_for(&runnable(r#"{"include_videos": false}"#), false);
    assert_eq!(option(&off, "extractor.twitter.videos"), Some("false"));
}

#[test]
fn texts_match_the_recorded_ones() {
    let document = golden::load("twitter/texts.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let strings = |values: &[&str]| Value::List(values.iter().map(|value| Value::from(*value)).collect());
        let ours = match name.as_str() {
            "FILENAME_FORMAT" => Value::from(FILENAME_FORMAT),
            "DIRECTORY_FORMAT" => strings(&DIRECTORY_FORMAT),
            "ARCHIVE_FILENAME" => Value::from(ARCHIVE_FILENAME),
            "COOKIE_FILENAME" => Value::from(COOKIE_FILENAME),
            "USER_LOOKUP_ERROR" => Value::from(USER_LOOKUP_ERROR),
            "LOGIN_URL" => Value::from(LOGIN_URL),
            "SESSION_COOKIE_URLS" => strings(&SESSION_COOKIE_URLS),
            other => panic!("no text {other}"),
        };
        assert_eq!(&ours, golden::get(case, "value"), "{name}");
    }
}

// ---------- sidecar parsing ----------

fn row_as_value(row: &SidecarRow) -> Value {
    let mut dict = Dict::new();
    dict.insert("tweet_id", Value::Int(row.tweet_id));
    dict.insert("num", Value::Int(row.num));
    for (key, value) in [
        ("author", &row.author),
        ("author_nick", &row.author_nick),
        ("author_id", &row.author_id),
        ("content", &row.content),
        ("tweet_date", &row.tweet_date),
        ("media_type", &row.media_type),
        ("local_path", &row.local_path),
        ("metadata", &row.metadata),
    ] {
        dict.insert(key, Value::from(value.as_str()));
    }
    Value::Dict(dict)
}

#[test]
fn sidecars_match_the_recorded_rows() {
    let document = golden::load("twitter/sidecars.json");
    for case in golden::cases(&document) {
        let text = golden::string(golden::get(case, "sidecar"));
        let Value::Dict(payload) = json(&text) else { panic!("a dict") };
        let row = parse_sidecar(
            &payload,
            &golden::string(golden::get(case, "media_path")),
            &golden::string(golden::get(case, "root")),
        );
        assert_eq!(row.as_ref().map_or(Value::Null, row_as_value), *golden::get(case, "row"), "{text}");
    }
}

// py: tests/test_twitter.py::test_a_sidecar_becomes_a_row_with_a_path_relative_to_the_collection
#[test]
fn a_sidecar_becomes_a_row_with_a_path_relative_to_the_collection() {
    let row = parse_sidecar(&sidecar_payload("{}"), "/data/twitter/artist/pic.jpg", "/data/twitter").expect("a row");

    assert_eq!(row.tweet_id, 1_234_567_890_123_456_789);
    assert_eq!(row.num, 2);
    assert_eq!(row.author, "artist");
    assert_eq!(row.author_nick, "The Artist");
    assert_eq!(row.author_id, "42");
    assert_eq!(row.content, "a caption");
    assert_eq!(row.tweet_date, "2026-05-01 12:34:56");
    assert_eq!(row.media_type, "photo");
    // Relative, so moving the collection directory does not invalidate the archive.
    assert_eq!(row.local_path, "artist/pic.jpg");
    assert_eq!(json(&row.metadata).get("tweet_id"), Some(&Value::Int(1_234_567_890_123_456_789)));
}

// py: tests/test_twitter.py::test_a_file_outside_the_collection_keeps_its_absolute_path
#[test]
fn a_file_outside_the_collection_keeps_its_absolute_path() {
    let row = parse_sidecar(&sidecar_payload("{}"), "/elsewhere/pic.jpg", "/data/twitter").expect("a row");

    assert_eq!(row.local_path, "/elsewhere/pic.jpg");
}

// py: tests/test_twitter.py::test_an_author_gallery_dl_could_not_resolve_leaves_empty_strings_not_none
#[test]
fn an_author_gallery_dl_could_not_resolve_leaves_empty_strings_not_none() {
    let row = parse_sidecar(&sidecar_payload(r#"{"author": {}}"#), "/p.jpg", "/data").expect("a row");

    assert_eq!(row.author, "");
    assert_eq!(row.author_id, "");
}

// py: tests/test_twitter.py::test_json_that_is_not_a_tweet_is_ignored_rather_than_stored
#[test]
fn json_that_is_not_a_tweet_is_ignored_rather_than_stored() {
    let Value::Dict(other) = json(r#"{"hello": "world"}"#) else {
        panic!("a dict")
    };
    assert_eq!(parse_sidecar(&other, "/p.jpg", "/data"), None);
    assert_eq!(parse_sidecar(&sidecar_payload(r#"{"num": "2"}"#), "/p.jpg", "/data"), None);
}

#[test]
fn relative_paths_follow_pathlib() {
    assert_eq!(relative_to("/data/twitter/a/b.jpg", "/data/twitter/").as_deref(), Some("a/b.jpg"));
    assert_eq!(relative_to("/data/twitter", "/data/twitter").as_deref(), Some("."));
    assert_eq!(relative_to("/r/../r/x.jpg", "/r").as_deref(), Some("../r/x.jpg"));
    assert_eq!(relative_to("a/b", ".").as_deref(), Some("a/b"));
    assert_eq!(relative_to("/data/twitterx/a", "/data/twitter"), None);
    assert_eq!(relative_to("data/a", "/data"), None);
}

// ---------- media type split ----------

#[tokio::test]
async fn destinations_match_the_recorded_ones() {
    let document = golden::load("twitter/destinations.json");
    for case in golden::cases(&document) {
        let path = golden::string(golden::get(case, "path"));
        let video_path = golden::get(case, "video_path");
        let video = match video_path {
            Value::Null => "null".to_owned(),
            other => format!("\"{}\"", golden::string(other)),
        };
        let source = offline(runnable(&format!(r#"{{"path": "{path}", "video_path": {video}}}"#)));
        let media_type = golden::string(golden::get(case, "media_type"));
        let media_path = golden::string(golden::get(case, "media_path"));
        let mut destination = String::new();
        let mut root = String::new();
        let records = logging::capture(|| {
            let found = source.destination_root(&media_type).clone();
            destination = source.destination_path(&media_path, &found);
            root = found.as_str().to_owned();
        });
        let warnings: Vec<Value> = records
            .iter()
            .filter(|record| record.level == Level::Warning)
            .map(|record| Value::from(record.message.as_str()))
            .collect();
        assert_eq!(root, golden::string(golden::get(case, "root")), "{case:?}");
        assert_eq!(destination, golden::string(golden::get(case, "destination")), "{case:?}");
        assert_eq!(&Value::List(warnings), golden::get(case, "warnings"), "{case:?}");
    }
}

// py: tests/test_twitter.py::test_an_unset_video_path_keeps_everything_in_one_place
#[tokio::test]
async fn an_unset_video_path_keeps_everything_in_one_place() {
    let source = offline(runnable(r#"{"path": "/data/twitter", "video_path": null}"#));

    assert_eq!(source.destination_root("video").as_str(), "/data/twitter");
    assert_eq!(source.destination_root("photo").as_str(), "/data/twitter");
}

// py: tests/test_twitter.py::test_videos_and_gifs_go_to_the_video_path_while_photos_stay
#[tokio::test]
async fn videos_and_gifs_go_to_the_video_path_while_photos_stay() {
    let source = offline(runnable(r#"{"path": "/data/twitter", "video_path": "/media/twitter-videos"}"#));

    // X stores a GIF as a short video, so it follows the videos.
    assert_eq!(source.destination_root("video").as_str(), "/media/twitter-videos");
    assert_eq!(source.destination_root("animated_gif").as_str(), "/media/twitter-videos");
    assert_eq!(source.destination_root("photo").as_str(), "/data/twitter");
    // An unfamiliar type files with the images rather than vanishing.
    assert_eq!(source.destination_root("").as_str(), "/data/twitter");
}

// py: tests/test_twitter.py::test_a_relocated_file_keeps_its_place_within_the_collection
#[tokio::test]
async fn a_relocated_file_keeps_its_place_within_the_collection() {
    let source = offline(runnable(r#"{"path": "/data/twitter", "video_path": "/media/vid"}"#));

    let destination = source.destination_path("/data/twitter/artist/clip.mp4", &PyPath::new("/media/vid"));

    assert_eq!(destination, "/media/vid/artist/clip.mp4");
}

// ---------- ingest ----------

async fn stored_rows(db: &Db) -> Vec<(i64, i32, String, String)> {
    sqlx::query_as("SELECT tweet_id, num, media_type, local_path FROM twitter ORDER BY tweet_id, num;")
        .fetch_all(db.pool())
        .await
        .expect("rows")
}

fn config_at(images: &Path, videos: Option<&Path>) -> TwitterConfig {
    let video = videos.map_or_else(|| "null".to_owned(), |videos| format!("\"{}\"", utf8(videos)));
    runnable(&format!(r#"{{"path": "{}", "video_path": {video}}}"#, utf8(images)))
}

// py: tests/test_twitter.py::test_ingest_inserts_one_row_per_sidecar_and_clears_the_queue
#[tokio::test]
async fn ingest_inserts_one_row_per_sidecar_and_clears_the_queue() {
    let database = database().await;
    let root = tempfile::tempdir().expect("tempdir");
    let first = write_sidecar(&root.path().join("artist"), "a.jpg", &sidecar_payload(r#"{"num": 1}"#));
    let second = write_sidecar(
        &root.path().join("other"),
        "b.mp4",
        &sidecar_payload(r#"{"tweet_id": 999, "num": 1, "type": "video"}"#),
    );
    let source = crawler(
        database.db().clone(),
        config_at(root.path(), None),
        &Arc::default(),
        &Arc::default(),
    );

    let inserted = source.ingest_sidecars().await.expect("ingest");

    assert_eq!(inserted, 2);
    // The sidecar is the queue entry: it goes away only once its row is in.
    assert!(!first.exists());
    assert!(!second.exists());
    assert!(root.path().join("artist/a.jpg").exists());
    let paths: Vec<String> = stored_rows(database.db()).await.into_iter().map(|(.., path)| path).collect();
    assert_eq!(paths, ["other/b.mp4", "artist/a.jpg"]);
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_ingest_moves_videos_to_the_video_path_and_leaves_photos
#[tokio::test]
async fn ingest_moves_videos_to_the_video_path_and_leaves_photos() {
    let database = database().await;
    let root = tempfile::tempdir().expect("tempdir");
    let images = root.path().join("images");
    let videos = root.path().join("videos");
    write_sidecar(
        &images.join("artist"),
        "pic.jpg",
        &sidecar_payload(r#"{"num": 1, "type": "photo"}"#),
    );
    write_sidecar(
        &images.join("artist"),
        "clip.mp4",
        &sidecar_payload(r#"{"tweet_id": 999, "num": 1, "type": "video"}"#),
    );
    write_sidecar(
        &images.join("artist"),
        "loop.mp4",
        &sidecar_payload(r#"{"tweet_id": 1000, "num": 1, "type": "animated_gif"}"#),
    );
    let source = crawler(
        database.db().clone(),
        config_at(&images, Some(&videos)),
        &Arc::default(),
        &Arc::default(),
    );

    assert_eq!(source.ingest_sidecars().await.expect("ingest"), 3);

    // The photo stays put; the video and the GIF move, keeping their author folder.
    assert!(images.join("artist/pic.jpg").exists());
    assert!(!images.join("artist/clip.mp4").exists());
    assert!(videos.join("artist/clip.mp4").exists());
    assert!(videos.join("artist/loop.mp4").exists());
    // Each path is recorded relative to the directory the file actually lives in.
    let mut by_type: Vec<(String, String)> = stored_rows(database.db())
        .await
        .into_iter()
        .map(|(_, _, media_type, path)| (media_type, path))
        .collect();
    by_type.sort();
    assert_eq!(
        by_type,
        [
            ("animated_gif".to_owned(), "artist/loop.mp4".to_owned()),
            ("photo".to_owned(), "artist/pic.jpg".to_owned()),
            ("video".to_owned(), "artist/clip.mp4".to_owned()),
        ]
    );
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_ingest_finishes_a_move_a_previous_run_died_halfway_through
#[tokio::test]
async fn ingest_finishes_a_move_a_previous_run_died_halfway_through() {
    let database = database().await;
    let root = tempfile::tempdir().expect("tempdir");
    let images = root.path().join("images");
    let videos = root.path().join("videos");
    // The sidecar is still queued, but the file was moved before the crash.
    let sidecar = write_sidecar(&images.join("artist"), "clip.mp4", &sidecar_payload(r#"{"type": "video"}"#));
    std::fs::create_dir_all(videos.join("artist")).expect("videos");
    std::fs::rename(images.join("artist/clip.mp4"), videos.join("artist/clip.mp4")).expect("moved");
    let source = crawler(
        database.db().clone(),
        config_at(&images, Some(&videos)),
        &Arc::default(),
        &Arc::default(),
    );

    assert_eq!(source.ingest_sidecars().await.expect("ingest"), 1);
    assert!(!sidecar.exists());
    assert!(videos.join("artist/clip.mp4").exists());
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_ingest_leaves_an_unparseable_sidecar_alone_instead_of_losing_it
#[test]
fn ingest_leaves_an_unparseable_sidecar_alone_instead_of_losing_it() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _entered = runtime.enter();
    let root = tempfile::tempdir().expect("tempdir");
    let broken = root.path().join("artist/a.jpg.json");
    std::fs::create_dir_all(broken.parent().expect("parent")).expect("directory");
    std::fs::write(&broken, "{not json").expect("sidecar");
    // Nothing reaches the database.
    let source = offline(config_at(root.path(), None));

    let mut inserted = None;
    let records = logging::capture(|| inserted = Some(runtime.block_on(source.ingest_sidecars())));

    assert_eq!(inserted.expect("ran").expect("ingest"), 0);
    assert!(broken.exists());
    let warning = &records
        .iter()
        .find(|record| record.level == Level::Warning)
        .expect("a warning")
        .message;
    assert_eq!(
        *warning,
        format!(
            "Skipping unreadable sidecar {}: Expecting property name enclosed in double quotes: line 1 column 2 (char 1)",
            utf8(&broken)
        )
    );
}

#[test]
fn sidecars_are_found_in_path_order() {
    let root = tempfile::tempdir().expect("tempdir");
    for name in ["a-c/x.json", "a/z.json", "a/b/y.json", ".hidden.json", "a/notes.txt"] {
        let path = root.path().join(name);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("directory");
        std::fs::write(path, "{}").expect("file");
    }
    let base = utf8(root.path());

    let found: Vec<String> = sidecars_under(&base)
        .into_iter()
        .map(|path| path[base.len() + 1..].to_owned())
        .collect();

    assert_eq!(found, [".hidden.json", "a/b/y.json", "a/z.json", "a-c/x.json"]);
}

// ---------- run ----------

// py: tests/test_twitter.py::test_an_unconfigured_source_does_nothing_rather_than_launching_gallery_dl
#[tokio::test]
async fn an_unconfigured_source_does_nothing_rather_than_launching_gallery_dl() {
    let runner = Arc::new(FakeGalleryDl::default());
    let mut source = crawler(unused_db(), config(r#"{"username": ""}"#), &runner, &Arc::default());

    source.update().await.expect("nothing to do");

    assert_eq!(runner.calls(), 0);
}

// ---------- transient user lookup failures ----------

fn lines(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|line| (*line).to_owned()).collect()
}

#[test]
fn user_lookups_match_the_recorded_ones() {
    let document = golden::load("twitter/user_lookup.json");
    for case in golden::cases(&document) {
        let code = i32::try_from(golden::int(golden::get(case, "returncode"))).expect("code");
        let errors: Vec<String> = golden::list(golden::get(case, "errors")).iter().map(golden::string).collect();
        assert_eq!(
            Value::Bool(is_transient_user_lookup_failure(code, &errors)),
            *golden::get(case, "transient"),
            "{code} {errors:?}"
        );
    }
}

// py: tests/test_twitter.py::test_a_clean_user_not_found_exit_is_worth_another_attempt
#[test]
fn a_clean_user_not_found_exit_is_worth_another_attempt() {
    assert!(is_transient_user_lookup_failure(4, &lines(&[USER_NOT_FOUND])));
    // gallery-dl refreshes its transaction keys every few hours and says so on stderr.
    assert!(is_transient_user_lookup_failure(4, &lines(&[KEYS_REFRESHED, USER_NOT_FOUND])));
}

// py: tests/test_twitter.py::test_anything_that_got_past_the_lookup_is_not_retried
#[test]
fn anything_that_got_past_the_lookup_is_not_retried() {
    // An authentication failure has its own exit bit and its own message.
    assert!(!is_transient_user_lookup_failure(16, &lines(&[USER_NOT_FOUND])));
    assert!(!is_transient_user_lookup_failure(4 | 16, &lines(&[USER_NOT_FOUND])));
    // A 429 or an API error means the timeline was reached; the lookup succeeded.
    assert!(!is_transient_user_lookup_failure(
        4,
        &lines(&["[twitter][warning] API errors (1/10):", USER_NOT_FOUND])
    ));
    // Some other extraction error, and a clean exit, are not this case either.
    assert!(!is_transient_user_lookup_failure(
        4,
        &lines(&["[twitter][error] Unable to retrieve Tweets from this timeline"])
    ));
    assert!(!is_transient_user_lookup_failure(0, &[]));
}

// py: tests/test_twitter.py::test_a_flaky_lookup_is_run_again_and_the_second_outcome_wins
#[tokio::test]
async fn a_flaky_lookup_is_run_again_and_the_second_outcome_wins() {
    let runner = Arc::new(FakeGalleryDl::new(vec![exits(4, &[USER_NOT_FOUND]), exits(0, &[])]));
    let source = crawler(unused_db(), runnable("{}"), &runner, &Arc::default());

    let outcome = source.run_gallery_dl_with_retry(&lines(&["gallery-dl"])).await.expect("ran");

    assert_eq!(outcome, (0, Vec::new()));
    assert_eq!(runner.calls(), 2);
}

// py: tests/test_twitter.py::test_a_lookup_that_keeps_failing_is_not_retried_forever
#[tokio::test]
async fn a_lookup_that_keeps_failing_is_not_retried_forever() {
    let runner = Arc::new(FakeGalleryDl::new(vec![exits(4, &[USER_NOT_FOUND]), exits(4, &[USER_NOT_FOUND])]));
    let source = crawler(unused_db(), runnable("{}"), &runner, &Arc::default());

    let outcome = source.run_gallery_dl_with_retry(&lines(&["gallery-dl"])).await.expect("ran");

    assert_eq!(outcome, (4, lines(&[USER_NOT_FOUND])));
    assert_eq!(runner.calls(), 1 + USER_LOOKUP_RETRIES as usize);
}

// py: tests/test_twitter.py::test_a_run_that_fails_elsewhere_is_reported_straight_away
#[tokio::test]
async fn a_run_that_fails_elsewhere_is_reported_straight_away() {
    let runner = Arc::new(FakeGalleryDl::new(vec![exits(
        16,
        &["[twitter][error] AuthorizationError: Login required"],
    )]));
    let source = crawler(unused_db(), runnable("{}"), &runner, &Arc::default());

    source.run_gallery_dl_with_retry(&lines(&["gallery-dl"])).await.expect("ran");

    assert_eq!(runner.calls(), 1);
}

#[test]
fn gallery_dl_output_is_logged_and_the_error_tail_kept() {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let _entered = runtime.enter();
    let stderr: Vec<String> = (1..=7).map(|index| format!("[twitter][warning] line {index}")).collect();
    let runner = Arc::new(FakeGalleryDl::new(vec![Run {
        returncode: 4,
        stdout: lines(&["/data/a.jpg", ""]),
        stderr: stderr.clone(),
        files: Vec::new(),
    }]));
    let source = crawler(unused_db(), runnable("{}"), &runner, &Arc::default());
    let mut outcome = None;

    let records = logging::capture(|| outcome = Some(runtime.block_on(source.run_gallery_dl(&lines(&["gallery-dl"])))));

    let (code, errors) = outcome.expect("ran").expect("exit code");
    assert_eq!(code, 4);
    assert_eq!(errors, stderr[2..]);
    let logged: Vec<(Level, &str)> = records.iter().map(|record| (record.level, record.message.as_str())).collect();
    assert_eq!(logged[0], (Level::Info, "Running gallery-dl for me likes"));
    assert_eq!(logged[1], (Level::Debug, "gallery-dl: /data/a.jpg"));
    assert_eq!(logged.len(), 2 + stderr.len());
    assert!(
        logged[2..]
            .iter()
            .all(|(level, message)| *level == Level::Warning && message.starts_with("gallery-dl: "))
    );
}

// ---------- the session ----------

#[test]
fn twid_cookies_match_the_recorded_ones() {
    let document = golden::load("twitter/user_ids.json");
    for case in golden::cases(&document) {
        let cookies = match golden::get(case, "twid") {
            Value::Null => Vec::new(),
            twid => vec![cookie("twid", &golden::string(twid), None)],
        };
        assert_eq!(
            user_id_from_twid(&cookies),
            golden::string(golden::get(case, "user_id")),
            "{case:?}"
        );
    }
}

// py: tests/test_twitter.py::test_the_account_id_is_read_off_the_twid_cookie
#[test]
fn the_account_id_is_read_off_the_twid_cookie() {
    assert_eq!(user_id_from_twid(&signed_in()), "4242");
    assert_eq!(user_id_from_twid(&[cookie("twid", "\"u=77\"", None)]), "77");
    assert_eq!(user_id_from_twid(&[cookie("twid", "garbage", None)]), "");
    assert_eq!(user_id_from_twid(&[]), "");
}

#[test]
fn session_checks_match_the_recorded_ones() {
    let document = golden::load("twitter/verify_session.json");
    let all = signed_in();
    for case in golden::cases(&document) {
        let names: Vec<String> = golden::list(golden::get(case, "cookies")).iter().map(golden::string).collect();
        let cookies: Vec<StoredCookie> = all.iter().filter(|cookie| names.contains(&cookie.name)).cloned().collect();
        let username = golden::string(golden::get(case, "username"));
        let answer = match golden::get(case, "answer") {
            Value::Null => None,
            answer => Some((
                u16::try_from(golden::int(golden::get(answer, "status"))).expect("status"),
                // The one raw-bytes answer recorded is ASCII; the rest are UTF-8 JSON.
                golden::string(golden::get(answer, "body")).into_bytes(),
            )),
        };
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        let mut outcome = None;
        let records = logging::capture(|| {
            outcome = Some(runtime.block_on(async {
                let server = FakeServer::start(move |_| {
                    let (status, body) = answer.clone().expect("no request expected");
                    Answer::bytes(status, &body)
                })
                .await;
                let check = verify_session_at(&server.url("/1.1/account/settings.json"), &cookies, &username, "").await;
                (check, server.received())
            }));
        });
        let (check, received) = outcome.expect("ran");
        let warnings: Vec<String> = records
            .into_iter()
            .filter(|record| record.level == Level::Warning)
            .map(|record| record.message)
            .collect();
        let want = golden::get(case, "check");
        let context = format!("{case:?}");
        assert_eq!(Value::Bool(check.ok), *golden::get(want, "ok"), "{context}");
        assert_eq!(check.account, golden::string(golden::get(want, "account")), "{context}");
        assert_eq!(check.message, golden::string(golden::get(want, "message")), "{context}");
        let want_warnings: Vec<String> = golden::list(golden::get(case, "warnings")).iter().map(golden::string).collect();
        assert_eq!(warnings, want_warnings, "{context}");
        let expected = golden::list(golden::get(case, "requests"));
        assert_eq!(received.len(), expected.len(), "{context}");
        for (got, want) in received.iter().zip(expected) {
            assert_request(got, want, &context);
        }
    }
}

/// The recorded request's headers, all of them and nothing more (bar the transport's own).
fn assert_request(got: &Received, want: &Value, context: &str) {
    assert_eq!(got.method, golden::string(golden::get(want, "method")), "{context}");
    assert_eq!(got.path, "/1.1/account/settings.json", "{context}");
    let Value::Dict(headers) = golden::get(want, "headers") else {
        panic!("headers")
    };
    for (name, value) in headers.iter() {
        let Key::Str(name) = name else { panic!("a header name") };
        assert_eq!(got.header(name), Some(golden::string(value).as_str()), "{context} {name}");
    }
    for name in got.headers.keys() {
        let name = name.as_str();
        let known = ["host", "accept", "accept-encoding", "connection"].contains(&name) || headers.get(name).is_some();
        assert!(known, "{context}: unexpected header {name}");
    }
}

fn settings_answer(status: u16, body: &'static str) -> impl Fn(&Received) -> Answer + Send + Sync + 'static {
    move |_| Answer::json(status, body)
}

// py: tests/test_twitter.py::test_a_login_is_verified_against_the_account_settings
#[tokio::test]
async fn a_login_is_verified_against_the_account_settings() {
    let server = FakeServer::start(settings_answer(200, r#"{"screen_name": "me"}"#)).await;

    let check = verify_session_at(&server.url("/1.1/account/settings.json"), &signed_in(), "Me", "").await;

    assert_eq!((check.ok, check.account.as_str()), (true, "me"));
    let seen = server.received();
    assert_eq!(seen[0].header("x-csrf-token"), Some("csrf"));
    assert_eq!(seen[0].header("cookie"), Some("auth_token=tok; ct0=csrf"));
}

// py: tests/test_twitter.py::test_a_login_into_another_account_than_the_configured_one_is_refused
#[tokio::test]
async fn a_login_into_another_account_than_the_configured_one_is_refused() {
    let server = FakeServer::start(settings_answer(200, r#"{"screen_name": "someone_else"}"#)).await;

    let check = verify_session_at(&server.url("/1.1/account/settings.json"), &signed_in(), "me", "").await;

    assert!(!check.ok);
    assert!(check.message.contains("@someone_else"), "{}", check.message);
}

// py: tests/test_twitter.py::test_a_rejected_session_is_not_a_login
#[tokio::test]
async fn a_rejected_session_is_not_a_login() {
    let server = FakeServer::start(settings_answer(401, r#"{"errors": []}"#)).await;

    let check = verify_session_at(&server.url("/1.1/account/settings.json"), &signed_in(), "me", "").await;

    assert!(!check.ok);
    assert!(check.message.contains("HTTP 401"), "{}", check.message);
}

// py: tests/test_twitter.py::test_without_auth_token_nothing_is_asked_of_x
#[tokio::test]
async fn without_auth_token_nothing_is_asked_of_x() {
    let server = FakeServer::start(|_| panic!("should not call X before a login completed")).await;

    let check = verify_session_at(&server.url("/1.1/account/settings.json"), &signed_in()[1..], "me", "").await;

    assert!(!check.ok);
    assert!(server.received().is_empty());
}

// py: tests/test_twitter.py::test_an_answer_without_a_screen_name_does_not_hold_the_login_up
#[tokio::test]
async fn an_answer_without_a_screen_name_does_not_hold_the_login_up() {
    let server = FakeServer::start(settings_answer(404, r#"{"errors": []}"#)).await;

    let check = verify_session_at(&server.url("/1.1/account/settings.json"), &signed_in(), "me", "").await;

    assert_eq!((check.ok, check.account.as_str()), (true, "me"));
}

#[tokio::test]
async fn x_not_answering_is_unreachable() {
    let check = verify_session_at("http://127.0.0.1:1/1.1/account/settings.json", &signed_in(), "me", "").await;

    assert!(!check.ok);
    assert_eq!(check.message, "X unreachable: ConnectError");
}

// ---------- update ----------

// py: tests/test_twitter.py::test_a_run_without_a_captured_login_fails_with_the_relogin_notice
#[tokio::test]
async fn a_run_without_a_captured_login_fails_with_the_relogin_notice() {
    let database = database().await;
    let root = tempfile::tempdir().expect("tempdir");
    let runner = Arc::new(FakeGalleryDl::default());
    let mut source = crawler(
        database.db().clone(),
        config_at(&root.path().join("twitter"), None),
        &runner,
        &Arc::default(),
    );

    let error = source.update().await.expect_err("not signed in");

    assert_eq!(error.class_name(), "TwitterDownloadError");
    assert_eq!(error.notification_dedupe_key(), Some("twitter:auth"));
    assert!(error.message().contains("登录态"), "{}", error.message());
    assert_eq!(runner.calls(), 0);
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_the_stored_cookies_reach_gallery_dl_and_a_clean_run_records_the_login_valid
#[tokio::test]
async fn the_stored_cookies_reach_gallery_dl_and_a_clean_run_records_the_login_valid() {
    let database = database().await;
    let db = database.db();
    sign_in(db, &signed_in()).await;
    site_sessions::record_check(db, SESSION_SITE, false, "stale").await.expect("check");
    let root = tempfile::tempdir().expect("tempdir");
    let runner = Arc::new(FakeGalleryDl::new(vec![Run::default()]));
    let mut source = crawler(db.clone(), config_at(&root.path().join("twitter"), None), &runner, &Arc::default());

    source.update().await.expect("a clean run");

    let written = runner.cookie_files.lock().expect("cookies").clone();
    let rows: Vec<Vec<&str>> = written[0]
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| line.split('\t').collect())
        .collect();
    assert!(
        rows.contains(&vec![".x.com", "TRUE", "/", "TRUE", "1900000000", "auth_token", "tok"]),
        "{rows:?}"
    );
    assert!(rows.contains(&vec![".x.com", "TRUE", "/", "TRUE", "0", "ct0", "csrf"]), "{rows:?}");
    assert_eq!(stored_status(db).await, ("valid".to_owned(), String::new()));
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_a_session_gallery_dl_cannot_authenticate_with_is_marked_invalid
#[tokio::test]
async fn a_session_gallery_dl_cannot_authenticate_with_is_marked_invalid() {
    let database = database().await;
    let db = database.db();
    sign_in(db, &signed_in()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let runner = Arc::new(FakeGalleryDl::new(vec![exits(
        16,
        &["[twitter][error] AuthorizationError: Login required"],
    )]));
    let mut source = crawler(db.clone(), config_at(&root.path().join("twitter"), None), &runner, &Arc::default());

    let error = source.update().await.expect_err("auth failure");

    assert_eq!(error.notification_dedupe_key(), Some("twitter:auth"));
    assert_eq!(stored_status(db).await.0, "invalid");
    database.drop_database().await;
}

// py: tests/test_twitter.py::test_update_still_raises_once_the_lookup_retry_is_spent
#[tokio::test]
async fn update_still_raises_once_the_lookup_retry_is_spent() {
    let database = database().await;
    sign_in(database.db(), &signed_in()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let runner = Arc::new(FakeGalleryDl::new(vec![exits(4, &[USER_NOT_FOUND]), exits(4, &[USER_NOT_FOUND])]));
    let mut source = crawler(
        database.db().clone(),
        config_at(&root.path().join("twitter"), None),
        &runner,
        &Arc::default(),
    );

    let error = source.update().await.expect_err("still failing");

    assert_eq!(runner.calls(), 2);
    assert!(error.message().contains("Requested user could not be found"), "{}", error.message());
    assert_eq!(error.notification_dedupe_key(), Some("twitter:exit:4"));
    database.drop_database().await;
}

#[tokio::test]
async fn a_backfill_ingests_notifies_and_makes_later_runs_incremental() {
    let database = database().await;
    let db = database.db();
    sign_in(db, &signed_in()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let images = root.path().join("twitter");
    let sidecar = pyjson::dumps(&Value::Dict(sidecar_payload(r#"{"num": 1}"#)), &DumpOptions::python_default()).expect("JSON");
    let runner = Arc::new(FakeGalleryDl::new(vec![
        Run {
            files: vec![
                ("artist/a.jpg".to_owned(), b"binary".to_vec()),
                ("artist/a.jpg.json".to_owned(), sidecar.into_bytes()),
            ],
            ..Run::default()
        },
        Run::default(),
    ]));
    let notifier = Arc::new(RecordingNotifier::default());
    let mut source = crawler(
        db.clone(),
        runnable(&format!(r#"{{"path": "{}", "abort_after": 9}}"#, utf8(&images))),
        &runner,
        &notifier,
    );

    source.update().await.expect("the backfill");
    source.update().await.expect("an incremental run");

    let commands = runner.commands.lock().expect("commands").clone();
    assert_eq!(option(&commands[0], "extractor.twitter.skip"), None);
    assert_eq!(option(&commands[1], "extractor.twitter.skip"), Some("abort:9"));
    assert_eq!(
        after(&commands[0], "--download-archive"),
        format!("{}/.gallery-dl-archive.db", utf8(&images))
    );
    assert_eq!(
        stored_rows(db).await,
        [(1_234_567_890_123_456_789, 1, "photo".to_owned(), "artist/a.jpg".to_owned())]
    );
    let sent = notifier.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].header, "X");
    assert_eq!(sent[0].title, "Likes update completed");
    assert_eq!(sent[0].body, "Downloaded 1 files from liked tweets.");
    assert_eq!(
        sent[0].payload.clone().map(Value::Dict),
        Some(json(r#"{"downloaded": 1, "backfilling": true, "username": "me"}"#))
    );
    database.drop_database().await;
}

#[tokio::test]
async fn a_failed_exit_names_the_error_tail() {
    let database = database().await;
    sign_in(database.db(), &signed_in()).await;
    let root = tempfile::tempdir().expect("tempdir");
    let runner = Arc::new(FakeGalleryDl::new(vec![
        exits(1, &["[twitter][error] one", "[twitter][error] two"]),
        exits(8, &[]),
    ]));
    let mut source = crawler(
        database.db().clone(),
        config_at(&root.path().join("twitter"), None),
        &runner,
        &Arc::default(),
    );

    let first = source.update().await.expect_err("exit 1");
    let second = source.update().await.expect_err("exit 8");

    assert_eq!(
        first.message(),
        "gallery-dl exited with code 1: [twitter][error] one | [twitter][error] two"
    );
    assert_eq!(first.notification_dedupe_key(), Some("twitter:exit:1"));
    assert_eq!(second.message(), "gallery-dl exited with code 8");
    database.drop_database().await;
}

// ---------- registration ----------

// py: tests/test_twitter.py::test_the_job_is_registered_and_parked_until_configured
#[tokio::test]
async fn the_job_is_registered_and_parked_until_configured() {
    let mut settings = Settings::default();
    settings.web.twitter.schedule.enabled = true;

    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "twitter").expect("a twitter job");

    assert_eq!(job.name, "X");
    assert_eq!(job.section, "web.twitter");
    assert_eq!(job.required_commands, ["gallery-dl"]);
    let db = unused_db();
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains(JOB_KEY));
    // Enabled in the UI but with no username, so it stays out of the scheduler.
    assert!(!job.enabled);
    assert!(job.missing_fields.iter().any(|field| field == "username"));
}
