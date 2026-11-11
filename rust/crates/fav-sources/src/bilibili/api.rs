//! The five Bilibili web API calls the crawl makes, as bilibili-api-python 17.1.1 makes
//! them: the watch-later list and its clearing, a favourite list page, a video's info and
//! its detail.
//!
//! Python origin: `bilibili_api.user.get_toview_list`, `user.clear_toview_list`,
//! `favorite_list.FavoriteList.get_content`, `video.Video.get_info` and `get_detail`, all
//! through `bilibili_api.utils.network.Api` on its httpx client. The requests (URL,
//! query order, cookies, form body) and what the library makes of an answer (the `code`
//! and `OK` checks, the `data`/`result` unwrapping, the `-403` retries, the exception
//! texts) are held to `goldens/bilibili/api_requests.json` and `api_answers.json`.
//!
//! Not carried over (`DIVERGENCES.md` D62): the library's process-wide httpx client keeps
//! every `Set-Cookie` an API answer sends and sends it back on later calls, for every
//! account; here each call sends only the account's own four cookies.

use std::fmt::Write as _;
use std::time::Duration;

use fav_core::pyjson::{Dict, Value};
use fav_net::httpx::HttpError;
use fav_net::reqwest::{self, Method, header};
use fav_runtime::pystr::{repr_value, str_value};
use fav_runtime::source::JobError;

use crate::notifier::HttpJobError as _;
use crate::pydecode::{self, PyError, py_eq_int, urlencode};

pub(crate) const API_BASE: &str = "https://api.bilibili.com";
const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
const REFERER: &str = "https://www.bilibili.com";
/// `request_settings` timeout: five seconds for every phase.
const TIMEOUT: Duration = Duration::from_secs(5);
/// `request_settings.get_wbi_retry_times()`: attempts while the answer is code -403.
const WBI_RETRY_TIMES: usize = 3;
const WBI_RETRY_CODE: i128 = -403;
const BVID_ERROR: &str = "bvid 提供错误，必须是以 BV 开头的纯字母和数字组成的 12 位字符串（大小写敏感）。";
const BVID_TABLE: &[u8; 58] = b"FcwAPNKTMug3GV5Lj7EJnHpWsx4tb8haYeviqBz6rkCy12mUSDQX9RdoZf";
const BVID_MAX_AID: i128 = 2_251_799_813_685_247;
const BVID_XOR: i128 = 23_442_827_791_579;

/// A failure of one API call: the transport, or a Python exception the library raises.
#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    Http(HttpError),
    /// `ResponseCodeException`: the answer's `code` (or `OK`) said no.
    ResponseCode {
        code: Value,
        message: String,
    },
    /// Any other exception, by class name and text.
    Python(PyError),
}

impl ApiError {
    fn python(class: &str, message: impl Into<String>) -> Self {
        Self::Python(PyError::new(class, message))
    }

    pub fn class_name(&self) -> &str {
        match self {
            Self::Http(error) => error.class_name(),
            Self::ResponseCode { .. } => "ResponseCodeException",
            Self::Python(error) => &error.class,
        }
    }

    /// `str(exc)`.
    pub fn message(&self) -> &str {
        match self {
            Self::Http(error) => error.message(),
            Self::ResponseCode { message, .. } => message,
            Self::Python(error) => &error.message,
        }
    }

    pub fn into_job_error(self) -> JobError {
        match self {
            Self::Http(error) => error.into_job_error(),
            other => JobError::new(other.class_name().to_owned(), other.message().to_owned()),
        }
    }
}

impl From<PyError> for ApiError {
    fn from(error: PyError) -> Self {
        Self::Python(error)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

/// `ResponseCodeException(code, msg, raw)` and its `__str__`.
fn response_code(code: Value, msg: &str, raw: &Dict) -> ApiError {
    let message = format!(
        "接口返回错误代码：{}，信息：{msg}。\n{}",
        str_value(&code),
        repr_value(&Value::Dict(raw.clone()))
    );
    ApiError::ResponseCode { code, message }
}

// ---------- credential and video ----------

/// `bilibili_api.Credential` from the four stored cookies.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential {
    pub sessdata: String,
    pub bili_jct: String,
    pub buvid3: String,
    pub dedeuserid: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Every field is part of a login; never print them.
        f.debug_struct("Credential").finish_non_exhaustive()
    }
}

impl Credential {
    /// The `Cookie` header `Api._prepare_request` builds: SESSDATA URL-quoted unless it
    /// already has a `%`, the refresh token always empty, `opus-goback=1` at the end.
    fn cookie_header(&self) -> String {
        let sessdata = if self.sessdata.contains('%') {
            self.sessdata.clone()
        } else {
            quote(&self.sessdata)
        };
        let mut parts = vec![
            format!("SESSDATA={sessdata}"),
            format!("buvid3={}", self.buvid3),
            format!("bili_jct={}", self.bili_jct),
            "ac_time_value=".to_owned(),
        ];
        if !self.dedeuserid.is_empty() {
            parts.push(format!("DedeUserID={}", self.dedeuserid));
        }
        parts.push("opus-goback=1".to_owned());
        parts.join("; ")
    }
}

/// `urllib.parse.quote(text)`: UTF-8, `/` kept.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// `bilibili_api.video.Video(bvid=...)`: a checked BV id and the av id it encodes.
///
/// Each value is its own video, as each Python object is: the crawl's info cache is keyed
/// by [`Video::key`], so the same BV id met in two lists is looked up twice, as in Python.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Video {
    bvid: String,
    aid: i128,
    key: u64,
}

impl Video {
    /// `Video(bvid=bvid)`: `ArgsException` for a malformed BV id, `ValueError` for a
    /// character outside the base-58 table.
    pub fn new(bvid: &str) -> Result<Self, ApiError> {
        static NEXT_KEY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        // `re.search("^BV[a-zA-Z0-9]{10}$", bvid)`: `$` also matches before a final newline.
        let body = bvid.strip_suffix('\n').unwrap_or(bvid);
        let valid = body.len() == 12 && body.starts_with("BV") && body.bytes().skip(2).all(|byte| byte.is_ascii_alphanumeric());
        if !valid {
            return Err(ApiError::python("ArgsException", BVID_ERROR));
        }
        Ok(Self {
            bvid: bvid.to_owned(),
            aid: bvid_to_aid(bvid)?,
            key: NEXT_KEY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        })
    }

    /// `get_bvid()`.
    pub fn bvid(&self) -> &str {
        &self.bvid
    }

    /// The identity of this video object.
    pub fn key(&self) -> u64 {
        self.key
    }
}

/// `bvid2aid`: swap positions 3/9 and 4/7, read the characters after `BV1` in base 58,
/// mask and XOR.
fn bvid_to_aid(bvid: &str) -> Result<i128, ApiError> {
    let mut chars: Vec<char> = bvid.chars().collect();
    chars.swap(3, 9);
    chars.swap(4, 7);
    let mut value: i128 = 0;
    for ch in &chars[3..] {
        let index = u8::try_from(*ch)
            .ok()
            .and_then(|byte| BVID_TABLE.iter().position(|entry| *entry == byte))
            .ok_or_else(|| ApiError::python("ValueError", format!("{} is not in list", bytes_repr(*ch))))?;
        value = value * 58 + i128::try_from(index).unwrap_or_default();
    }
    Ok((value & BVID_MAX_AID) ^ BVID_XOR)
}

/// `repr(ch.encode())` for the one character `bvid2aid` could not find.
fn bytes_repr(ch: char) -> String {
    let mut out = String::from("b'");
    let mut buffer = [0; 4];
    for byte in ch.encode_utf8(&mut buffer).bytes() {
        match byte {
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b'\t' => out.push_str("\\t"),
            b'\\' => out.push_str("\\\\"),
            b'\'' => out.push_str("\\'"),
            0x20..=0x7e => out.push(char::from(byte)),
            _ => {
                let _ = write!(out, "\\x{byte:02x}");
            }
        }
    }
    out.push('\'');
    out
}

// ---------- the client ----------

/// The library's httpx client and one account's credential.
#[derive(Clone)]
pub struct Api {
    client: reqwest::Client,
    base: String,
    credential: Credential,
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Api").field("base", &self.base).finish_non_exhaustive()
    }
}

impl Api {
    /// A client for `https://api.bilibili.com`, or for another origin (tests).
    pub fn new(credential: Credential, base: &str) -> Result<Self, HttpError> {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::REFERER, header::HeaderValue::from_static(REFERER));
        let client = fav_net::client_builder()
            .user_agent(USER_AGENT)
            .default_headers(headers)
            .connect_timeout(TIMEOUT)
            .read_timeout(TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| HttpError::transport(&error))?;
        Ok(Self {
            client,
            base: base.trim_end_matches('/').to_owned(),
            credential,
        })
    }

    /// The account this client signs its requests with.
    pub fn credential(&self) -> &Credential {
        &self.credential
    }

    /// `Api.request()`: up to three attempts while the answer is code -403.
    async fn request(&self, method: Method, path: &str, params: &[(&str, String)], data: &[(&str, String)]) -> Result<Value, ApiError> {
        for _ in 0..WBI_RETRY_TIMES {
            match self.request_once(method.clone(), path, params, data).await {
                Err(ApiError::ResponseCode { code, .. }) if py_eq_int(&code, WBI_RETRY_CODE) => {}
                other => return other,
            }
        }
        Err(ApiError::python("WbiRetryTimesExceedException", "Wbi 重试达到最大次数"))
    }

    async fn request_once(
        &self,
        method: Method,
        path: &str,
        params: &[(&str, String)],
        data: &[(&str, String)],
    ) -> Result<Value, ApiError> {
        let mut url = format!("{}{path}", self.base);
        if !params.is_empty() {
            url.push('?');
            url.push_str(&urlencode(params));
        }
        let mut request = self
            .client
            .request(method.clone(), url)
            .header(header::COOKIE, self.credential.cookie_header());
        if method != Method::GET {
            // Not a GET: the csrf token goes in the form, twice.
            let mut form: Vec<(&str, String)> = data.to_vec();
            form.push(("csrf", self.credential.bili_jct.clone()));
            form.push(("csrf_token", self.credential.bili_jct.clone()));
            request = request
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(urlencode(&form));
        }
        let response = request.send().await.map_err(|error| ApiError::Http(HttpError::transport(&error)))?;
        let content_length = response
            .headers()
            .get(header::CONTENT_LENGTH)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned());
        let body = response
            .bytes()
            .await
            .map_err(|error| ApiError::Http(HttpError::transport(&error)))?;
        process_response(content_length.as_deref(), &body)
    }

    /// `user.get_toview_list()`.
    pub async fn toview_list(&self) -> Result<Value, ApiError> {
        self.request(Method::GET, "/x/v2/history/toview", &[], &[]).await
    }

    /// `user.clear_toview_list()`.
    pub async fn clear_toview_list(&self) -> Result<Value, ApiError> {
        self.request(Method::POST, "/x/v2/history/toview/clear", &[], &[]).await
    }

    /// `FavoriteList(media_id=...).get_content(page=...)`.
    pub async fn favorite_page(&self, media_id: i64, page: i64) -> Result<Value, ApiError> {
        let params = [
            ("media_id", media_id.to_string()),
            ("pn", page.to_string()),
            ("ps", "20".to_owned()),
            ("order", "mtime".to_owned()),
            ("tid", "0".to_owned()),
            ("type", "0".to_owned()),
        ];
        self.request(Method::GET, "/x/v3/fav/resource/list", &params, &[]).await
    }

    /// `Video.get_info()`.
    pub async fn video_info(&self, video: &Video) -> Result<Value, ApiError> {
        let params = [("bvid", video.bvid.clone()), ("aid", video.aid.to_string())];
        self.request(Method::GET, "/x/web-interface/view", &params, &[]).await
    }

    /// `Video.get_detail()`.
    pub async fn video_detail(&self, video: &Video) -> Result<Value, ApiError> {
        let params = [
            ("bvid", video.bvid.clone()),
            ("aid", video.aid.to_string()),
            ("need_operation_card", "0".to_owned()),
            ("need_elec", "0".to_owned()),
        ];
        self.request(Method::GET, "/x/web-interface/wbi/view/detail", &params, &[]).await
    }
}

/// `Api._process_response`: `None` (null) for an empty answer, else the checked JSON's
/// `data` (or `result`).
fn process_response(content_length: Option<&str>, body: &[u8]) -> Result<Value, ApiError> {
    if let Some(length) = content_length {
        match fav_core::pyint::parse_int(length) {
            Ok(0) => return Ok(Value::Null),
            Ok(_) => {}
            Err(_) => {
                return Err(ApiError::python(
                    "ValueError",
                    format!("invalid literal for int() with base 10: {}", fav_runtime::pystr::repr_str(length)),
                ));
            }
        }
    }
    let text = pydecode::decode_utf8(body)?;
    let data = pydecode::json_loads(text)?;
    let Value::Dict(dict) = &data else {
        return Err(ApiError::python(
            "AttributeError",
            format!("'{}' object has no attribute 'get'", crate::pyerr::type_name(&data)),
        ));
    };
    let present = |key: &str| dict.get(key).filter(|value| !matches!(value, Value::Null));
    match present("OK") {
        None => {
            let Some(code) = present("code") else {
                return Err(response_code(Value::Int(-1), "API 返回数据未含 code 字段", dict));
            };
            if !py_eq_int(code, 0) {
                let message = present("msg")
                    .or_else(|| present("message"))
                    .map_or_else(|| "接口未返回错误信息".to_owned(), str_value);
                return Err(response_code(code.clone(), &message, dict));
            }
            Ok(present("data").or_else(|| present("result")).cloned().unwrap_or(Value::Null))
        }
        Some(ok) if !py_eq_int(ok, 1) => Err(response_code(Value::Int(-1), "API 返回数据 OK 不为 1", dict)),
        Some(_) => Ok(data),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use fav_core::golden;
    use fav_core::pyjson::{self, Value};

    use super::{Api, Credential, Video, process_response};
    use crate::testkit::{Answer, FakeServer};

    fn credential(sessdata: &str) -> Credential {
        Credential {
            sessdata: sessdata.to_owned(),
            bili_jct: "jct".to_owned(),
            buvid3: "b3".to_owned(),
            dedeuserid: "42".to_owned(),
        }
    }

    #[test]
    fn bv_ids_become_av_ids_or_python_errors() {
        let document = golden::load("bilibili/bvid.json");
        for case in golden::cases(&document) {
            let bvid = golden::string(golden::get(case, "bvid"));
            match Video::new(&bvid) {
                Ok(video) => assert_eq!(video.aid, golden::int(golden::get(case, "aid")), "{bvid}"),
                Err(error) => {
                    assert_eq!(error.class_name(), golden::string(golden::get(case, "error_class")), "{bvid:?}");
                    assert_eq!(error.message(), golden::string(golden::get(case, "error")), "{bvid:?}");
                }
            }
        }
    }

    #[test]
    fn two_videos_of_one_bv_id_are_two_objects() {
        let first = Video::new("BV1xx411c7mD").expect("valid");
        let second = Video::new("BV1xx411c7mD").expect("valid");
        assert_ne!(first.key(), second.key());
        assert_eq!(first.bvid(), second.bvid());
    }

    // py: (golden) bilibili/api_answers.json
    #[test]
    fn answers_are_checked_like_the_library() {
        let document = golden::load("bilibili/api_answers.json");
        for case in golden::cases(&document) {
            if case.get("script").is_some() {
                continue;
            }
            // The golden stores the body as latin-1 text: one char per byte.
            let body: Vec<u8> = golden::string(golden::get(case, "body"))
                .chars()
                .map(|ch| u8::try_from(u32::from(ch)).expect("latin-1"))
                .collect();
            let length = matches!(case.get("content_length_zero"), Some(Value::Bool(true))).then_some("0");
            let outcome = process_response(length, &body);
            if let Some(expected) = case.get("result") {
                assert_eq!(&outcome.expect("a result"), expected, "{body:?}");
            } else {
                let error = outcome.expect_err("an error");
                assert_eq!(error.class_name(), golden::string(golden::get(case, "error_class")), "{body:?}");
                assert_eq!(error.message(), golden::string(golden::get(case, "error")), "{body:?}");
            }
        }
    }

    // py: (golden) bilibili/api_answers.json, the -403 scripts
    #[tokio::test]
    async fn code_403_is_retried_three_times() {
        let document = golden::load("bilibili/api_answers.json");
        for case in golden::cases(&document) {
            let Some(script) = case.get("script") else { continue };
            let script: Vec<String> = golden::list(script).iter().map(golden::string).collect();
            let attempts = Arc::new(Mutex::new(0_usize));
            let counter = Arc::clone(&attempts);
            let server = FakeServer::start(move |_| {
                let mut seen = counter.lock().expect("counter");
                *seen += 1;
                Answer::json(200, &script[(*seen).min(script.len()) - 1])
            })
            .await;
            let api = Api::new(credential("s"), &server.base_url).expect("client");
            let outcome = api.video_info(&Video::new("BV1xx411c7mD").expect("valid")).await;
            assert_eq!(
                i128::try_from(*attempts.lock().expect("counter")).expect("small"),
                golden::int(golden::get(case, "attempts"))
            );
            if let Some(expected) = case.get("result") {
                assert_eq!(&outcome.expect("a result"), expected);
            } else {
                let error = outcome.expect_err("an error");
                assert_eq!(error.class_name(), golden::string(golden::get(case, "error_class")));
                assert_eq!(error.message(), golden::string(golden::get(case, "error")));
            }
        }
    }

    // py: (golden) bilibili/api_requests.json
    #[tokio::test]
    async fn requests_match_the_library() {
        let document = golden::load("bilibili/api_requests.json");
        for case in golden::cases(&document) {
            let call = golden::string(golden::get(case, "call"));
            let sessdata = case.get("sessdata").map_or_else(|| "sess,data".to_owned(), golden::string);
            let ok = r#"{"code": 0, "message": "0", "ttl": 1, "data": {"count": 1, "list": [{"bvid": "BV1xx411c7mD"}]}}"#;
            let server = FakeServer::start(move |_| Answer::json(200, ok)).await;
            let api = Api::new(credential(&sessdata), &server.base_url).expect("client");
            let result = match call.as_str() {
                "toview_list" => api.toview_list().await,
                "toview_clear" => api.clear_toview_list().await,
                "fav_content_page_1" => api.favorite_page(123, 1).await,
                "fav_content_page_3" => api.favorite_page(9_876_543_210, 3).await,
                "video_info" => api.video_info(&Video::new("BV1xx411c7mD").expect("valid")).await,
                "video_detail" => api.video_detail(&Video::new("BV1GJ411x7h7").expect("valid")).await,
                other => panic!("unknown call {other}"),
            };
            if let Some(expected) = case.get("result") {
                assert_eq!(&result.expect("a result"), expected, "{call}");
            }
            let received = server.received();
            let expected = golden::list(golden::get(case, "requests"));
            assert_eq!(received.len(), expected.len(), "{call}");
            for (got, want) in received.iter().zip(expected) {
                let url = golden::string(golden::get(want, "url"));
                let path_and_query = url.strip_prefix("https://api.bilibili.com").expect("api url");
                assert_eq!(got.method, golden::string(golden::get(want, "method")), "{call}");
                assert_eq!(got.path_and_query(), path_and_query, "{call}");
                for header in ["user-agent", "referer", "origin", "cookie", "content-type"] {
                    let key = header.replace('-', "_");
                    let want = golden::get(want, &key).as_str();
                    assert_eq!(got.header(header), want, "{call} {header}");
                }
                assert_eq!(got.body_text(), golden::string(golden::get(want, "body")), "{call}");
            }
        }
    }

    #[test]
    fn an_answer_value_is_python_json() {
        assert_eq!(
            process_response(None, b"{\"code\": 0, \"data\": [1]}"),
            Ok(pyjson::loads("[1]").expect("json"))
        );
    }
}
