//! The OpenAPI document (`/openapi.json`).
//!
//! Python origin: what FastAPI generates from `src/api/routes.py`, `credentials.py`,
//! `app.py` and the pydantic models in `schemas.py`. Built with utoipa's builders rather
//! than its derive macros, because pydantic's output carries details the derives cannot
//! express (a `title` on every field, `anyOf` for optional values, `const` for literals).
//! The document is compared with Python's (`rust/goldens/api/openapi.json`) path by path:
//! every path is either here and identical, or listed in [`PENDING`] with the work package
//! that brings it.

use std::collections::BTreeSet;

use fav_core::pyjson::Value;
use serde_json::json;
use utoipa::openapi::extensions::Extensions;
use utoipa::openapi::path::{HttpMethod, OperationBuilder, ParameterBuilder, ParameterIn, PathItem};
use utoipa::openapi::request_body::RequestBodyBuilder;
use utoipa::openapi::schema::{AdditionalProperties, AnyOfBuilder, ArrayBuilder, ObjectBuilder, Schema, SchemaType, Type};
use utoipa::openapi::{
    ComponentsBuilder, ContentBuilder, InfoBuilder, OpenApi, OpenApiBuilder, PathsBuilder, Ref, RefOr, Required, ResponseBuilder,
};

use crate::handlers::{JOB_REQUEST_STATUSES, JOB_REQUEST_TARGETS, SETTINGS_SECTION_PATTERN, SITE_PATTERN};

pub const API_TITLE: &str = "fav API";
pub const API_DESCRIPTION: &str = "Internal control and Hanime1 scan-target creation API for fav.";
pub const APP_VERSION: &str = "2.0.0";

/// Routes Python has and this build does not yet, with the work package that ports each.
/// Every path of Python's document that is missing here must be listed.
pub const PENDING: &[(&str, &str)] = &[
    ("/api/v2/services/tikhub/test", "WP13"),
    ("/api/v2/azurlane/proxy/test", "WP11"),
    ("/api/v2/rednote/proxy/test", "WP17"),
    ("/api/v2/hanime1/videos", "WP8"),
    ("/api/v2/hanime1/seeds", "WP8"),
    ("/api/v2/hanime1/seeds/{canonical_video_id}", "WP8"),
    ("/api/v2/hanime1/authors", "WP8"),
    ("/api/v2/hanime1/authors/{author_id}", "WP8"),
    ("/api/v2/kemono/creators/resolve", "WP5"),
    ("/api/v2/azurlane/characters", "WP12"),
    ("/api/v2/azurlane/sidebar/characters", "WP12"),
    ("/api/v2/azurlane/skin-updates", "WP12"),
    ("/api/v2/azurlane/characters/{character_key}", "WP12"),
    ("/api/v2/azurlane/characters/{character_key}/ship-detail", "WP12"),
    ("/api/v2/bd2/characters", "WP10"),
    ("/api/v2/bd2/sidebar/characters", "WP10"),
    ("/api/v2/bd2/characters/{content_id}", "WP10"),
    (
        "/api/v2/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}",
        "WP10",
    ),
    ("/api/v2/nikke/characters", "WP9"),
    ("/api/v2/nikke/sidebar/characters", "WP9"),
    ("/api/v2/nikke/characters/{content_id}", "WP9"),
    (
        "/api/v2/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}",
        "WP9",
    ),
    ("/api/v2/site-sessions/{site}/login", "WP16"),
    ("/api/v2/site-sessions/login", "WP16"),
    ("/readyz", "WP8"),
];

/// Routes outside the document that are pending too: the VNC relay.
pub const PENDING_UNDOCUMENTED: &[(&str, &str)] = &[("/api/v2/site-sessions/vnc", "WP16")];

// ---------- schema helpers ----------

fn extensions(pairs: &[(&str, serde_json::Value)]) -> Extensions {
    pairs.iter().map(|(key, value)| (*key, value.clone())).collect()
}

fn typed(kind: Type, title: &str) -> ObjectBuilder {
    ObjectBuilder::new().schema_type(kind).title(Some(title))
}

fn string(title: &str) -> RefOr<Schema> {
    typed(Type::String, title).into()
}

fn integer(title: &str) -> RefOr<Schema> {
    typed(Type::Integer, title).into()
}

fn boolean(title: &str) -> RefOr<Schema> {
    typed(Type::Boolean, title).into()
}

fn with_default(builder: ObjectBuilder, value: serde_json::Value) -> RefOr<Schema> {
    builder.default(Some(value)).into()
}

/// A literal (`Literal['ok']`).
fn constant(title: &str, value: &str) -> RefOr<Schema> {
    typed(Type::String, title)
        .extensions(Some(extensions(&[("const", json!(value))])))
        .into()
}

/// `T | None`: `anyOf` of the type and null.
fn nullable(kind: Type, title: &str) -> RefOr<Schema> {
    AnyOfBuilder::new()
        .item(ObjectBuilder::new().schema_type(kind))
        .item(ObjectBuilder::new().schema_type(Type::Null))
        .extensions(Some(extensions(&[("title", json!(title))])))
        .into()
}

fn reference(name: &str) -> RefOr<Schema> {
    Ref::from_schema_name(name).into()
}

fn array_of(items: RefOr<Schema>, title: &str) -> RefOr<Schema> {
    ArrayBuilder::new().items(items).title(Some(title)).into()
}

fn string_list(title: &str) -> RefOr<Schema> {
    array_of(ObjectBuilder::new().schema_type(Type::String).into(), title)
}

fn free_object(title: &str) -> RefOr<Schema> {
    typed(Type::Object, title)
        .additional_properties(Some(AdditionalProperties::FreeForm(true)))
        .into()
}

fn string_enum(name: &str, values: &[&str]) -> RefOr<Schema> {
    typed(Type::String, name).enum_values(Some(values.iter().copied())).into()
}

/// One field of a model: name, schema, required.
type Field = (&'static str, RefOr<Schema>, bool);

/// A pydantic model; `forbid` is `extra='forbid'` (`ApiSchema`).
fn model(name: &str, description: Option<&str>, forbid: bool, fields: Vec<Field>) -> RefOr<Schema> {
    let mut builder = typed(Type::Object, name).description(description);
    for (field, schema, required) in fields {
        builder = builder.property(field, schema);
        if required {
            builder = builder.required(field);
        }
    }
    if forbid {
        builder = builder.additional_properties(Some(AdditionalProperties::FreeForm(false)));
    }
    builder.into()
}

/// `{items: [ref], total}`.
fn list_model(name: &str, item: &str) -> RefOr<Schema> {
    model(
        name,
        None,
        true,
        vec![
            ("items", array_of(reference(item), "Items"), true),
            ("total", integer("Total"), true),
        ],
    )
}

/// The components, alphabetically, as FastAPI lists them.
fn schemas() -> Vec<(&'static str, RefOr<Schema>)> {
    [schemas_a(), schemas_b(), schemas_c(), schemas_d()].concat()
}

fn schemas_a() -> Vec<(&'static str, RefOr<Schema>)> {
    vec![
        (
            "ArchiveItem",
            model(
                "ArchiveItem",
                None,
                true,
                vec![
                    ("source", string("Source"), true),
                    ("id", string("Id"), true),
                    ("title", string("Title"), true),
                    ("subtitle", with_default(typed(Type::String, "Subtitle"), json!("")), false),
                    ("created_at", nullable(Type::String, "Created At"), false),
                    ("url", nullable(Type::String, "Url"), false),
                    ("fields", free_object("Fields"), false),
                ],
            ),
        ),
        (
            "ArchiveListResponse",
            model(
                "ArchiveListResponse",
                None,
                true,
                vec![
                    ("items", array_of(reference("ArchiveItem"), "Items"), true),
                    ("total", integer("Total"), true),
                    ("limit", integer("Limit"), true),
                    ("offset", integer("Offset"), true),
                ],
            ),
        ),
        (
            "ArchiveSourceListResponse",
            list_model("ArchiveSourceListResponse", "ArchiveSourceStat"),
        ),
        (
            "ArchiveSourceStat",
            model(
                "ArchiveSourceStat",
                None,
                true,
                vec![
                    ("source", string("Source"), true),
                    ("name", string("Name"), true),
                    ("total", integer("Total"), true),
                    ("latest_at", nullable(Type::String, "Latest At"), false),
                ],
            ),
        ),
        (
            "CredentialCookie",
            model(
                "CredentialCookie",
                None,
                false,
                vec![
                    ("name", string("Name"), true),
                    ("value", string("Value"), true),
                    ("domain", string("Domain"), true),
                    ("path", string("Path"), true),
                    ("expires", nullable(Type::Number, "Expires"), true),
                ],
            ),
        ),
    ]
}

fn schemas_b() -> Vec<(&'static str, RefOr<Schema>)> {
    vec![
        (
            "HTTPValidationError",
            model(
                "HTTPValidationError",
                None,
                false,
                vec![("detail", array_of(reference("ValidationError"), "Detail"), false)],
            ),
        ),
        (
            "HealthResponse",
            model(
                "HealthResponse",
                None,
                true,
                vec![
                    ("status", constant("Status", "ok"), true),
                    ("generated_at", string("Generated At"), true),
                ],
            ),
        ),
        ("JobListResponse", list_model("JobListResponse", "JobSummary")),
        (
            "JobRequest",
            model(
                "JobRequest",
                None,
                true,
                vec![
                    ("id", integer("Id"), true),
                    ("target", reference("JobRequestTarget"), true),
                    ("kind", with_default(typed(Type::String, "Kind"), json!("trigger_job")), false),
                    ("status", reference("JobRequestStatus"), true),
                    ("requested_at", string("Requested At"), true),
                    ("started_at", nullable(Type::String, "Started At"), false),
                    ("finished_at", nullable(Type::String, "Finished At"), false),
                    ("result", with_default(typed(Type::String, "Result"), json!("")), false),
                    ("error", with_default(typed(Type::String, "Error"), json!("")), false),
                ],
            ),
        ),
        (
            "JobRequestCreate",
            model(
                "JobRequestCreate",
                None,
                true,
                vec![("target", reference("JobRequestTarget"), true)],
            ),
        ),
        ("JobRequestListResponse", list_model("JobRequestListResponse", "JobRequest")),
        ("JobRequestStatus", string_enum("JobRequestStatus", &JOB_REQUEST_STATUSES)),
        ("JobRequestTarget", string_enum("JobRequestTarget", &JOB_REQUEST_TARGETS)),
    ]
}

fn schemas_c() -> Vec<(&'static str, RefOr<Schema>)> {
    vec![
        (
            "JobSummary",
            model(
                "JobSummary",
                None,
                true,
                vec![
                    ("key", string("Key"), true),
                    ("name", string("Name"), true),
                    ("enabled", boolean("Enabled"), true),
                    ("notify", with_default(typed(Type::Boolean, "Notify"), json!(true)), false),
                    ("cron", string("Cron"), true),
                    ("section", with_default(typed(Type::String, "Section"), json!("")), false),
                    ("missing_fields", string_list("Missing Fields"), false),
                ],
            ),
        ),
        (
            "LoginAttemptView",
            model(
                "LoginAttemptView",
                None,
                true,
                vec![
                    ("site", string("Site"), true),
                    (
                        "state",
                        typed(Type::String, "State")
                            .enum_values(Some(["starting", "waiting", "saved", "failed", "timeout", "cancelled"]))
                            .into(),
                        true,
                    ),
                    ("active", boolean("Active"), true),
                    ("message", with_default(typed(Type::String, "Message"), json!("")), false),
                    ("account", with_default(typed(Type::String, "Account"), json!("")), false),
                    ("started_at", nullable(Type::String, "Started At"), false),
                    ("deadline", nullable(Type::String, "Deadline"), false),
                    ("finished_at", nullable(Type::String, "Finished At"), false),
                    ("ticket", with_default(typed(Type::String, "Ticket"), json!("")), false),
                ],
            ),
        ),
        ("SettingsListResponse", list_model("SettingsListResponse", "SettingsSection")),
        (
            "SettingsSection",
            model(
                "SettingsSection",
                None,
                true,
                vec![
                    ("section", string("Section"), true),
                    ("value", free_object("Value"), true),
                    ("missing_fields", string_list("Missing Fields"), false),
                ],
            ),
        ),
        (
            "SiteCredential",
            model(
                "SiteCredential",
                None,
                false,
                vec![
                    ("site", string("Site"), true),
                    ("account", string("Account"), true),
                    ("status", string("Status"), true),
                    ("signed_in_at", nullable(Type::String, "Signed In At"), true),
                    ("checked_at", nullable(Type::String, "Checked At"), true),
                    ("cookies", array_of(reference("CredentialCookie"), "Cookies"), true),
                ],
            ),
        ),
    ]
}

fn schemas_d() -> Vec<(&'static str, RefOr<Schema>)> {
    vec![
        (
            "SiteSessionListResponse",
            model(
                "SiteSessionListResponse",
                None,
                true,
                vec![
                    ("items", array_of(reference("SiteSessionView"), "Items"), true),
                    (
                        "login",
                        AnyOfBuilder::new()
                            .item(Ref::from_schema_name("LoginAttemptView"))
                            .item(ObjectBuilder::new().schema_type(Type::Null))
                            .into(),
                        false,
                    ),
                ],
            ),
        ),
        (
            "SiteSessionView",
            model(
                "SiteSessionView",
                Some("One site's stored login, as the 登录态 page shows it. Never carries cookie values."),
                true,
                vec![
                    ("site", string("Site"), true),
                    ("label", string("Label"), true),
                    ("signed_in", boolean("Signed In"), true),
                    ("hint", with_default(typed(Type::String, "Hint"), json!("")), false),
                    ("account", with_default(typed(Type::String, "Account"), json!("")), false),
                    ("status", with_default(typed(Type::String, "Status"), json!("")), false),
                    ("message", with_default(typed(Type::String, "Message"), json!("")), false),
                    ("cookie_count", with_default(typed(Type::Integer, "Cookie Count"), json!(0)), false),
                    ("expires_at", nullable(Type::String, "Expires At"), false),
                    ("signed_in_at", nullable(Type::String, "Signed In At"), false),
                    ("checked_at", nullable(Type::String, "Checked At"), false),
                ],
            ),
        ),
        (
            "TelegramNotificationTestResponse",
            model(
                "TelegramNotificationTestResponse",
                None,
                true,
                vec![
                    ("status", constant("Status", "delivered"), true),
                    ("message_id", nullable(Type::Integer, "Message Id"), false),
                    ("warnings", string_list("Warnings"), false),
                ],
            ),
        ),
        (
            "ValidationError",
            model(
                "ValidationError",
                None,
                false,
                vec![
                    (
                        "loc",
                        array_of(
                            AnyOfBuilder::new()
                                .item(ObjectBuilder::new().schema_type(Type::String))
                                .item(ObjectBuilder::new().schema_type(Type::Integer))
                                .into(),
                            "Location",
                        ),
                        true,
                    ),
                    ("msg", string("Message"), true),
                    ("type", string("Error Type"), true),
                    (
                        "input",
                        ObjectBuilder::new().schema_type(SchemaType::AnyValue).title(Some("Input")).into(),
                        false,
                    ),
                    ("ctx", typed(Type::Object, "Context").into(), false),
                ],
            ),
        ),
    ]
}

// ---------- operations ----------

/// One operation: tag, summary, operation id, optional description.
struct Op {
    tag: &'static str,
    summary: &'static str,
    id: &'static str,
    description: Option<&'static str>,
    status: &'static str,
    response: &'static str,
    parameters: Vec<utoipa::openapi::path::Parameter>,
    body: Option<RefOr<Schema>>,
}

impl Op {
    fn new(tag: &'static str, summary: &'static str, id: &'static str, response: &'static str) -> Self {
        Self {
            tag,
            summary,
            id,
            description: None,
            status: "200",
            response,
            parameters: Vec::new(),
            body: None,
        }
    }

    fn build(self) -> utoipa::openapi::path::Operation {
        let json_content = |schema: RefOr<Schema>| ContentBuilder::new().schema(Some(schema)).build();
        let validates = !self.parameters.is_empty() || self.body.is_some();
        let mut builder = OperationBuilder::new()
            .tag(self.tag)
            .summary(Some(self.summary))
            .description(self.description)
            .operation_id(Some(self.id))
            .response(
                self.status,
                ResponseBuilder::new()
                    .description("Successful Response")
                    .content("application/json", json_content(reference(self.response))),
            );
        if validates {
            builder = builder.response(
                "422",
                ResponseBuilder::new()
                    .description("Validation Error")
                    .content("application/json", json_content(reference("HTTPValidationError"))),
            );
        }
        if !self.parameters.is_empty() {
            builder = builder.parameters(Some(self.parameters));
        }
        if let Some(body) = self.body {
            builder = builder.request_body(Some(
                RequestBodyBuilder::new()
                    .required(Some(Required::True))
                    .content("application/json", json_content(body))
                    .build(),
            ));
        }
        builder.build()
    }
}

fn parameter(name: &str, location: ParameterIn, required: bool, schema: RefOr<Schema>) -> utoipa::openapi::path::Parameter {
    ParameterBuilder::new()
        .name(name)
        .parameter_in(location)
        .required(if required { Required::True } else { Required::False })
        .schema(Some(schema))
        .build()
}

fn bounded_int(title: &str, minimum: Option<i64>, maximum: Option<i64>, default: i64) -> RefOr<Schema> {
    typed(Type::Integer, title)
        .minimum(minimum)
        .maximum(maximum)
        .default(Some(json!(default)))
        .into()
}

fn section_parameter() -> utoipa::openapi::path::Parameter {
    parameter(
        "section",
        ParameterIn::Path,
        true,
        typed(Type::String, "Section")
            .min_length(Some(1))
            .max_length(Some(64))
            .pattern(Some(SETTINGS_SECTION_PATTERN))
            .into(),
    )
}

/// The paths in registration order.
fn paths() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    let mut paths = paths_a();
    paths.extend(paths_b());
    paths
}

fn paths_a() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
        (
            "/api/v2/jobs",
            vec![(HttpMethod::Get, Op::new("jobs", "List Jobs", "listJobs", "JobListResponse"))],
        ),
        (
            "/api/v2/job-requests",
            vec![
                (
                    HttpMethod::Post,
                    Op {
                        status: "202",
                        body: Some(reference("JobRequestCreate")),
                        ..Op::new("jobs", "Create Job Request", "createJobRequest", "JobRequest")
                    },
                ),
                (
                    HttpMethod::Get,
                    Op {
                        parameters: vec![
                            parameter(
                                "status",
                                ParameterIn::Query,
                                false,
                                AnyOfBuilder::new()
                                    .item(ArrayBuilder::new().items(Ref::from_schema_name("JobRequestStatus")))
                                    .item(ObjectBuilder::new().schema_type(Type::Null))
                                    .extensions(Some(extensions(&[("title", json!("Status"))])))
                                    .into(),
                            ),
                            parameter("limit", ParameterIn::Query, false, bounded_int("Limit", Some(1), Some(200), 50)),
                        ],
                        ..Op::new("jobs", "List Job Requests", "listJobRequests", "JobRequestListResponse")
                    },
                ),
            ],
        ),
        (
            "/api/v2/job-requests/{request_id}",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![parameter(
                        "request_id",
                        ParameterIn::Path,
                        true,
                        typed(Type::Integer, "Request Id").exclusive_minimum(Some(0)).into(),
                    )],
                    ..Op::new("jobs", "Get Job Request", "getJobRequest", "JobRequest")
                },
            )],
        ),
        (
            "/api/v2/settings",
            vec![(
                HttpMethod::Get,
                Op::new("settings", "List Settings", "listSettings", "SettingsListResponse"),
            )],
        ),
        (
            "/api/v2/settings/{section}",
            vec![
                (
                    HttpMethod::Get,
                    Op {
                        parameters: vec![section_parameter()],
                        ..Op::new("settings", "Get Settings Section", "getSettingsSection", "SettingsSection")
                    },
                ),
                (
                    HttpMethod::Put,
                    Op {
                        parameters: vec![section_parameter()],
                        body: Some(free_object("Payload")),
                        ..Op::new("settings", "Update Settings Section", "updateSettingsSection", "SettingsSection")
                    },
                ),
            ],
        ),
    ]
}

fn paths_b() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
        (
            "/api/v2/notifications/telegram/test",
            vec![(
                HttpMethod::Post,
                Op {
                    description: Some("Send a test message with the stored bot credentials."),
                    ..Op::new(
                        "settings",
                        "Test Telegram Notification",
                        "testTelegramNotification",
                        "TelegramNotificationTestResponse",
                    )
                },
            )],
        ),
        (
            "/api/v2/archive/sources",
            vec![(
                HttpMethod::Get,
                Op::new("archive", "List Archive Sources", "listArchiveSources", "ArchiveSourceListResponse"),
            )],
        ),
        (
            "/api/v2/archive/items",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![
                        parameter(
                            "source",
                            ParameterIn::Query,
                            true,
                            typed(Type::String, "Source").min_length(Some(1)).max_length(Some(40)).into(),
                        ),
                        parameter(
                            "q",
                            ParameterIn::Query,
                            false,
                            AnyOfBuilder::new()
                                .item(
                                    ObjectBuilder::new()
                                        .schema_type(Type::String)
                                        .min_length(Some(1))
                                        .max_length(Some(200)),
                                )
                                .item(ObjectBuilder::new().schema_type(Type::Null))
                                .extensions(Some(extensions(&[("title", json!("Q"))])))
                                .into(),
                        ),
                        parameter("limit", ParameterIn::Query, false, bounded_int("Limit", Some(1), Some(200), 50)),
                        parameter("offset", ParameterIn::Query, false, bounded_int("Offset", Some(0), None, 0)),
                    ],
                    ..Op::new("archive", "List Archive Items", "listArchiveItems", "ArchiveListResponse")
                },
            )],
        ),
        (
            "/api/v2/site-sessions",
            vec![(
                HttpMethod::Get,
                Op {
                    description: Some("Each login-backed site with its stored session, plus the browser login in progress, if any."),
                    ..Op::new("site-sessions", "List Site Sessions", "listSiteSessions", "SiteSessionListResponse")
                },
            )],
        ),
        (
            "/api/v2/credentials/{site}",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![parameter(
                        "site",
                        ParameterIn::Path,
                        true,
                        typed(Type::String, "Site")
                            .min_length(Some(1))
                            .max_length(Some(96))
                            .pattern(Some(SITE_PATTERN))
                            .into(),
                    )],
                    ..Op::new("credentials", "Get Site Credential", "getSiteCredential", "SiteCredential")
                },
            )],
        ),
        (
            "/healthz",
            vec![(HttpMethod::Get, Op::new("system", "Get Health", "getHealth", "HealthResponse"))],
        ),
    ]
}

/// The document.
pub fn document() -> OpenApi {
    let mut paths = PathsBuilder::new();
    for (path, operations) in self::paths() {
        let mut item: Option<PathItem> = None;
        for (method, op) in operations {
            let single = PathItem::new(method, op.build());
            match item.as_mut() {
                Some(existing) => existing.merge_operations(single),
                None => item = Some(single),
            }
        }
        if let Some(item) = item {
            paths = paths.path(path, item);
        }
    }
    let mut components = ComponentsBuilder::new();
    for (name, schema) in schemas() {
        components = components.schema(name, schema);
    }
    OpenApiBuilder::new()
        .info(
            InfoBuilder::new()
                .title(API_TITLE)
                .description(Some(API_DESCRIPTION))
                .version(APP_VERSION)
                .build(),
        )
        .paths(paths.build())
        .components(Some(components.build()))
        .build()
}

/// The document as a JSON value (what `/openapi.json` answers).
pub fn document_value() -> Value {
    Value::from_serde(&document_json())
}

/// The document as JSON, with each path's operations in registration order: utoipa
/// writes them in a fixed order (`get` before `post`), FastAPI in the order the routes
/// were added (`POST /api/v2/job-requests` comes before its `GET`).
pub fn document_json() -> serde_json::Value {
    let mut value = serde_json::to_value(document()).unwrap_or(serde_json::Value::Null);
    if let Some(served) = value.get_mut("paths").and_then(serde_json::Value::as_object_mut) {
        for (path, operations) in paths() {
            let Some(serde_json::Value::Object(item)) = served.get_mut(path) else {
                continue;
            };
            let mut ordered = serde_json::Map::new();
            for (method, _) in &operations {
                let name = method_name(method);
                if let Some(operation) = item.remove(name) {
                    ordered.insert(name.to_owned(), operation);
                }
            }
            ordered.append(item);
            *item = ordered;
        }
    }
    value
}

fn method_name(method: &HttpMethod) -> &'static str {
    match method {
        HttpMethod::Get => "get",
        HttpMethod::Post => "post",
        HttpMethod::Put => "put",
        HttpMethod::Delete => "delete",
        HttpMethod::Options => "options",
        HttpMethod::Head => "head",
        HttpMethod::Patch => "patch",
        HttpMethod::Trace => "trace",
    }
}

/// Schema names referenced anywhere under `value`.
pub fn referenced_schemas(value: &serde_json::Value, found: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                if key == "$ref"
                    && let Some(name) = item.as_str().and_then(|text| text.strip_prefix("#/components/schemas/"))
                {
                    found.insert(name.to_owned());
                }
                referenced_schemas(item, found);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                referenced_schemas(item, found);
            }
        }
        _ => {}
    }
}
