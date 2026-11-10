//! The MCP endpoint, `/mcp`: fav's runtime state and a few repairs, for MCP clients.
//!
//! Python origin: `src/api/mcp_server.py` (`build_mcp_server`, mounted by `create_app`)
//! on FastMCP 1.30 with `stateless_http=True` and `json_response=True`, so every POST is
//! one JSON-RPC message answered by one JSON body, and no session is kept between
//! requests. The transport is reproduced here rather than taken from `rmcp`
//! (`DIVERGENCES.md`): the checks of `StreamableHTTPServerTransport` in their order, its
//! error envelopes, pydantic's validation of the message (`JSONRPCMessage`, then
//! `ClientRequest`) and the low-level server's answers to each method, all checked
//! against FastMCP's own responses (`rust/goldens/mcp/protocol.json`).
//!
//! The tools are [`tools`]; the queries and repairs behind them [`ops`].

use std::path::PathBuf;
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use fav_core::pyjson::{self, Dict, LoadError, Value};
use fav_core::validate::{self, ErrorItem, Errors, Fields, LocItem};

use crate::router::Request;
use crate::{ApiState, json};

mod isoformat;
mod json_text;
mod ops;
mod pydantic;
mod tools;

pub use ops::{WorkerProcess, local_timezone};

#[cfg(test)]
mod tests;

/// Where the endpoint is mounted; it answers there itself, not at `/mcp/`.
pub(crate) const MCP_PATH: &str = "/mcp";

/// `SUPPORTED_PROTOCOL_VERSIONS`, oldest first.
const SUPPORTED_VERSIONS: [&str; 4] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
/// `LATEST_PROTOCOL_VERSION`: what `initialize` answers to a version it does not know.
const LATEST_VERSION: &str = "2025-11-25";
/// `DEFAULT_NEGOTIATED_VERSION`: assumed when a request has no `mcp-protocol-version`.
const DEFAULT_NEGOTIATED_VERSION: &str = "2025-03-26";
/// The installed `mcp` package's version, which FastMCP reports as the server version.
const SERVER_VERSION: &str = "1.30.0";
/// `DEFAULT_MAX_REQUEST_BODY_SIZE`.
pub(crate) const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;
/// sse-starlette's `DEFAULT_PING_INTERVAL`.
const PING_INTERVAL: Duration = Duration::from_secs(15);

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;

/// `_INSTRUCTIONS`, sent with the `initialize` result.
const INSTRUCTIONS: &str = "fav crawls content sources on a schedule (one job per source), stores media under collection/ and its\nstate in PostgreSQL, and notifies through Telegram. The API and the worker run in one pod; the worker\nexecutes runs, and every run, scheduled or manual, is recorded in control_requests (list_runs).\n\nTypical checks: get_runtime for the deployed git commit and restart times; list_jobs for schedules and\neach job's last run; list_runs/get_run for errors; read_logs for detail (log files survive pod restarts);\nwechat_status for the WeChat source. run_job queues a manual run that the worker picks up within about a\nsecond; poll get_run with the returned id. Secret values are masked in settings, logs and errors.";

/// What the MCP tools need beyond the API's own state.
pub struct McpState {
    /// `API_TOKEN` and the database password, which the redactor masks wherever they
    /// appear even though no settings section holds them.
    pub bootstrap_secrets: Vec<String>,
    /// The log directory `read_logs` reads (`./log`, resolved).
    pub log_dir: PathBuf,
    /// `TZ`, as the scheduler resolves it: next fire times, and naive database times.
    pub timezone: Tz,
    /// The zone's name as `get_runtime` reports it.
    pub timezone_name: String,
    /// `API_STARTED_AT`.
    pub started_at: DateTime<Utc>,
    pub worker: WorkerProcess,
}

impl std::fmt::Debug for McpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpState")
            .field("bootstrap_secrets", &"<redacted>")
            .field("log_dir", &self.log_dir)
            .field("timezone_name", &self.timezone_name)
            .field("started_at", &self.started_at)
            .field("worker", &self.worker)
            .finish_non_exhaustive()
    }
}

impl McpState {
    /// The state for an API started now, from its token, the database DSN, the log
    /// directory and `TZ`.
    pub fn new(api_token: &str, postgres_dsn: &str, log_dir: PathBuf, tz: Option<&str>, worker: WorkerProcess) -> Self {
        let mut bootstrap_secrets = vec![api_token.to_owned()];
        bootstrap_secrets.extend(ops::dsn_password(postgres_dsn));
        let (timezone, timezone_name) = local_timezone(tz);
        Self {
            bootstrap_secrets,
            log_dir,
            timezone,
            timezone_name,
            started_at: Utc::now(),
            worker,
        }
    }
}

// ---------- responses ----------

/// `_create_error_response`: the `server-error` envelope.
fn error_response(status: StatusCode, code: i64, message: &str) -> Response {
    let body = json::dict([
        ("jsonrpc", Value::from("2.0")),
        ("id", Value::from("server-error")),
        (
            "error",
            json::dict([("code", Value::from(code)), ("message", Value::from(message))]),
        ),
    ]);
    json_body(status, Some(&body))
}

/// `_create_json_response`: a message, or an empty body.
fn json_body(status: StatusCode, message: Option<&Value>) -> Response {
    let mut response = Response::new(message.map_or_else(Body::empty, |message| Body::from(json_text::compact(message))));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

/// A Starlette `Response` with text and no media type, so no `content-type` either.
fn plain(status: StatusCode, text: &'static str) -> Response {
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = status;
    response
}

/// What uvicorn sends when the application returned without starting a response.
fn no_response() -> Response {
    let mut response = Response::new(Body::from("Internal Server Error"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    response
}

/// The answer to a body over [`MAX_BODY_BYTES`] (`RequestBodyLimitMiddleware`).
pub(crate) fn body_too_large() -> Response {
    plain(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large")
}

// ---------- the transport ----------

/// `_check_accept_headers`: whether the client accepts JSON, and event streams.
fn accepts(request: &Request) -> (bool, bool) {
    let accept = request.header("accept").unwrap_or_default();
    let types: Vec<&str> = accept.split(',').map(fav_core::pytext::py_strip).collect();
    (
        types.iter().any(|kind| kind.starts_with("application/json")),
        types.iter().any(|kind| kind.starts_with("text/event-stream")),
    )
}

/// `_validate_protocol_version`.
fn protocol_version_error(request: &Request) -> Option<Response> {
    let version = request
        .header("mcp-protocol-version")
        .unwrap_or_else(|| DEFAULT_NEGOTIATED_VERSION.to_owned());
    if SUPPORTED_VERSIONS.contains(&version.as_str()) {
        return None;
    }
    Some(error_response(
        StatusCode::BAD_REQUEST,
        INVALID_REQUEST,
        &format!(
            "Bad Request: Unsupported protocol version: {version}. Supported versions: {}",
            SUPPORTED_VERSIONS.join(", ")
        ),
    ))
}

/// Answer one request to [`MCP_PATH`] (`StreamableHTTPServerTransport.handle_request`).
pub(crate) async fn handle(state: &ApiState, request: &Request) -> Response {
    if request.body.len() > MAX_BODY_BYTES {
        return body_too_large();
    }
    // TransportSecurityMiddleware: DNS-rebinding protection is off, the content type is not.
    if request.method == "POST" {
        let content_type = request.header("content-type").unwrap_or_default();
        if !content_type.to_lowercase().starts_with("application/json") {
            let logger = fav_core::logging::get_external("mcp.server.transport_security");
            if content_type.is_empty() {
                logger.warning("Missing Content-Type header in POST request");
            } else {
                logger.warning(format_args!("Invalid Content-Type header: {content_type}"));
            }
            return plain(StatusCode::BAD_REQUEST, "Invalid Content-Type header");
        }
    }
    match request.method.as_str() {
        "POST" => post(state, request).await,
        "GET" => get(request),
        "DELETE" => error_response(
            StatusCode::METHOD_NOT_ALLOWED,
            INVALID_REQUEST,
            "Method Not Allowed: Session termination not supported",
        ),
        _ => {
            let mut response = error_response(StatusCode::METHOD_NOT_ALLOWED, INVALID_REQUEST, "Method Not Allowed");
            response
                .headers_mut()
                .insert(header::ALLOW, HeaderValue::from_static("GET, POST, DELETE"));
            if request.method == "HEAD" {
                *response.body_mut() = Body::empty();
            }
            response
        }
    }
}

/// `_handle_get_request`: the stream for server-initiated messages. A stateless server
/// never sends any, so it only carries sse-starlette's keep-alive comments.
fn get(request: &Request) -> Response {
    if !accepts(request).1 {
        return error_response(
            StatusCode::NOT_ACCEPTABLE,
            INVALID_REQUEST,
            "Not Acceptable: Client must accept text/event-stream",
        );
    }
    if let Some(response) = protocol_version_error(request) {
        return response;
    }
    if request.header("last-event-id").is_some_and(|id| !id.is_empty()) {
        // `if last_event_id := ...`: an empty header opens the stream. With one,
        // `_replay_events` returns without answering when there is no event store.
        return no_response();
    }
    let pings = futures_util::stream::unfold((), |()| async {
        tokio::time::sleep(PING_INTERVAL).await;
        let now = Utc::now().fixed_offset();
        let comment = format!(": ping - {}\r\n\r\n", pyjson::py_str_datetime(&now));
        Some((Ok::<_, std::convert::Infallible>(Bytes::from(comment)), ()))
    });
    let mut response = Response::new(Body::from_stream(pings));
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache, no-transform"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

/// `_handle_post_request`.
async fn post(state: &ApiState, request: &Request) -> Response {
    if !accepts(request).0 {
        return error_response(
            StatusCode::NOT_ACCEPTABLE,
            INVALID_REQUEST,
            "Not Acceptable: Client must accept application/json",
        );
    }
    let content_type = request.header("content-type").unwrap_or_default();
    let first = content_type.split(';').next().unwrap_or_default();
    if !first.split(',').any(|part| fav_core::pytext::py_strip(part) == "application/json") {
        return error_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            INVALID_REQUEST,
            "Unsupported Media Type: Content-Type must be application/json",
        );
    }
    let Some(text) = decode_body(&request.body) else {
        // A UnicodeDecodeError, which only the outer `except Exception` catches.
        return error_response(StatusCode::INTERNAL_SERVER_ERROR, INTERNAL_ERROR, "Error handling POST request");
    };
    let raw = match pyjson::loads(&text) {
        Ok(raw) => raw,
        Err(error) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                PARSE_ERROR,
                &format!("Parse error: {}", decode_error_text(&text, &error)),
            );
        }
    };
    let message = match validate_message(&raw) {
        Ok(message) => message,
        Err(errors) => {
            return error_response(
                StatusCode::BAD_REQUEST,
                INVALID_PARAMS,
                &format!("Validation error: {}", pydantic::error_text("JSONRPCMessage", &errors)),
            );
        }
    };
    let initialize = matches!(&message, Message::Request { method, .. } if method == "initialize");
    if !initialize && let Some(response) = protocol_version_error(request) {
        return response;
    }
    match message {
        Message::Other => json_body(StatusCode::ACCEPTED, None),
        Message::Request { id, method, params } => {
            let params = params.map(|params| json_mode_dict(&params));
            let answer = answer(state, &method, params.as_ref()).await;
            let mut reply = Dict::new();
            reply.insert("jsonrpc", Value::from("2.0"));
            reply.insert("id", id);
            match answer {
                Ok(result) => reply.insert("result", result),
                Err((code, message, data)) => {
                    let mut error = Dict::new();
                    error.insert("code", Value::from(code));
                    error.insert("message", Value::Str(message));
                    if let Some(data) = data {
                        error.insert("data", data);
                    }
                    reply.insert("error", Value::Dict(error));
                }
            }
            json_body(StatusCode::OK, Some(&Value::Dict(reply)))
        }
    }
}

// ---------- the body ----------

/// `json.detect_encoding` and the decode `json.loads` does for bytes.
fn decode_body(body: &[u8]) -> Option<String> {
    const UTF8_BOM: &[u8] = b"\xef\xbb\xbf";
    if body.starts_with(b"\x00\x00\xfe\xff") {
        return decode_utf32(&body[4..], true);
    }
    if body.starts_with(b"\xff\xfe\x00\x00") {
        return decode_utf32(&body[4..], false);
    }
    if body.starts_with(b"\xfe\xff") {
        return decode_utf16(&body[2..], true);
    }
    if body.starts_with(b"\xff\xfe") {
        return decode_utf16(&body[2..], false);
    }
    if let Some(rest) = body.strip_prefix(UTF8_BOM) {
        return String::from_utf8(rest.to_vec()).ok();
    }
    match body {
        [0, second, ..] if body.len() >= 4 => {
            if *second == 0 {
                decode_utf32(body, true)
            } else {
                decode_utf16(body, true)
            }
        }
        [_, 0, third, fourth, ..] => {
            if *third != 0 || *fourth != 0 {
                decode_utf16(body, false)
            } else {
                decode_utf32(body, false)
            }
        }
        [0, _] => decode_utf16(body, true),
        [_, 0] => decode_utf16(body, false),
        _ => String::from_utf8(body.to_vec()).ok(),
    }
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes.chunks_exact(2).map(|pair| {
        let pair = [pair[0], pair[1]];
        if big_endian {
            u16::from_be_bytes(pair)
        } else {
            u16::from_le_bytes(pair)
        }
    });
    char::decode_utf16(units).collect::<Result<String, _>>().ok()
}

fn decode_utf32(bytes: &[u8], big_endian: bool) -> Option<String> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    bytes
        .chunks_exact(4)
        .map(|quad| {
            let quad = [quad[0], quad[1], quad[2], quad[3]];
            char::from_u32(if big_endian {
                u32::from_be_bytes(quad)
            } else {
                u32::from_le_bytes(quad)
            })
        })
        .collect()
}

/// `str(JSONDecodeError)`: the message, then the line, column and character offset.
fn decode_error_text(text: &str, error: &LoadError) -> String {
    let (message, position) = match *error {
        LoadError::ExpectingValue(at) => ("Expecting value", at),
        LoadError::ExtraData(at) => ("Extra data", at),
        LoadError::UnterminatedString(at) => ("Unterminated string starting at", at),
        LoadError::ControlCharacter(at) => ("Invalid control character at", at),
        LoadError::InvalidEscape(at) => ("Invalid \\escape", at),
        LoadError::ExpectingPropertyName(at) => ("Expecting property name enclosed in double quotes", at),
        LoadError::ExpectingColon(at) => ("Expecting ':' delimiter", at),
        LoadError::ExpectingComma(at) => ("Expecting ',' delimiter", at),
        LoadError::LoneSurrogate(at) => ("Lone surrogate escape", at),
        LoadError::IntegerTooLarge(at) => ("Integer out of range", at),
    };
    let before: Vec<char> = text.chars().take(position).collect();
    let line = before.iter().filter(|&&ch| ch == '\n').count() + 1;
    let column = match before.iter().rposition(|&ch| ch == '\n') {
        Some(newline) => position - newline,
        None => position + 1,
    };
    format!("{message}: line {line} column {column} (char {position})")
}

// ---------- JSONRPCMessage ----------

/// A message that passed `JSONRPCMessage.model_validate`.
enum Message {
    /// A `JSONRPCRequest`, answered.
    Request { id: Value, method: String, params: Option<Dict> },
    /// A notification, response or error from the client: accepted and dropped.
    Other,
}

fn error(kind: &'static str, msg: &str, loc: &[LocItem], input: &Value) -> Errors {
    vec![ErrorItem::new(kind, msg, loc, input)]
}

/// `params: dict[str, Any] | None`, and `result: dict[str, Any]`.
fn dict_field(value: &Value, loc: &[LocItem]) -> Result<Option<Dict>, Errors> {
    match value {
        Value::Dict(dict) => Ok(Some(dict.clone())),
        other => Err(error("dict_type", "Input should be a valid dictionary", loc, other)),
    }
}

fn optional_dict(value: &Value, loc: &[LocItem]) -> Result<Option<Dict>, Errors> {
    match value {
        Value::Null => Ok(None),
        other => dict_field(other, loc),
    }
}

/// `jsonrpc: Literal['2.0']`.
fn jsonrpc(value: &Value, loc: &[LocItem]) -> Result<(), Errors> {
    if value.as_str() == Some("2.0") {
        return Ok(());
    }
    let mut item = ErrorItem::new("literal_error", "Input should be '2.0'", loc, value);
    let mut ctx = Dict::new();
    ctx.insert("expected", Value::from("'2.0'"));
    item.ctx = Some(ctx);
    Err(vec![item])
}

/// `RequestId = Annotated[int, Field(strict=True)] | str`.
fn request_id(value: &Value, loc: &[LocItem]) -> Result<Value, Errors> {
    match value {
        Value::Int(_) | Value::Str(_) => Ok(value.clone()),
        other => {
            let mut errors = error("int_type", "Input should be a valid integer", &validate::key(loc, "int"), other);
            errors.extend(error(
                "string_type",
                "Input should be a valid string",
                &validate::key(loc, "str"),
                other,
            ));
            Err(errors)
        }
    }
}

/// `JSONRPCError.id: str | int`, in lax mode.
fn error_id(value: &Value, loc: &[LocItem]) -> Result<(), Errors> {
    if matches!(value, Value::Str(_)) {
        return Ok(());
    }
    let Err(int_errors) = validate::int_unbounded(value, &validate::key(loc, "int")) else {
        return Ok(());
    };
    let mut errors = error("string_type", "Input should be a valid string", &validate::key(loc, "str"), value);
    errors.extend(int_errors);
    Err(errors)
}

fn error_data(value: &Value, loc: &[LocItem]) -> Result<(), Errors> {
    let mut fields = Fields::begin(value, loc, "ErrorData")?;
    let code = fields.required("code", validate::int_unbounded);
    let message = fields.required("message", validate::string);
    fields.finish(|| code.zip(message), |_| Ok(())).map(|_| ())
}

/// The session re-validates `model_dump(mode="json")` of the request, which writes a
/// non-finite float in the parameters as `None`.
fn json_mode_dict(dict: &Dict) -> Dict {
    let mut out = Dict::new();
    for (key, value) in dict.iter() {
        out.insert(key.clone(), json_mode(value));
    }
    out
}

fn json_mode(value: &Value) -> Value {
    match value {
        Value::Float(number) if !number.is_finite() => Value::Null,
        Value::List(items) => Value::List(items.iter().map(json_mode).collect()),
        Value::Dict(dict) => Value::Dict(json_mode_dict(dict)),
        other => other.clone(),
    }
}

fn request(value: &Value) -> Result<Message, Errors> {
    let loc = [LocItem::Key("JSONRPCRequest".to_owned())];
    let mut fields = Fields::begin(value, &loc, "JSONRPCRequest")?;
    let method = fields.required("method", validate::string);
    let params = fields.optional("params", || None, optional_dict);
    let version = fields.required("jsonrpc", jsonrpc);
    let id = fields.required("id", request_id);
    fields.finish(
        || {
            version?;
            Some(Message::Request {
                id: id?,
                method: method?,
                params: params?,
            })
        },
        |_| Ok(()),
    )
}

fn notification(value: &Value) -> Result<(), Errors> {
    let loc = [LocItem::Key("JSONRPCNotification".to_owned())];
    let mut fields = Fields::begin(value, &loc, "JSONRPCNotification")?;
    let method = fields.required("method", validate::string);
    let params = fields.optional("params", || None, optional_dict);
    let version = fields.required("jsonrpc", jsonrpc);
    fields.finish(|| method.and(params).and(version), |()| Ok(()))
}

fn response(value: &Value) -> Result<(), Errors> {
    let loc = [LocItem::Key("JSONRPCResponse".to_owned())];
    let mut fields = Fields::begin(value, &loc, "JSONRPCResponse")?;
    let version = fields.required("jsonrpc", jsonrpc);
    let id = fields.required("id", request_id);
    let result = fields.required("result", dict_field);
    fields.finish(|| version.and(id).and(result).map(|_| ()), |()| Ok(()))
}

fn error_message(value: &Value) -> Result<(), Errors> {
    let loc = [LocItem::Key("JSONRPCError".to_owned())];
    let mut fields = Fields::begin(value, &loc, "JSONRPCError")?;
    let version = fields.required("jsonrpc", jsonrpc);
    let id = fields.required("id", error_id);
    let data = fields.required("error", error_data);
    fields.finish(|| version.and(id).and(data), |()| Ok(()))
}

/// `JSONRPCMessage.model_validate`: a union in smart mode. A valid request wins; any other
/// valid member makes the message one the server only acknowledges; with none valid, every
/// member's errors are reported, in member order.
fn validate_message(value: &Value) -> Result<Message, Errors> {
    let mut errors = match request(value) {
        Ok(message) => return Ok(message),
        Err(errors) => errors,
    };
    for member in [notification, response, error_message] {
        match member(value) {
            Ok(()) => return Ok(Message::Other),
            Err(member_errors) => errors.extend(member_errors),
        }
    }
    Err(errors)
}

// ---------- ClientRequest ----------

/// Whether `ClientRequest.model_validate` accepts the request. Its errors are never
/// shown: the server answers `Invalid request parameters` with empty data.
fn valid_client_request(method: &str, params: Option<&Dict>) -> bool {
    let base = params.is_none_or(base_params_valid);
    let required = |check: fn(&Dict) -> bool| params.is_some_and(|params| base_params_valid(params) && check(params));
    match method {
        "ping" | "tools/list" | "resources/list" | "resources/templates/list" | "prompts/list" | "tasks/list" => {
            base && params.is_none_or(|params| field_ok(params, "cursor", optional_str))
        }
        "initialize" => required(|params| {
            params.get("protocolVersion").is_some_and(protocol_version_ok)
                && params.get("capabilities").is_some_and(capabilities_ok)
                && params.get("clientInfo").is_some_and(implementation_ok)
        }),
        "tools/call" => required(|params| {
            params.get("name").is_some_and(is_str) && field_ok(params, "arguments", |value| matches!(value, Value::Null | Value::Dict(_)))
        }),
        "resources/read" | "resources/subscribe" | "resources/unsubscribe" => {
            required(|params| params.get("uri").is_some_and(|uri| any_url(uri).is_some()))
        }
        "prompts/get" => required(|params| {
            params.get("name").is_some_and(is_str)
                && field_ok(params, "arguments", |value| match value {
                    Value::Null => true,
                    Value::Dict(arguments) => arguments.iter().all(|(_, value)| is_str(value)),
                    _ => false,
                })
        }),
        "completion/complete" => required(|params| {
            params.get("ref").is_some_and(reference_ok)
                && params
                    .get("argument")
                    .and_then(Value::as_dict)
                    .is_some_and(|argument| argument.get("name").is_some_and(is_str) && argument.get("value").is_some_and(is_str))
                && field_ok(params, "context", |value| matches!(value, Value::Null | Value::Dict(_)))
        }),
        "logging/setLevel" => required(|params| {
            params.get("level").and_then(Value::as_str).is_some_and(|level| {
                matches!(
                    level,
                    "debug" | "info" | "notice" | "warning" | "error" | "critical" | "alert" | "emergency"
                )
            })
        }),
        "tasks/get" | "tasks/result" | "tasks/cancel" => required(|params| params.get("taskId").is_some_and(is_str)),
        _ => false,
    }
}

fn is_str(value: &Value) -> bool {
    matches!(value, Value::Str(_))
}

fn optional_str(value: &Value) -> bool {
    matches!(value, Value::Null | Value::Str(_))
}

/// An absent field passes; a present one must satisfy `check`.
fn field_ok(params: &Dict, name: &str, check: impl Fn(&Value) -> bool) -> bool {
    params.get(name).is_none_or(check)
}

/// `RequestParams`: `_meta` (with its `progressToken: str | int`) and `task` (with its
/// `ttl: int | None`), each a dictionary or null.
fn base_params_valid(params: &Dict) -> bool {
    let meta_ok = field_ok(params, "_meta", |meta| match meta {
        Value::Null => true,
        Value::Dict(meta) => field_ok(meta, "progressToken", |token| {
            matches!(token, Value::Null | Value::Str(_)) || validate::int_unbounded(token, &[]).is_ok()
        }),
        _ => false,
    });
    let task_ok = field_ok(params, "task", |task| match task {
        Value::Null => true,
        Value::Dict(task) => field_ok(task, "ttl", |ttl| matches!(ttl, Value::Null | Value::Int(_))),
        _ => false,
    });
    meta_ok && task_ok
}

/// `protocolVersion: str | int`.
fn protocol_version_ok(value: &Value) -> bool {
    is_str(value) || validate::int_unbounded(value, &[]).is_ok()
}

/// `ClientCapabilities`: a dictionary whose known capabilities are dictionaries or null.
fn capabilities_ok(value: &Value) -> bool {
    let Value::Dict(capabilities) = value else {
        return false;
    };
    ["experimental", "sampling", "elicitation", "roots", "tasks"]
        .iter()
        .all(|name| field_ok(capabilities, name, |value| matches!(value, Value::Null | Value::Dict(_))))
}

/// `Implementation`: `name` and `version` strings.
fn implementation_ok(value: &Value) -> bool {
    value
        .as_dict()
        .is_some_and(|info| info.get("name").is_some_and(is_str) && info.get("version").is_some_and(is_str))
}

/// `PromptReference | ResourceTemplateReference`.
fn reference_ok(value: &Value) -> bool {
    let Some(reference) = value.as_dict() else {
        return false;
    };
    match reference.get("type").and_then(Value::as_str) {
        Some("ref/prompt") => reference.get("name").is_some_and(is_str),
        Some("ref/resource") => reference.get("uri").is_some_and(is_str),
        _ => false,
    }
}

/// `AnyUrl`: pydantic-core parses with the `url` crate, and `str()` is its serialization.
fn any_url(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    url::Url::parse(text).ok().map(String::from)
}

// ---------- the methods ----------

type Answer = Result<Value, (i64, String, Option<Value>)>;

fn method_not_found() -> Answer {
    Err((METHOD_NOT_FOUND, "Method not found".to_owned(), None))
}

/// The low-level server's answer to a request that passed `JSONRPCMessage`.
async fn answer(state: &ApiState, method: &str, params: Option<&Dict>) -> Answer {
    if !valid_client_request(method, params) {
        return Err((INVALID_PARAMS, "Invalid request parameters".to_owned(), Some(Value::from(""))));
    }
    let param = |name: &str| params.and_then(|params| params.get(name));
    match method {
        "initialize" => {
            let requested = param("protocolVersion").and_then(Value::as_str);
            let version = requested
                .filter(|version| SUPPORTED_VERSIONS.contains(version))
                .unwrap_or(LATEST_VERSION);
            Ok(json::dict([
                ("protocolVersion", Value::from(version)),
                (
                    "capabilities",
                    json::dict([
                        ("experimental", Value::Dict(Dict::new())),
                        ("prompts", json::dict([("listChanged", Value::Bool(false))])),
                        (
                            "resources",
                            json::dict([("subscribe", Value::Bool(false)), ("listChanged", Value::Bool(false))]),
                        ),
                        ("tools", json::dict([("listChanged", Value::Bool(false))])),
                    ]),
                ),
                (
                    "serverInfo",
                    json::dict([("name", Value::from("fav")), ("version", Value::from(SERVER_VERSION))]),
                ),
                ("instructions", Value::from(INSTRUCTIONS)),
            ]))
        }
        "ping" => Ok(Value::Dict(Dict::new())),
        "tools/list" => Ok(tools::list()),
        "tools/call" => {
            let name = param("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = param("arguments").and_then(Value::as_dict).cloned().unwrap_or_default();
            Ok(tools::call(state, name, &arguments).await)
        }
        "resources/list" => Ok(json::dict([("resources", Value::List(Vec::new()))])),
        "resources/templates/list" => Ok(json::dict([("resourceTemplates", Value::List(Vec::new()))])),
        "prompts/list" => Ok(json::dict([("prompts", Value::List(Vec::new()))])),
        "resources/read" => {
            let uri = param("uri").and_then(any_url).unwrap_or_default();
            Err((0, format!("Unknown resource: {uri}"), None))
        }
        "prompts/get" => {
            let name = param("name").and_then(Value::as_str).unwrap_or_default();
            Err((0, format!("Unknown prompt: {name}"), None))
        }
        _ => method_not_found(),
    }
}
