//! The 18 tools of the MCP endpoint, as FastMCP lists and calls them.
//!
//! Python origin: the `@mcp.tool()` functions of `src/api/mcp_server.py` and FastMCP 1.30's
//! tool machinery (`func_metadata`, `Tool.run`, `_convert_to_content`). A tool's input
//! schema is pydantic's for its signature, its description its docstring as written. A
//! call's arguments go through FastMCP's `pre_parse_json` (a string given for a non-string
//! parameter that holds a JSON list, object or `null` is replaced by it) and then
//! pydantic's lax validation of the arguments model; a failure, like an error the tool
//! raises, becomes an `isError` result reading `Error executing tool <name>: <message>`.
//! A result is returned twice: as text (one `TextContent` per list item, indented
//! `pydantic_core.to_json`) and as `structuredContent` (a dictionary as is, anything else
//! wrapped as `{"result": ...}`).

use fav_core::pyjson::{self, Dict, Value};
use fav_core::validate::{self, ErrorItem, Errors, Fields, LocItem};
use fav_runtime::pystr::repr_value;

use super::json_text;
use super::ops::{self, EventQuery, LogQuery, NotificationQuery, OpsError, RunQuery};
use super::pydantic;
use crate::error::ApiError;
use crate::{ApiState, handlers, json};

/// A parameter's annotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `str`
    Str,
    /// `int`
    Int,
    /// `bool`
    Bool,
    /// `dict[str, Any]`
    Dict,
    /// `list[int]`
    Ints,
    /// `list[int] | None`
    OptionalInts,
}

/// A parameter's default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    Required,
    Str(&'static str),
    Int(i64),
    Bool(bool),
    None,
}

#[derive(Debug, Clone, Copy)]
struct Param {
    name: &'static str,
    kind: Kind,
    default: Fallback,
}

const fn param(name: &'static str, kind: Kind, default: Fallback) -> Param {
    Param { name, kind, default }
}

/// The return annotation, which decides the output schema and how the result is wrapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Output {
    /// `dict[str, Any]`
    Dict,
    /// `list[dict[str, Any]]`
    List,
    /// `list[dict[str, Any]] | dict[str, Any]`
    ListOrDict,
}

/// One tool.
#[derive(Debug, Clone, Copy)]
struct Tool {
    name: &'static str,
    description: &'static str,
    params: &'static [Param],
    output: Output,
}

const fn tool(name: &'static str, description: &'static str, params: &'static [Param], output: Output) -> Tool {
    Tool {
        name,
        description,
        params,
        output,
    }
}

use Fallback as D;
use Kind as K;

/// The tools in registration order, which is the order `tools/list` gives.
const TOOLS: [Tool; 18] = [
    tool("get_runtime", GET_RUNTIME, &[], Output::Dict),
    tool("list_jobs", LIST_JOBS, &[], Output::List),
    tool(
        "list_runs",
        LIST_RUNS,
        &[
            param("job", K::Str, D::Str("")),
            param("status", K::Str, D::Str("")),
            param("kind", K::Str, D::Str("")),
            param("since", K::Str, D::Str("")),
            param("limit", K::Int, D::Int(20)),
        ],
        Output::List,
    ),
    tool("get_run", GET_RUN, &[param("run_id", K::Int, D::Required)], Output::Dict),
    tool(
        "read_logs",
        READ_LOGS,
        &[
            param("since", K::Str, D::Str("1h")),
            param("until", K::Str, D::Str("")),
            param("contains", K::Str, D::Str("")),
            param("logger_name", K::Str, D::Str("")),
            param("level", K::Str, D::Str("INFO")),
            param("limit", K::Int, D::Int(200)),
        ],
        Output::Dict,
    ),
    tool(
        "get_settings",
        GET_SETTINGS,
        &[param("section", K::Str, D::Str(""))],
        Output::ListOrDict,
    ),
    tool(
        "update_settings",
        UPDATE_SETTINGS,
        &[param("section", K::Str, D::Required), param("values", K::Dict, D::Required)],
        Output::Dict,
    ),
    tool("run_job", RUN_JOB, &[param("job", K::Str, D::Required)], Output::Dict),
    tool("send_test_notification", SEND_TEST_NOTIFICATION, &[], Output::Dict),
    tool("site_sessions", SITE_SESSIONS, &[], Output::List),
    tool(
        "wechat_status",
        WECHAT_STATUS,
        &[param("recent_failures", K::Int, D::Int(10))],
        Output::Dict,
    ),
    tool(
        "list_wechat_events",
        LIST_WECHAT_EVENTS,
        &[
            param("status", K::Str, D::Str("")),
            param("work", K::Str, D::Str("")),
            param("msg_ids", K::OptionalInts, D::None),
            param("include_message", K::Bool, D::Bool(false)),
            param("limit", K::Int, D::Int(20)),
        ],
        Output::List,
    ),
    tool(
        "retry_wechat_events",
        RETRY_WECHAT_EVENTS,
        &[
            param("msg_ids", K::OptionalInts, D::None),
            param("status", K::Str, D::Str("failed")),
        ],
        Output::Dict,
    ),
    tool(
        "set_wechat_state",
        SET_WECHAT_STATE,
        &[param("key", K::Str, D::Required), param("value", K::Str, D::Required)],
        Output::Dict,
    ),
    tool(
        "list_notifications",
        LIST_NOTIFICATIONS,
        &[
            param("delivery_status", K::Str, D::Str("")),
            param("source", K::Str, D::Str("")),
            param("limit", K::Int, D::Int(20)),
        ],
        Output::List,
    ),
    tool(
        "retry_notifications",
        RETRY_NOTIFICATIONS,
        &[param("ids", K::Ints, D::Required)],
        Output::Dict,
    ),
    tool("list_archive_sources", LIST_ARCHIVE_SOURCES, &[], Output::List),
    tool(
        "search_archive",
        SEARCH_ARCHIVE,
        &[
            param("source", K::Str, D::Required),
            param("query", K::Str, D::Str("")),
            param("limit", K::Int, D::Int(20)),
            param("offset", K::Int, D::Int(0)),
        ],
        Output::Dict,
    ),
];

// ---------- tools/list ----------

/// pydantic's default field title: `run_id` is `Run Id`.
fn title(name: &str) -> String {
    name.split('_')
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn object_of_anything() -> Dict {
    let mut schema = Dict::new();
    schema.insert("additionalProperties", Value::Bool(true));
    schema.insert("type", Value::from("object"));
    schema
}

fn int_array() -> Dict {
    let mut schema = Dict::new();
    schema.insert("items", json::dict([("type", Value::from("integer"))]));
    schema.insert("type", Value::from("array"));
    schema
}

/// One property of the arguments schema, with pydantic's keys in its (sorted) order.
fn property(param: &Param) -> Value {
    let mut schema = Dict::new();
    match param.kind {
        Kind::Dict => schema.insert("additionalProperties", Value::Bool(true)),
        Kind::OptionalInts => schema.insert(
            "anyOf",
            Value::List(vec![Value::Dict(int_array()), json::dict([("type", Value::from("null"))])]),
        ),
        Kind::Str | Kind::Int | Kind::Bool | Kind::Ints => {}
    }
    match param.default {
        Fallback::Required => {}
        Fallback::Str(text) => schema.insert("default", Value::from(text)),
        Fallback::Int(int) => schema.insert("default", Value::from(int)),
        Fallback::Bool(flag) => schema.insert("default", Value::Bool(flag)),
        Fallback::None => schema.insert("default", Value::Null),
    }
    if param.kind == Kind::Ints {
        schema.insert("items", json::dict([("type", Value::from("integer"))]));
    }
    schema.insert("title", Value::Str(title(param.name)));
    let kind = match param.kind {
        Kind::Str => Some("string"),
        Kind::Int => Some("integer"),
        Kind::Bool => Some("boolean"),
        Kind::Dict => Some("object"),
        Kind::Ints => Some("array"),
        Kind::OptionalInts => None,
    };
    if let Some(kind) = kind {
        schema.insert("type", Value::from(kind));
    }
    Value::Dict(schema)
}

fn input_schema(tool: &Tool) -> Value {
    let mut properties = Dict::new();
    for param in tool.params {
        properties.insert(param.name, property(param));
    }
    let required: Vec<Value> = tool
        .params
        .iter()
        .filter(|param| param.default == Fallback::Required)
        .map(|param| Value::from(param.name))
        .collect();
    let mut schema = Dict::new();
    schema.insert("properties", Value::Dict(properties));
    if !required.is_empty() {
        schema.insert("required", Value::List(required));
    }
    schema.insert("title", Value::Str(format!("{}Arguments", tool.name)));
    schema.insert("type", Value::from("object"));
    Value::Dict(schema)
}

fn output_schema(tool: &Tool) -> Value {
    let wrapped = |result: Dict| {
        json::dict([
            ("properties", json::dict([("result", Value::Dict(result))])),
            ("required", Value::List(vec![Value::from("result")])),
            ("title", Value::Str(format!("{}Output", tool.name))),
            ("type", Value::from("object")),
        ])
    };
    let list_of_objects = || {
        let mut list = Dict::new();
        list.insert("items", Value::Dict(object_of_anything()));
        list
    };
    match tool.output {
        Output::Dict => {
            let mut schema = Dict::new();
            schema.insert("additionalProperties", Value::Bool(true));
            schema.insert("title", Value::Str(format!("{}DictOutput", tool.name)));
            schema.insert("type", Value::from("object"));
            Value::Dict(schema)
        }
        Output::List => {
            let mut result = list_of_objects();
            result.insert("title", Value::from("Result"));
            result.insert("type", Value::from("array"));
            wrapped(result)
        }
        Output::ListOrDict => {
            let mut list = list_of_objects();
            list.insert("type", Value::from("array"));
            let mut result = Dict::new();
            result.insert("anyOf", Value::List(vec![Value::Dict(list), Value::Dict(object_of_anything())]));
            result.insert("title", Value::from("Result"));
            wrapped(result)
        }
    }
}

/// `tools/list`'s result.
pub(crate) fn list() -> Value {
    let tools = TOOLS
        .iter()
        .map(|tool| {
            json::dict([
                ("name", Value::from(tool.name)),
                ("description", Value::from(tool.description)),
                ("inputSchema", input_schema(tool)),
                ("outputSchema", output_schema(tool)),
            ])
        })
        .collect();
    json::dict([("tools", Value::List(tools))])
}

// ---------- arguments ----------

/// A validated argument.
#[derive(Debug, Clone)]
enum Arg {
    Str(String),
    Int(i128),
    Bool(bool),
    Dict(Dict),
    Ints(Vec<i128>),
    OptionalInts(Option<Vec<i128>>),
}

/// The validated arguments of one call, by parameter name.
#[derive(Debug, Default)]
struct Args(Vec<(&'static str, Arg)>);

impl Args {
    fn get(&self, name: &str) -> Option<&Arg> {
        self.0.iter().find(|(key, _)| *key == name).map(|(_, arg)| arg)
    }

    fn str(&self, name: &str) -> &str {
        match self.get(name) {
            Some(Arg::Str(text)) => text,
            _ => "",
        }
    }

    fn int(&self, name: &str) -> i128 {
        match self.get(name) {
            Some(Arg::Int(int)) => *int,
            _ => 0,
        }
    }

    fn flag(&self, name: &str) -> bool {
        matches!(self.get(name), Some(Arg::Bool(true)))
    }

    fn dict(&self, name: &str) -> Dict {
        match self.get(name) {
            Some(Arg::Dict(dict)) => dict.clone(),
            _ => Dict::new(),
        }
    }

    fn ints(&self, name: &str) -> Option<Vec<i128>> {
        match self.get(name) {
            Some(Arg::Ints(ints)) => Some(ints.clone()),
            Some(Arg::OptionalInts(ints)) => ints.clone(),
            _ => None,
        }
    }
}

/// `pre_parse_json`: a string given for a parameter that is not `str` is replaced by the
/// JSON it holds, unless that is itself a string or a number (`bool` included).
fn pre_parse(tool: &Tool, arguments: &Dict) -> Dict {
    let mut parsed = arguments.clone();
    for (key, value) in arguments.iter() {
        let pyjson::Key::Str(name) = key else {
            continue;
        };
        let Some(param) = tool.params.iter().find(|param| param.name == name) else {
            continue;
        };
        let Value::Str(text) = value else {
            continue;
        };
        if param.kind == Kind::Str {
            continue;
        }
        match pyjson::loads(text) {
            Ok(Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_)) | Err(_) => {}
            Ok(other) => parsed.insert(name.clone(), other),
        }
    }
    parsed
}

fn dict_arg(value: &Value, loc: &[LocItem]) -> Result<Dict, Errors> {
    match value {
        Value::Dict(dict) => Ok(dict.clone()),
        other => Err(vec![ErrorItem {
            kind: "dict_type",
            loc: loc.to_vec(),
            msg: "Input should be a valid dictionary".to_owned(),
            input: other.clone(),
            ctx: None,
        }]),
    }
}

fn convert(kind: Kind, value: &Value, loc: &[LocItem]) -> Result<Arg, Errors> {
    match kind {
        Kind::Str => validate::string(value, loc).map(Arg::Str),
        Kind::Int => validate::int_unbounded(value, loc).map(Arg::Int),
        Kind::Bool => validate::boolean(value, loc).map(Arg::Bool),
        Kind::Dict => dict_arg(value, loc).map(Arg::Dict),
        Kind::Ints => validate::list_of(validate::int_unbounded)(value, loc).map(Arg::Ints),
        Kind::OptionalInts => validate::nullable(validate::list_of(validate::int_unbounded))(value, loc).map(Arg::OptionalInts),
    }
}

fn default_arg(param: &Param) -> Arg {
    match param.default {
        Fallback::Str(text) => Arg::Str(text.to_owned()),
        Fallback::Int(int) => Arg::Int(i128::from(int)),
        Fallback::Bool(flag) => Arg::Bool(flag),
        Fallback::Required | Fallback::None => Arg::OptionalInts(None),
    }
}

/// `<tool>Arguments.model_validate(arguments)`; extra arguments are ignored.
fn validate_arguments(tool: &Tool, arguments: &Dict) -> Result<Args, String> {
    let whole = Value::Dict(pre_parse(tool, arguments));
    let mut fields =
        Fields::begin(&whole, &[], tool.name).map_err(|errors| pydantic::error_text(&format!("{}Arguments", tool.name), &errors))?;
    let mut args = Vec::new();
    for param in tool.params {
        let converted = if param.default == Fallback::Required {
            fields.required(param.name, |value, loc| convert(param.kind, value, loc))
        } else {
            fields.optional(param.name, || default_arg(param), |value, loc| convert(param.kind, value, loc))
        };
        if let Some(arg) = converted {
            args.push((param.name, arg));
        }
    }
    fields
        .finish(|| Some(Args(args)), |_| Ok(()))
        .map_err(|errors| pydantic::error_text(&format!("{}Arguments", tool.name), &errors))
}

// ---------- calls ----------

/// What a tool returned.
enum Returned {
    Dict(Value),
    List(Vec<Value>),
}

/// `_tool_error`: the service's message, and its details when there are any.
fn service_error(error: ApiError) -> String {
    if error.details.is_truthy() {
        format!("{} Details: {}", error.message, repr_value(&error.details))
    } else {
        error.message
    }
}

#[allow(clippy::needless_pass_by_value)] // Used as `map_err(ops_error)`.
fn ops_error(error: OpsError) -> String {
    error.message().to_owned()
}

fn mcp_log() -> fav_core::logging::Logger {
    fav_core::logging::get("fav-mcp")
}

/// `_merge`: nested dictionaries are merged, everything else replaced.
pub(crate) fn merge(base: &Dict, changes: &Dict) -> Dict {
    let mut merged = base.clone();
    for (key, value) in changes.iter() {
        let name = fav_runtime::pystr::str_key(key);
        let replacement = match (merged.get(&name), value) {
            (Some(Value::Dict(current)), Value::Dict(change)) => Value::Dict(merge(current, change)),
            _ => value.clone(),
        };
        merged.insert(key.clone(), replacement);
    }
    merged
}

fn as_list(value: Vec<Value>) -> Returned {
    Returned::List(value)
}

#[allow(clippy::too_many_lines)] // One arm per tool, each the body of its Python function.
async fn run(state: &ApiState, name: &str, args: &Args) -> Result<Returned, String> {
    let now = (state.clock)();
    let zone = state.mcp.timezone;
    match name {
        "get_runtime" => Ok(Returned::Dict(ops::runtime_info(state))),
        "list_jobs" => {
            let jobs = handlers::scheduled_jobs(state).await.map_err(service_error)?;
            let last_runs = ops::last_runs_by_job(&state.db).await.map_err(ops_error)?;
            Ok(as_list(ops::describe_jobs(&jobs, &last_runs, zone, now)))
        }
        "list_runs" => {
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            let query = RunQuery {
                job: args.str("job").to_owned(),
                status: args.str("status").to_owned(),
                kind: args.str("kind").to_owned(),
                since: ops::parse_since(args.str("since"), now).map_err(ops_error)?,
                limit: args.int("limit"),
            };
            ops::list_runs(&state.db, &query, &redact).await.map(as_list).map_err(ops_error)
        }
        "get_run" => {
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            let run_id = args.int("run_id");
            match ops::get_run(&state.db, run_id, &redact).await.map_err(ops_error)? {
                Some(run) => Ok(Returned::Dict(run)),
                None => Err(format!("Run {run_id} not found.")),
            }
        }
        "read_logs" => {
            let since = ops::parse_since(args.str("since"), now).map_err(ops_error)?;
            let until = ops::parse_since(args.str("until"), now).map_err(ops_error)?;
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            let query = LogQuery {
                since,
                until,
                contains: args.str("contains").to_owned(),
                logger_name: args.str("logger_name").to_owned(),
                min_level: args.str("level").to_owned(),
                limit: args.int("limit"),
            };
            // Reading and merging the log files is blocking file I/O: keep it off the
            // async workers.
            let log_dir = state.mcp.log_dir.clone();
            let read = tokio::task::spawn_blocking(move || ops::read_logs(&log_dir, &query, zone, &redact));
            match read.await {
                Ok(result) => result.map(Returned::Dict).map_err(ops_error),
                Err(error) => std::panic::resume_unwind(error.into_panic()),
            }
        }
        "get_settings" => {
            let section = args.str("section");
            if section.is_empty() {
                let sections = handlers::list_settings_sections(state).await.map_err(service_error)?;
                Ok(Returned::List(sections))
            } else {
                handlers::get_settings_section(state, section)
                    .await
                    .map(Returned::Dict)
                    .map_err(service_error)
            }
        }
        "update_settings" => {
            let section = args.str("section");
            let values = args.dict("values");
            let current = handlers::get_settings_section(state, section).await.map_err(service_error)?;
            let current = match current.get("value") {
                Some(Value::Dict(value)) => value.clone(),
                _ => Dict::new(),
            };
            let merged = merge(&current, &values);
            let result = handlers::save_settings_section(state, section, &merged)
                .await
                .map_err(service_error)?;
            let mut fields: Vec<String> = values.iter().map(|(key, _)| fav_runtime::pystr::str_key(key)).collect();
            fields.sort();
            let fields = Value::List(fields.into_iter().map(Value::Str).collect());
            mcp_log().info(format!("MCP updated settings section={section} fields={}", repr_value(&fields)));
            Ok(Returned::Dict(result))
        }
        "run_job" => {
            let job = args.str("job");
            let result = handlers::create_job_request_value(state, job).await.map_err(service_error)?;
            let id = result.get("id").map_or_else(|| "None".to_owned(), fav_runtime::pystr::str_value);
            mcp_log().info(format!("MCP queued job={job} request={id}"));
            Ok(Returned::Dict(result))
        }
        "send_test_notification" => handlers::send_test_notification(state)
            .await
            .map(Returned::Dict)
            .map_err(service_error),
        "site_sessions" => ops::site_sessions_summary(&state.db).await.map(as_list).map_err(ops_error),
        "wechat_status" => {
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            ops::wechat_status(&state.db, &redact, args.int("recent_failures"), zone)
                .await
                .map(Returned::Dict)
                .map_err(ops_error)
        }
        "list_wechat_events" => {
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            let query = EventQuery {
                status: args.str("status").to_owned(),
                work: args.str("work").to_owned(),
                msg_ids: args.ints("msg_ids"),
                include_message: args.flag("include_message"),
                limit: args.int("limit"),
            };
            ops::list_wechat_events(&state.db, &query, &redact, zone)
                .await
                .map(as_list)
                .map_err(ops_error)
        }
        "retry_wechat_events" => {
            let ids = args.ints("msg_ids");
            let retried = ops::retry_wechat_events(&state.db, ids.as_deref(), args.str("status"))
                .await
                .map_err(ops_error)?;
            mcp_log().info(format!("MCP retried WeChat events count={}", retried.len()));
            Ok(Returned::Dict(retried_value(retried)))
        }
        "set_wechat_state" => {
            let key = args.str("key");
            let result = ops::set_wechat_state(&state.db, key, args.str("value")).await.map_err(ops_error)?;
            let shown = |name: &str| result.get(name).map_or_else(|| "None".to_owned(), fav_runtime::pystr::str_value);
            mcp_log().info(format!(
                "MCP set WeChat state {key} from {} to {}",
                shown("previous"),
                shown("value")
            ));
            Ok(Returned::Dict(result))
        }
        "list_notifications" => {
            let redact = ops::redactor(state).await.map_err(ops_error)?;
            let query = NotificationQuery {
                delivery_status: args.str("delivery_status").to_owned(),
                source: args.str("source").to_owned(),
                limit: args.int("limit"),
            };
            ops::list_notifications(&state.db, &query, &redact)
                .await
                .map(as_list)
                .map_err(ops_error)
        }
        "retry_notifications" => {
            let ids = args.ints("ids").unwrap_or_default();
            let retried = ops::retry_notifications(&state.db, &ids).await.map_err(ops_error)?;
            mcp_log().info(format!("MCP retried notifications count={}", retried.len()));
            Ok(Returned::Dict(retried_value(retried)))
        }
        "list_archive_sources" => handlers::archive_sources(state).await.map(as_list).map_err(service_error),
        "search_archive" => {
            let clamp = |value: i128| i64::try_from(value.clamp(i128::from(i64::MIN), i128::from(i64::MAX))).unwrap_or_default();
            handlers::archive_items(
                state,
                args.str("source"),
                args.str("query"),
                clamp(args.int("limit")),
                clamp(args.int("offset")),
            )
            .await
            .map(Returned::Dict)
            .map_err(service_error)
        }
        other => Err(format!("Unknown tool: {other}")),
    }
}

fn retried_value(retried: Vec<Value>) -> Value {
    let count = i64::try_from(retried.len()).unwrap_or(i64::MAX);
    json::dict([("retried", Value::List(retried)), ("count", Value::from(count))])
}

/// `_make_error_result`.
fn error_result(message: String) -> Value {
    json::dict([
        ("content", Value::List(vec![text_content(message)])),
        ("isError", Value::Bool(true)),
    ])
}

/// `_convert_to_content`: a string is the text itself, anything else its indented JSON.
fn content_text(value: &Value) -> String {
    match value {
        Value::Str(text) => text.clone(),
        other => json_text::pretty(other),
    }
}

fn text_content(text: String) -> Value {
    json::dict([("type", Value::from("text")), ("text", Value::Str(text))])
}

/// `tools/call`'s result: the tool's output, or the error a failure becomes.
pub(crate) async fn call(state: &ApiState, name: &str, arguments: &Dict) -> Value {
    let Some(tool) = TOOLS.iter().find(|tool| tool.name == name) else {
        fav_core::logging::get_external("mcp.server.lowlevel.server")
            .warning(format_args!("Tool '{name}' not listed, no validation will be performed"));
        return error_result(format!("Unknown tool: {name}"));
    };
    let outcome = match validate_arguments(tool, arguments) {
        Ok(args) => run(state, tool.name, &args).await,
        Err(message) => Err(message),
    };
    let returned = match outcome {
        Ok(returned) => returned,
        Err(message) => return error_result(format!("Error executing tool {}: {message}", tool.name)),
    };
    let (content, structured) = match returned {
        Returned::List(items) => (
            items.iter().map(|item| text_content(content_text(item))).collect(),
            json::dict([("result", Value::List(items))]),
        ),
        Returned::Dict(value) => {
            let content = vec![text_content(content_text(&value))];
            let structured = if tool.output == Output::ListOrDict {
                json::dict([("result", value)])
            } else {
                value
            };
            (content, structured)
        }
    };
    json::dict([
        ("content", Value::List(content)),
        ("structuredContent", structured),
        ("isError", Value::Bool(false)),
    ])
}

// ---------- descriptions: the docstrings, as written ----------

const GET_RUNTIME: &str = "Deployed version and process state: git commit and build time of the image, pod name,\n        when the container, API and worker processes started, and whether the worker is running.";
const LIST_JOBS: &str = "Every job (one per source): enabled, cron, next run time, settings section, fields still\n        missing before it can run, and its latest run.";
const LIST_RUNS: &str = "Recent runs, newest first, from control_requests.\n\n        Args:\n            job: job key such as wechat or azurlane ('all' for a manual run of every job).\n            status: pending, running, succeeded, failed, rejected or interrupted.\n            kind: scheduled_job or trigger_job (manual).\n            since: ISO 8601 time or relative 30m/6h/2d.\n            limit: at most 500. Errors are cut at 4000 characters; get_run returns them whole.\n        ";
const GET_RUN: &str = "One run with its full result and error text.";
const READ_LOGS: &str = "Application log records from the log files on the data volume (API and worker), which\n        outlive pod restarts. Returns the newest matching records, oldest first.\n\n        Args:\n            since: ISO 8601 time or relative 30m/6h/2d.\n            until: optional end of the window, same formats.\n            contains: case-insensitive substring of the message (tracebacks included).\n            logger_name: case-insensitive substring of the logger, e.g. wechat, azurlane, fav-api.\n            level: minimum level: DEBUG, INFO, NOTICE, WARNING, ERROR or CRITICAL.\n            limit: at most 1000 records.\n        ";
const GET_SETTINGS: &str = "Settings as the web UI shows them, secrets masked. Without a section, every section.\n        Sections are named like web.wechat, web.azurlane, notifications.telegram.";
const UPDATE_SETTINGS: &str = "Change settings in one section. `values` holds only the fields to change; nested objects\n        are merged and lists are replaced whole. Masked secrets are kept as stored. The change is\n        validated like a save from the UI and applies to the next run without a restart.";
const RUN_JOB: &str = "Queue a manual run of a job (or 'all' for every enabled job), the same as the Run button.\n        The worker claims it within about a second; poll get_run with the returned id.";
const SEND_TEST_NOTIFICATION: &str = "Send a Telegram test message to check that the bot can reach the chat.";
const SITE_SESSIONS: &str = "Logins captured on the web UI's 登录态 page (pixiv, rednote): account, whether the last\n        crawl or check found them valid (status, message), cookie names and expiry. Never values.";
const WECHAT_STATUS: &str = "WeChat source state: wechat_state (events_after cursor, work_version), event counts by\n        work and status, and the latest failed or retrying events with their errors.";
const LIST_WECHAT_EVENTS: &str = "WeChat events queued for processing, newest first.\n\n        Args:\n            status: pending, done or failed.\n            work: the kind of work, e.g. image, article, channel_video.\n            msg_ids: only these message ids.\n            include_message: also return the stored bridge message (its content and media keys).\n            limit: at most 500.\n        ";
const RETRY_WECHAT_EVENTS: &str = "Put WeChat events back to pending with a fresh attempt budget, so the next WeChat run\n        processes them again. Without msg_ids, every event in `status` (default failed).";
const SET_WECHAT_STATE: &str = "Set a WeChat source state value. events_after is the bridge event cursor (0 re-reads the\n        whole event log; already queued events are not duplicated); work_version decides whether\n        the log is re-read once after the set of handled message types changes.";
const LIST_NOTIFICATIONS: &str = "Telegram notification outbox, newest first. delivery_status: pending, sending, delivered\n        or failed; source: the job that queued it (or worker for job failures).";
const RETRY_NOTIFICATIONS: &str = "Send failed notifications again with a fresh attempt budget.";
const LIST_ARCHIVE_SOURCES: &str = "What has been collected per source (counts and latest item), as on the Records page.";
const SEARCH_ARCHIVE: &str = "Collected items of one source, newest first, optionally filtered by a text query.";
