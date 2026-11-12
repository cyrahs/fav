//! Replays `goldens/post_links/`: every pure function of the post-link parsers
//! against Python's answers (`parsing.json`), every read and download against a scripted
//! web (`fetch.json`: the requests made, the answer, the log lines and the session checks),
//! and the constants (`constants.json`). Errors are `{"error": "Class: message"}`.
//!
//! The fake web serves plain HTTP only, so NGA's and Zhihu lens's fixed `https://` URLs
//! are pointed at `http://` here and mapped back when the requests are compared.

use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use fav_core::golden;
use fav_core::logging;
use fav_core::pyjson::{self, Dict, DumpOptions, Value};
use fav_db::SettingsStore;
use fav_runtime::BoxFuture;
use fav_runtime::site_sessions::{SiteSession, StoredCookie};

use crate::nga_post::{self, NgaError, SessionCheck};
use crate::post_links::{self, Endpoints, PostClient, PostError, PostLinks, SessionStore};
use crate::pydecode::PyError;
use crate::testutil::{FakeWeb, Reply, Seen};
use crate::tieba_post::{self, TiebaError};
use crate::tikhub::{self, TikHubAccount, TikHubError};
use crate::wechat_article::{self, ArticleLink, ArticlePage, PostVideo};
use crate::zhihu_post::{self, ZhihuError, ZhihuKind, ZhihuLink, ZhihuPost, ZhihuVideo};

// ---------- Python's `_plain` of each answer ----------

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

fn strings(values: &[String]) -> Value {
    Value::List(values.iter().map(|value| text(value)).collect())
}

fn optional<T>(value: Option<T>, plain: impl FnOnce(T) -> Value) -> Value {
    value.map_or(Value::Null, plain)
}

fn time(value: Option<DateTime<Utc>>) -> Value {
    optional(value, |time| Value::Str(time.to_rfc3339()))
}

fn hex(bytes: &[u8]) -> Value {
    let digits = bytes.iter().fold(String::new(), |mut digits, byte| {
        let _ = write!(digits, "{byte:02x}");
        digits
    });
    dict(vec![("hex", Value::Str(digits))])
}

fn unhex(value: &Value) -> Vec<u8> {
    let digits = golden::string(golden::get(value, "hex"));
    (0..digits.len())
        .step_by(2)
        .map(|at| u8::from_str_radix(&digits[at..at + 2], 16).expect("hex"))
        .collect()
}

fn link(link: &ArticleLink) -> Value {
    dict(vec![
        ("url", text(&link.url)),
        ("title", text(&link.title)),
        ("description", text(&link.description)),
        ("account", text(&link.account)),
        ("show_type", text(&link.show_type)),
        ("picture_count", Value::Int(link.picture_count)),
        ("cover_url", text(&link.cover_url)),
    ])
}

fn video(video: &PostVideo) -> Value {
    dict(vec![("url", text(&video.url)), ("cover_url", text(&video.cover_url))])
}

fn page(page: &ArticlePage) -> Value {
    dict(vec![
        ("title", text(&page.title)),
        ("account", text(&page.account)),
        ("published_at", time(page.published_at)),
        ("images", strings(&page.images)),
        ("picture_count", Value::Int(page.picture_count)),
        ("videos", Value::List(page.videos.iter().map(video).collect())),
    ])
}

fn zhihu_link(link: &ZhihuLink) -> Value {
    dict(vec![("kind", text(link.kind.as_str())), ("id", text(&link.id))])
}

fn zhihu_video(video: &ZhihuVideo) -> Value {
    dict(vec![
        ("id", text(&video.id)),
        ("fallback_url", text(&video.fallback_url)),
        ("cover_url", text(&video.cover_url)),
    ])
}

fn zhihu_videos(videos: &[ZhihuVideo]) -> Value {
    Value::List(videos.iter().map(zhihu_video).collect())
}

fn zhihu_post(post: &ZhihuPost) -> Value {
    dict(vec![
        ("link", zhihu_link(&post.link)),
        ("title", text(&post.title)),
        ("author", text(&post.author)),
        ("published_at", time(post.published_at)),
        ("images", strings(&post.images)),
        ("videos", zhihu_videos(&post.videos)),
    ])
}

fn error(class: &str, message: &str) -> Value {
    dict(vec![("error", Value::Str(format!("{class}: {message}")))])
}

fn py_error(failure: &PyError) -> Value {
    error(&failure.class, &failure.message)
}

fn answer<T>(result: Result<T, PyError>, plain: impl FnOnce(T) -> Value) -> Value {
    result.map_or_else(|failure| py_error(&failure), plain)
}

fn tieba_error(failure: &TiebaError) -> Value {
    match failure {
        TiebaError::Tieba(message) => error("TiebaError", message),
        TiebaError::Other(failure) => py_error(failure),
    }
}

fn nga_error(failure: &NgaError) -> Value {
    match failure {
        NgaError::Nga(message) => error("NgaError", message),
        NgaError::Login(message) => error("NgaLoginError", message),
        NgaError::Other(failure) => py_error(failure),
    }
}

fn zhihu_error(failure: &ZhihuError) -> Value {
    match failure {
        // An outage is a `ZhihuError` in Python (D185).
        ZhihuError::Zhihu(message) | ZhihuError::Outage(message) => error("ZhihuError", message),
        ZhihuError::Account(message) => error("ZhihuAccountError", message),
        ZhihuError::Other(failure) => py_error(failure),
    }
}

fn post_error(failure: &PostError) -> Value {
    match failure {
        PostError::Link(failure) => {
            let mut pairs = vec![
                ("error", Value::Str(format!("{}: {}", failure.class_name(), failure.message))),
                ("needs", text(failure.needs.as_str())),
            ];
            if failure.captcha {
                pairs.push(("captcha", Value::Bool(true)));
            }
            dict(pairs)
        }
        PostError::Other(failure) => py_error(failure),
    }
}

fn tikhub_error(failure: &TikHubError) -> Value {
    dict(vec![
        ("error", Value::Str(format!("TikHubError: {}", failure.message))),
        ("code", text(failure.code)),
    ])
}

fn account(account: &TikHubAccount) -> Value {
    dict(vec![
        ("key_name", text(&account.key_name)),
        ("balance", Value::Float(account.balance)),
        ("free_credit", Value::Float(account.free_credit)),
    ])
}

fn session_check(check: &SessionCheck) -> Value {
    dict(vec![
        ("ok", Value::Bool(check.ok)),
        ("account", text(&check.account)),
        ("message", text(&check.message)),
    ])
}

// ---------- arguments ----------

/// `text or ''`.
fn text_arg(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        other => golden::string(other),
    }
}

fn field(value: &Value, key: &str) -> String {
    value.get(key).map(text_arg).unwrap_or_default()
}

fn link_arg(value: &Value) -> ArticleLink {
    ArticleLink {
        url: field(value, "url"),
        title: field(value, "title"),
        description: field(value, "description"),
        account: field(value, "account"),
        show_type: field(value, "show_type"),
        picture_count: value.get("picture_count").map_or(0, golden::int),
        cover_url: field(value, "cover_url"),
    }
}

fn dict_arg(value: &Value) -> &Dict {
    value.as_dict().unwrap_or_else(|| panic!("expected a dict, got {value:?}"))
}

fn kind_arg(value: &Value) -> ZhihuKind {
    ZhihuKind::from_name(&golden::string(value)).expect("a Zhihu kind")
}

fn cookie(name: &str, value: &str) -> StoredCookie {
    StoredCookie {
        name: name.to_owned(),
        value: value.to_owned(),
        domain: ".nga.cn".to_owned(),
        path: "/".to_owned(),
        expires: None,
    }
}

/// A `{name: value}` object as cookies, in its order.
fn cookies_arg(value: &Value) -> Vec<StoredCookie> {
    dict_arg(value)
        .iter()
        .map(|(name, value)| cookie(&pyjson_key(name), &golden::string(value)))
        .collect()
}

fn pyjson_key(key: &pyjson::Key) -> String {
    match key {
        pyjson::Key::Str(name) => name.clone(),
        other => panic!("expected a string key, got {other:?}"),
    }
}

fn pairs(pairs: &[(String, String)]) -> Value {
    let mut dict = Dict::new();
    for (key, value) in pairs {
        dict.insert(key.as_str(), text(value));
    }
    Value::Dict(dict)
}

// ---------- parsing.json ----------

#[allow(clippy::too_many_lines)] // reason: one arm per function under test, as the golden lists them
fn parse(name: &str, args: &[Value]) -> Value {
    let arg = |index: usize| text_arg(&args[index]);
    let opt_link = |found: Option<ArticleLink>| optional(found, |found| link(&found));
    let opt_text = |found: Option<String>| optional(found, Value::Str);
    match name {
        "wechat_article._tag" => Value::Str(wechat_article::tag(&arg(0), &arg(1))),
        "wechat_article.appmsg_link" => {
            let types: Vec<String> = golden::list(&args[1]).iter().map(golden::string).collect();
            let types: Vec<&str> = types.iter().map(String::as_str).collect();
            answer(wechat_article::appmsg_link(&arg(0), &types), opt_link)
        }
        "wechat_article.parse_article_link" => answer(wechat_article::parse_article_link(&arg(0)), opt_link),
        "wechat_article.cover_only_page" => optional(wechat_article::cover_only_page(&link_arg(&args[0])), |found| page(&found)),
        "wechat_article.canonical_url" => answer(wechat_article::canonical_url(&arg(0)), Value::Str),
        "wechat_article._js_unescape" => Value::Str(wechat_article::js_unescape(&arg(0))),
        "wechat_article.picture_list" => strings(&wechat_article::picture_list(&arg(0))),
        "wechat_article.parse_article_page" => answer(wechat_article::parse_article_page(&arg(0)), |found| page(&found)),
        "wechat_article.original_image_url" => answer(wechat_article::original_image_url(&arg(0)), Value::Str),
        "wechat_article.image_extension" => answer(wechat_article::image_extension(&arg(0), &arg(1)), text),
        "wechat_article.is_captcha" => answer(wechat_article::is_captcha(&arg(0)), Value::Bool),
        "tieba_post.thread_id" => answer(tieba_post::thread_id(&arg(0)), opt_text),
        "tieba_post.parse_tieba_link" => answer(tieba_post::parse_tieba_link(&arg(0)), opt_link),
        "tieba_post._signed" => {
            let params: Vec<(String, String)> = dict_arg(&args[0])
                .iter()
                .map(|(key, value)| (pyjson_key(key), golden::string(value)))
                .collect();
            pairs(&tieba_post::signed(&params))
        }
        "tieba_post._images" => answer(tieba_post::images(dict_arg(&args[0])), |found| strings(&found)),
        "tieba_post._videos" => answer(tieba_post::videos(dict_arg(&args[0]), dict_arg(&args[1])), |found| {
            Value::List(found.iter().map(video).collect())
        }),
        "nga_post.nga_thread_id" => answer(nga_post::nga_thread_id(&arg(0)), opt_text),
        "nga_post.parse_nga_link" => answer(nga_post::parse_nga_link(&arg(0)), opt_link),
        "nga_post.original_url" => answer(nga_post::original_url(&arg(0)), opt_text),
        "nga_post.floor_images" => answer(nga_post::floor_images(dict_arg(&args[0])), |found| strings(&found)),
        "nga_post._error_text" => Value::Str(nga_post::error_text(Some(&args[0]))),
        "nga_post._timestamp" => answer(nga_post::timestamp(Some(&args[0])), time),
        "nga_post.login_cookies" => {
            let cookies: Vec<StoredCookie> = golden::list(&args[0])
                .iter()
                .map(|pair| {
                    let pair = golden::list(pair);
                    cookie(&golden::string(&pair[0]), &golden::string(&pair[1]))
                })
                .collect();
            pairs(&nga_post::login_cookies(&cookies))
        }
        "zhihu_post.parse_zhihu_url" => answer(zhihu_post::parse_zhihu_url(&arg(0)), |found| {
            optional(found, |found| zhihu_link(&found))
        }),
        "zhihu_post.ZhihuLink.url" => Value::Str(ZhihuLink::new(kind_arg(&args[0]), arg(1)).url()),
        "zhihu_post.content_images" => answer(zhihu_post::content_images(&arg(0)), |found| strings(&found)),
        "zhihu_post.content_videos" => answer(zhihu_post::content_videos(&arg(0)), |found| zhihu_videos(&found)),
        "zhihu_post._best_playlist_url" => answer(zhihu_post::best_playlist_url(Some(&args[0])), Value::Str),
        "zhihu_post._pin_media" => answer(zhihu_post::pin_media(Some(&args[0])), |(images, videos, found)| {
            Value::List(vec![strings(&images), zhihu_videos(&videos), Value::Str(found)])
        }),
        "zhihu_post._timestamp" => answer(zhihu_post::timestamp(Some(&args[0])), time),
        "zhihu_post._short" => {
            let limit = usize::try_from(golden::int(&args[1])).expect("a byte count");
            Value::Str(zhihu_post::short(&arg(0), limit))
        }
        "zhihu_post.parse_post" => {
            let target = ZhihuLink::new(kind_arg(&args[0]), arg(1));
            answer(zhihu_post::parse_post(&target, dict_arg(&args[2])), |found| zhihu_post(&found))
        }
        "post_links.link_for_url" => {
            let (title, description) = if args.len() > 1 {
                (arg(1), arg(2))
            } else {
                (String::new(), String::new())
            };
            answer(post_links::link_for_url(&arg(0), &title, &description), opt_link)
        }
        "post_links.find_link" | "post_links.first_url" => {
            let texts: Vec<String> = args.iter().map(text_arg).collect();
            let texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            if name == "post_links.find_link" {
                answer(post_links::find_link(&texts), opt_link)
            } else {
                Value::Str(post_links::first_url(&texts))
            }
        }
        "post_links.unsupported_reason" => Value::Str(post_links::unsupported_reason(&arg(0), &arg(1))),
        "post_links.shared_link" => answer(post_links::shared_link(&arg(0)), opt_link),
        "post_links.site_name" => answer(post_links::site_name(&ArticleLink::new(arg(0), "", "")), text),
        "post_links.sniff_extension" => optional(post_links::sniff_extension(&unhex(&args[0])), text),
        "post_links.is_mp4" => Value::Bool(post_links::is_mp4(&unhex(&args[0]))),
        other => panic!("no replay for {other}"),
    }
}

#[test]
fn every_parser_answers_as_python_does() {
    let document = golden::load("post_links/parsing.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "fn"));
        let args = golden::list(golden::get(case, "args"));
        assert_eq!(&parse(&name, args), golden::get(case, "result"), "{name} {args:?}");
    }
}

#[test]
fn the_constants_are_pythons() {
    let document = golden::load("post_links/constants.json");
    let constants = &golden::cases(&document)[0];
    let expect = |key: &str, value: Value| assert_eq!(golden::get(constants, key), &value, "{key}");
    let list = |values: &[&str]| Value::List(values.iter().map(|value| text(value)).collect());
    expect("post_links.USER_AGENT", text(post_links::USER_AGENT));
    #[allow(clippy::cast_precision_loss)] // reason: a small whole number of seconds
    expect("post_links.VIDEO_READ_SECONDS", Value::Float(post_links::VIDEO_READ_SECONDS as f64));
    #[allow(clippy::cast_precision_loss)] // reason: a small whole number of seconds
    expect(
        "post_links._REQUEST_TIMEOUT_SECONDS",
        Value::Float(post_links::REQUEST_TIMEOUT_SECONDS as f64),
    );
    expect("zhihu_post.USER_AGENT", text(zhihu_post::USER_AGENT));
    let mut hosts = nga_post::NGA_HOSTS.to_vec();
    hosts.sort_unstable();
    expect("nga_post.NGA_HOSTS", list(&hosts));
    expect("nga_post.READ_URL", text(nga_post::READ_URL));
    expect("nga_post._SESSION_CHECK_URL", text(nga_post::SESSION_CHECK_URL));
    expect("nga_post.LOGIN_COOKIES", list(&nga_post::LOGIN_COOKIES));
    expect("nga_post.SESSION_SITE", text(nga_post::SESSION_SITE));
    expect("nga_post.LOGIN_URL", text(nga_post::LOGIN_URL));
    expect("nga_post.SESSION_COOKIE_URLS", list(&nga_post::SESSION_COOKIE_URLS));
    expect("nga_post.SESSION_COOKIE", text(nga_post::SESSION_COOKIE));
    expect("nga_post.RELOGIN_HINT", text(nga_post::RELOGIN_HINT));
    expect("wechat_article.LINK_SHARE_TYPE", text(wechat_article::LINK_SHARE_TYPE));
    expect("wechat_article.VIDEO_SHARE_TYPE", text(wechat_article::VIDEO_SHARE_TYPE));
    expect("wechat_article.IMAGE_POST", text(wechat_article::IMAGE_POST));
    expect("wechat_article.CAPTCHA_PATHS", list(&wechat_article::CAPTCHA_PATHS));
    expect(
        "tikhub.ACCOUNT_STATUSES",
        Value::List(
            tikhub::ACCOUNT_STATUSES
                .iter()
                .map(|status| Value::Int(i128::from(*status)))
                .collect(),
        ),
    );
}

// ---------- fetch.json ----------

/// The fixed `https://` hosts the replay serves over plain HTTP.
const HTTPS_HOSTS: [&str; 2] = ["bbs.nga.cn", "lens.zhihu.com"];

fn endpoints() -> Endpoints {
    Endpoints {
        nga_read: nga_post::READ_URL.replacen("https://", "http://", 1),
        lens: zhihu_post::LENS_API.replacen("https://", "http://", 1),
        ..Endpoints::default()
    }
}

/// The URL Python's mock transport saw for a request the fake web saw.
fn python_url(url: &str) -> String {
    for host in HTTPS_HOSTS {
        if let Some(rest) = url.strip_prefix(&format!("http://{host}/")) {
            return format!("https://{host}/{rest}");
        }
    }
    url.to_owned()
}

/// `httpx.Response(...)` for a scripted route, with the content type httpx adds itself.
fn reply(route: &Value) -> Reply {
    let status = u16::try_from(golden::int(golden::get(route, "status"))).expect("status");
    let mut headers: Vec<(String, String)> = dict_arg(golden::get(route, "headers"))
        .iter()
        .map(|(name, value)| (pyjson_key(name), golden::string(value)))
        .collect();
    let has_type = headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("content-type"));
    let (body, content_type) = if let Some(json) = route.get("json") {
        (
            pyjson::dumps(json, &DumpOptions::compact()).expect("dumpable").into_bytes(),
            Some("application/json"),
        )
    } else if let Some(body) = route.get("text") {
        (golden::string(body).into_bytes(), Some("text/plain; charset=utf-8"))
    } else if let Some(body) = route.get("body_b64") {
        (base64_decode(&golden::string(body)), None)
    } else {
        (Vec::new(), None)
    };
    if let Some(content_type) = content_type.filter(|_| !has_type) {
        headers.push(("content-type".to_owned(), content_type.to_owned()));
    }
    Reply::Respond { status, headers, body }
}

fn base64_decode(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut buffer = 0_u32;
    let mut bits = 0;
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let index = ALPHABET.iter().position(|letter| *letter == byte).expect("base64");
        buffer = (buffer << 6) | u32::try_from(index).expect("small");
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((buffer >> bits) & 0xff).expect("a byte"));
        }
    }
    out
}

/// The request as the golden records it.
fn recorded(seen: &Seen) -> Value {
    let mut headers = Dict::new();
    for name in ["user-agent", "authorization", "cookie", "referer", "content-type"] {
        if let Some(value) = seen.header(name) {
            headers.insert(name, text(value));
        }
    }
    dict(vec![
        ("method", text(&seen.method)),
        ("url", Value::Str(python_url(&seen.url))),
        ("headers", Value::Dict(headers)),
        ("body", Value::Str(String::from_utf8_lossy(&seen.body).into_owned())),
    ])
}

/// `_Sessions`: an optional NGA login, and the checks recorded.
struct Sessions {
    login: Option<Vec<StoredCookie>>,
    checks: Mutex<Vec<Value>>,
}

impl SessionStore for Sessions {
    fn load<'a>(&'a self, site: &'a str) -> BoxFuture<'a, Result<Option<SiteSession>, fav_runtime::Error>> {
        Box::pin(async move {
            Ok(self.login.clone().map(|cookies| SiteSession {
                site: site.to_owned(),
                cookies,
                account: "42".to_owned(),
                status: "valid".to_owned(),
                message: String::new(),
                expires_at: None,
                signed_in_at: None,
                checked_at: None,
                extra: Vec::new(),
            }))
        })
    }

    fn record_check<'a>(&'a self, site: &'a str, ok: bool, message: &'a str) -> BoxFuture<'a, Result<(), fav_runtime::Error>> {
        self.checks
            .lock()
            .expect("lock")
            .push(Value::List(vec![text(site), Value::Bool(ok), text(message)]));
        Box::pin(async { Ok(()) })
    }
}

fn zhihu_args(args: &Value) -> (ZhihuLink, String, String) {
    (
        ZhihuLink::new(kind_arg(golden::get(args, "kind")), field(args, "id")),
        field(args, "api_url"),
        field(args, "token"),
    )
}

async fn call(case: &Value, web: &FakeWeb, sessions: Arc<Sessions>, folder: &std::path::Path) -> Value {
    let args = golden::get(case, "args");
    let name = golden::string(golden::get(case, "call"));
    let proxy = Some(web.proxy.as_str());
    let client = PostClient::with(proxy, endpoints()).expect("client");
    let link = || link_arg(golden::get(args, "link"));
    match name.as_str() {
        "nga_post.verify_session" => {
            let check_url = "http://bbs.nga.cn/nuke.php";
            session_check(&nga_post::verify_session_at(check_url, proxy, &cookies_arg(golden::get(args, "cookies"))).await)
        }
        "tikhub.check_account" => {
            let cfg = fav_core::settings::TikHub {
                api_url: field(args, "api_url"),
                token: field(args, "token"),
            };
            tikhub::check_account_via(&cfg, proxy)
                .await
                .map_or_else(|failure| tikhub_error(&failure), |found| account(&found))
        }
        "tieba_post.fetch_tieba_post" => tieba_post::fetch_tieba_post(&client, &link())
            .await
            .map_or_else(|failure| tieba_error(&failure), |found| page(&found)),
        "nga_post.fetch_nga_post" => {
            let cookies: Vec<(String, String)> = dict_arg(golden::get(args, "cookies"))
                .iter()
                .map(|(key, value)| (pyjson_key(key), golden::string(value)))
                .collect();
            nga_post::fetch_nga_post(&client, &link(), &cookies)
                .await
                .map_or_else(|failure| nga_error(&failure), |found| page(&found))
        }
        "zhihu_post.fetch_post" => {
            let (target, api_url, token) = zhihu_args(args);
            zhihu_post::fetch_post(&client, &target, &api_url, &token)
                .await
                .map_or_else(|failure| zhihu_error(&failure), |found| zhihu_post(&found))
        }
        "zhihu_post.fetch_page" => {
            let (target, api_url, token) = zhihu_args(args);
            zhihu_post::fetch_page(&client, &target, &api_url, &token)
                .await
                .map_or_else(|failure| zhihu_error(&failure), |found| page(&found))
        }
        "zhihu_post.resolve_video" => {
            let target = golden::get(args, "video");
            let target = ZhihuVideo {
                id: field(target, "id"),
                fallback_url: field(target, "fallback_url"),
                cover_url: field(target, "cover_url"),
            };
            answer(zhihu_post::resolve_video(&client, &target).await, Value::Str)
        }
        "post_links.fetch_page" => {
            let mut settings = fav_db::settings_store::default_settings();
            let tikhub = args.get("tikhub");
            settings.services.tikhub.api_url = tikhub.map_or_else(|| "http://api.tikhub.dev".to_owned(), |cfg| field(cfg, "api_url"));
            settings.services.tikhub.token = tikhub.map(|cfg| field(cfg, "token")).unwrap_or_default();
            let links = PostLinks::new(client, Arc::new(SettingsStore::pinned(settings)), sessions);
            links.fetch_page(&link()).await.map_or_else(
                |failure| post_error(&failure),
                |(found, html)| Value::List(vec![page(&found), Value::Str(html)]),
            )
        }
        "post_links.fetch_image" => answer(client.fetch_image(&field(args, "url")).await, |found| {
            optional(found, |(body, extension)| Value::List(vec![hex(&body), text(extension)]))
        }),
        "post_links.fetch_video" => {
            let target = golden::get(args, "video");
            let target = PostVideo::new(field(target, "url"), field(target, "cover_url"));
            let partial = folder.join("video.mp4.part");
            answer(client.fetch_video(&target, &partial).await, |ok| {
                let file = std::fs::read(&partial).ok();
                dict(vec![("ok", Value::Bool(ok)), ("file", optional(file, |file| hex(&file)))])
            })
        }
        other => panic!("no replay for {other}"),
    }
}

fn run(case: &Value) -> (Value, Vec<Value>, Vec<logging::Record>, Vec<Value>) {
    let routes: Vec<(String, Reply)> = dict_arg(golden::get(case, "routes"))
        .iter()
        .map(|(url, route)| (pyjson_key(url), reply(route)))
        .collect();
    let login = match golden::get(case, "args").get("login") {
        None | Some(Value::Null) => None,
        Some(login) => Some(cookies_arg(login)),
    };
    let sessions = Arc::new(Sessions {
        login,
        checks: Mutex::new(Vec::new()),
    });
    let folder = tempfile::tempdir().expect("temporary folder");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    let mut outcome = None;
    let records = logging::capture(|| {
        outcome = Some(runtime.block_on(async {
            let web = FakeWeb::start(move |seen| {
                let url = python_url(&seen.url);
                routes.iter().find(|(route, _)| *route == url).map_or_else(
                    || Reply::Respond {
                        status: 404,
                        headers: vec![("content-type".to_owned(), "text/plain; charset=utf-8".to_owned())],
                        body: b"not scripted".to_vec(),
                    },
                    |(_, reply)| reply.clone(),
                )
            })
            .await;
            let result = call(case, &web, Arc::clone(&sessions), folder.path()).await;
            (result, web.seen().iter().map(recorded).collect::<Vec<_>>())
        }));
    });
    let (result, requests) = outcome.expect("ran");
    let checks = sessions.checks.lock().expect("lock").clone();
    (result, requests, records, checks)
}

#[test]
fn every_read_and_download_goes_as_pythons_did() {
    let document = golden::load("post_links/fetch.json");
    for case in golden::cases(&document) {
        let name = golden::string(golden::get(case, "name"));
        let (result, requests, records, checks) = run(case);
        assert_eq!(&result, golden::get(case, "result"), "{name}: result");
        assert_eq!(&Value::List(requests), golden::get(case, "requests"), "{name}: requests");
        let logs: Vec<Value> = records
            .iter()
            .map(|record| Value::List(vec![text(&record.name), text(record.level.name()), text(&record.message)]))
            .collect();
        assert_eq!(&Value::List(logs), golden::get(case, "logs"), "{name}: logs");
        assert_eq!(&Value::List(checks), golden::get(case, "checks"), "{name}: checks");
    }
}
