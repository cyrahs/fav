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
use crate::hanime1::NUMERIC_ID_PATTERN;
use crate::live2d_overrides::{MODEL_ID_PATTERN, PROFILE_PATTERN};

pub const API_TITLE: &str = "fav API";
pub const API_DESCRIPTION: &str = "Internal control and Hanime1 scan-target creation API for fav.";
pub const APP_VERSION: &str = "2.0.0";

/// Routes Python has and this build does not yet, with the work package that ports each.
/// Every path of Python's document that is missing here must be listed.
pub const PENDING: &[(&str, &str)] = &[
    ("/api/v2/services/tikhub/test", "WP13"),
    ("/api/v2/azurlane/proxy/test", "WP11"),
    ("/api/v2/rednote/proxy/test", "WP17"),
    ("/api/v2/azurlane/characters", "WP12"),
    ("/api/v2/azurlane/sidebar/characters", "WP12"),
    ("/api/v2/azurlane/skin-updates", "WP12"),
    ("/api/v2/azurlane/characters/{character_key}", "WP12"),
    ("/api/v2/azurlane/characters/{character_key}/ship-detail", "WP12"),
    ("/api/v2/site-sessions/{site}/login", "WP16"),
    ("/api/v2/site-sessions/login", "WP16"),
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

/// A create body with one non-empty string field (`Hanime1SeedCreate`).
fn create_model(name: &str, field: &'static str, title: &str) -> RefOr<Schema> {
    model(
        name,
        None,
        true,
        vec![(field, typed(Type::String, title).min_length(Some(1)).into(), true)],
    )
}

/// The components, alphabetically (by code point), as FastAPI lists them.
fn schemas() -> Vec<(&'static str, RefOr<Schema>)> {
    let mut schemas = [
        schemas_a(),
        schemas_bd2(),
        schemas_live2d(),
        schemas_hanime1(),
        schemas_b(),
        schemas_c(),
        schemas_c2(),
        schemas_d(),
        crate::nikke::schema::components(),
    ]
    .concat();
    schemas.sort_by_key(|(name, _)| *name);
    schemas
}

/// `T | None` of a referenced model: `anyOf` of the reference and null, without a title.
fn nullable_ref(name: &str) -> RefOr<Schema> {
    AnyOfBuilder::new()
        .item(Ref::from_schema_name(name))
        .item(ObjectBuilder::new().schema_type(Type::Null))
        .into()
}

fn defaulted_string(title: &str) -> RefOr<Schema> {
    with_default(typed(Type::String, title), json!(""))
}

fn free_object_list(title: &str) -> RefOr<Schema> {
    array_of(
        ObjectBuilder::new()
            .schema_type(Type::Object)
            .additional_properties(Some(AdditionalProperties::FreeForm(true)))
            .into(),
        title,
    )
}

/// `int | str | None`.
fn int_or_str(title: &str) -> RefOr<Schema> {
    AnyOfBuilder::new()
        .item(ObjectBuilder::new().schema_type(Type::Integer))
        .item(ObjectBuilder::new().schema_type(Type::String))
        .item(ObjectBuilder::new().schema_type(Type::Null))
        .extensions(Some(extensions(&[("title", json!(title))])))
        .into()
}

/// `dict[str, T]` of a simple type.
fn typed_map(kind: Type, title: &str) -> RefOr<Schema> {
    typed(Type::Object, title)
        .additional_properties(Some(AdditionalProperties::RefOr(ObjectBuilder::new().schema_type(kind).into())))
        .into()
}

fn bd2_summary_fields() -> Vec<Field> {
    vec![
        ("content_id", integer("Content Id"), true),
        ("title", string("Title"), true),
        ("directory_name", defaulted_string("Directory Name"), false),
        ("source_url", defaulted_string("Source Url"), false),
        ("updated_at", int_or_str("Updated At"), false),
        ("fetched_at", nullable(Type::String, "Fetched At"), false),
        ("asset_counts", typed_map(Type::Integer, "Asset Counts"), false),
        ("tags", typed_map(Type::String, "Tags"), false),
        ("profile", array_of(reference("BD2ProfileItem"), "Profile"), false),
        ("icon", nullable_ref("BD2Asset"), false),
        ("portrait", nullable_ref("BD2Asset"), false),
        (
            "costume_count",
            with_default(typed(Type::Integer, "Costume Count"), json!(0)),
            false,
        ),
        (
            "live2d_model_count",
            with_default(typed(Type::Integer, "Live2D Model Count"), json!(0)),
            false,
        ),
        ("search_terms", string_list("Search Terms"), false),
    ]
}

/// The BD2 models (`schemas.py`).
#[allow(clippy::too_many_lines)] // One entry per model, as the other lists.
fn schemas_bd2() -> Vec<(&'static str, RefOr<Schema>)> {
    let mut detail = bd2_summary_fields();
    detail.extend([
        (
            "tree_row",
            AnyOfBuilder::new()
                .item(
                    ObjectBuilder::new()
                        .schema_type(Type::Object)
                        .additional_properties(Some(AdditionalProperties::FreeForm(true))),
                )
                .item(ObjectBuilder::new().schema_type(Type::Null))
                .extensions(Some(extensions(&[("title", json!("Tree Row"))])))
                .into(),
            false,
        ),
        ("base_info", free_object("Base Info"), false),
        ("costumes", array_of(reference("BD2Costume"), "Costumes"), false),
        ("live2d_models", array_of(reference("BD2Live2DModel"), "Live2D Models"), false),
        ("assets", array_of(reference("BD2Asset"), "Assets"), false),
    ]);
    let mut live2d_model: Vec<Field> = vec![
        ("model_id", defaulted_string("Model Id"), false),
        ("label", defaulted_string("Label"), false),
        ("section", defaulted_string("Section"), false),
        ("style_index", nullable(Type::Integer, "Style Index"), false),
        ("style_name", defaulted_string("Style Name"), false),
        ("costume_title", defaulted_string("Costume Title"), false),
        ("costume_category", defaulted_string("Costume Category"), false),
        ("row_index", nullable(Type::Integer, "Row Index"), false),
        ("column_index", nullable(Type::Integer, "Column Index"), false),
        ("field", defaulted_string("Field"), false),
        ("is_art_row", with_default(typed(Type::Boolean, "Is Art Row"), json!(false)), false),
    ];
    for (name, title) in [
        ("column_name", "Column Name"),
        ("column_category", "Column Category"),
        ("column_role", "Column Role"),
        ("column_header", "Column Header"),
        ("key", "Key"),
        ("stable_id", "Stable Id"),
        ("live2d_key", "Live2D Key"),
        ("animation", "Animation"),
        ("skin", "Skin"),
    ] {
        live2d_model.push((name, defaulted_string(title), false));
    }
    live2d_model.push(("limit_age", with_default(typed(Type::Boolean, "Limit Age"), json!(false)), false));
    for (name, title) in [
        ("source", "Source"),
        ("variant", "Variant"),
        ("supplement_reason", "Supplement Reason"),
        ("viewer_entry_id", "Viewer Entry Id"),
        ("viewer_stem", "Viewer Stem"),
        ("source_page_url", "Source Page Url"),
    ] {
        live2d_model.push((name, defaulted_string(title), false));
    }
    live2d_model.extend([
        ("position", free_object("Position"), false),
        ("bg_position", free_object("Bg Position"), false),
        (
            "view_overrides",
            typed(Type::Object, "View Overrides")
                .additional_properties(Some(AdditionalProperties::RefOr(reference("Live2DViewOverrideValue"))))
                .into(),
            false,
        ),
        ("source_urls", free_object("Source Urls"), false),
        ("spine_version", nullable(Type::String, "Spine Version"), false),
        ("assets", free_object("Assets"), false),
    ]);
    let mut asset: Vec<Field> = vec![
        ("kind", string("Kind"), true),
        ("path", string("Path"), true),
        ("url", string("Url"), true),
        ("content_type", defaulted_string("Content Type"), false),
        ("size", with_default(typed(Type::Integer, "Size"), json!(0)), false),
    ];
    for (name, title) in [("sha256", "Sha256"), ("status", "Status"), ("label", "Label"), ("field", "Field")] {
        asset.push((name, defaulted_string(title), false));
    }
    asset.push(("style_index", nullable(Type::Integer, "Style Index"), false));
    for (name, title) in [
        ("style_name", "Style Name"),
        ("costume_title", "Costume Title"),
        ("costume_category", "Costume Category"),
        ("column_role", "Column Role"),
        ("live2d_key", "Live2D Key"),
    ] {
        asset.push((name, defaulted_string(title), false));
    }
    asset.push(("available", with_default(typed(Type::Boolean, "Available"), json!(false)), false));
    asset.push(("contexts", free_object_list("Contexts"), false));
    vec![
        ("BD2Asset", model("BD2Asset", None, true, asset)),
        ("BD2CharacterDetail", model("BD2CharacterDetail", None, true, detail)),
        (
            "BD2CharacterListResponse",
            model(
                "BD2CharacterListResponse",
                None,
                true,
                vec![
                    ("items", array_of(reference("BD2CharacterSummary"), "Items"), true),
                    ("total", integer("Total"), true),
                    ("limit", nullable(Type::Integer, "Limit"), false),
                    ("offset", with_default(typed(Type::Integer, "Offset"), json!(0)), false),
                ],
            ),
        ),
        (
            "BD2CharacterSummary",
            model("BD2CharacterSummary", None, true, bd2_summary_fields()),
        ),
        (
            "BD2Costume",
            model(
                "BD2Costume",
                None,
                true,
                vec![
                    ("style_index", nullable(Type::Integer, "Style Index"), false),
                    ("style_name", defaulted_string("Style Name"), false),
                    ("title", defaulted_string("Title"), false),
                    ("category", defaulted_string("Category"), false),
                    ("sprite", nullable_ref("BD2Asset"), false),
                    ("portrait", nullable_ref("BD2Asset"), false),
                    ("full_portrait", nullable_ref("BD2Asset"), false),
                    ("gallery", array_of(reference("BD2Asset"), "Gallery"), false),
                    ("videos", array_of(reference("BD2Asset"), "Videos"), false),
                    ("audio", array_of(reference("BD2Asset"), "Audio"), false),
                    ("live2d_models", array_of(reference("BD2Live2DModel"), "Live2D Models"), false),
                    ("rows", free_object_list("Rows"), false),
                ],
            ),
        ),
        ("BD2Live2DModel", model("BD2Live2DModel", None, true, live2d_model)),
        (
            "BD2ProfileItem",
            model(
                "BD2ProfileItem",
                None,
                true,
                vec![
                    ("key", string("Key"), true),
                    ("label", string("Label"), true),
                    ("value", defaulted_string("Value"), false),
                    ("asset", nullable_ref("BD2Asset"), false),
                ],
            ),
        ),
        (
            "BD2SidebarCharacter",
            model(
                "BD2SidebarCharacter",
                None,
                true,
                vec![
                    ("content_id", integer("Content Id"), true),
                    ("title", string("Title"), true),
                    ("icon", reference("BD2SidebarIcon"), true),
                    ("updated_at", int_or_str("Updated At"), false),
                    ("fetched_at", nullable(Type::String, "Fetched At"), false),
                ],
            ),
        ),
        (
            "BD2SidebarCharacterListResponse",
            list_model("BD2SidebarCharacterListResponse", "BD2SidebarCharacter"),
        ),
        (
            "BD2SidebarIcon",
            model(
                "BD2SidebarIcon",
                None,
                true,
                vec![
                    ("url", string("Url"), true),
                    ("available", boolean("Available"), true),
                    ("sha256", string("Sha256"), true),
                    ("content_type", string("Content Type"), true),
                ],
            ),
        ),
    ]
}

/// A `float` field with `gt=0`; pydantic writes the bound as the float `0.0`.
fn positive_number() -> ObjectBuilder {
    ObjectBuilder::new()
        .schema_type(Type::Number)
        .extensions(Some(extensions(&[("exclusiveMinimum", json!(0.0))])))
}

fn override_value_fields() -> Vec<Field> {
    vec![
        ("position", reference("Live2DVector2"), true),
        ("scale", typed(Type::Number, "Scale").into(), true),
        ("background_position", nullable_ref("Live2DVector2"), false),
        ("background_scale", nullable(Type::Number, "Background Scale"), false),
        ("created_at", string("Created At"), true),
        ("updated_at", string("Updated At"), true),
    ]
}

/// The Live2D view-override models.
fn schemas_live2d() -> Vec<(&'static str, RefOr<Schema>)> {
    let mut full = override_value_fields();
    full.extend([
        (
            "source",
            typed(Type::String, "Source").enum_values(Some(["bd2", "nikke"])).into(),
            true,
        ),
        ("content_id", integer("Content Id"), true),
        ("model_id", string("Model Id"), true),
        ("profile", string("Profile"), true),
    ]);
    vec![
        (
            "Live2DVector2",
            model(
                "Live2DVector2",
                None,
                true,
                vec![
                    ("x", typed(Type::Number, "X").into(), true),
                    ("y", typed(Type::Number, "Y").into(), true),
                ],
            ),
        ),
        ("Live2DViewOverride", model("Live2DViewOverride", None, true, full)),
        (
            "Live2DViewOverrideUpsert",
            model(
                "Live2DViewOverrideUpsert",
                None,
                true,
                vec![
                    ("position", reference("Live2DVector2"), true),
                    ("scale", positive_number().title(Some("Scale")).into(), true),
                    ("background_position", nullable_ref("Live2DVector2"), false),
                    (
                        "background_scale",
                        AnyOfBuilder::new()
                            .item(positive_number())
                            .item(ObjectBuilder::new().schema_type(Type::Null))
                            .extensions(Some(extensions(&[("title", json!("Background Scale"))])))
                            .into(),
                        false,
                    ),
                ],
            ),
        ),
        (
            "Live2DViewOverrideValue",
            model("Live2DViewOverrideValue", None, true, override_value_fields()),
        ),
    ]
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
            "ComponentReadiness",
            model(
                "ComponentReadiness",
                None,
                true,
                vec![
                    (
                        "status",
                        typed(Type::String, "Status")
                            .enum_values(Some(["ok", "degraded", "skipped"]))
                            .into(),
                        true,
                    ),
                    ("code", string("Code"), true),
                    ("message", string("Message"), true),
                    // pydantic writes `ge=0` as the float `0.0`, which utoipa's `minimum` would
                    // turn into an integer.
                    (
                        "sampled_targets",
                        typed(Type::Integer, "Sampled Targets")
                            .extensions(Some(extensions(&[("minimum", json!(0.0))])))
                            .into(),
                        true,
                    ),
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
            "KemonoCreatorResolveRequest",
            model(
                "KemonoCreatorResolveRequest",
                None,
                true,
                vec![("creator", typed(Type::String, "Creator").min_length(Some(1)).into(), true)],
            ),
        ),
        (
            "KemonoCreatorResolved",
            model(
                "KemonoCreatorResolved",
                None,
                true,
                vec![
                    ("service", string("Service"), true),
                    ("id", string("Id"), true),
                    ("name", string("Name"), true),
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
    ]
}

/// `HTTPValidationError` and the Hanime1 models, which sort between it and `HealthResponse`.
#[allow(clippy::too_many_lines)] // One entry per model of `schemas.py`, as the other lists.
fn schemas_hanime1() -> Vec<(&'static str, RefOr<Schema>)> {
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
            "Hanime1Author",
            model(
                "Hanime1Author",
                None,
                true,
                vec![
                    ("author_id", string("Author Id"), true),
                    ("name", string("Name"), true),
                    ("author_url", string("Author Url"), true),
                ],
            ),
        ),
        ("Hanime1AuthorCreate", create_model("Hanime1AuthorCreate", "author", "Author")),
        (
            "Hanime1AuthorDetail",
            model(
                "Hanime1AuthorDetail",
                None,
                true,
                vec![
                    ("author_id", string("Author Id"), true),
                    ("name", string("Name"), true),
                    ("author_url", string("Author Url"), true),
                    ("video_count", integer("Video Count"), true),
                    ("created_at", nullable(Type::String, "Created At"), false),
                    ("updated_at", nullable(Type::String, "Updated At"), false),
                    ("last_scanned_at", nullable(Type::String, "Last Scanned At"), false),
                    (
                        "last_scan_error",
                        with_default(typed(Type::String, "Last Scan Error"), json!("")),
                        false,
                    ),
                ],
            ),
        ),
        (
            "Hanime1AuthorListResponse",
            list_model("Hanime1AuthorListResponse", "Hanime1AuthorDetail"),
        ),
        ("Hanime1ListResponse", list_model("Hanime1ListResponse", "Hanime1Video")),
        (
            "Hanime1Seed",
            model(
                "Hanime1Seed",
                None,
                true,
                vec![
                    ("video_id", string("Video Id"), true),
                    ("title", string("Title"), true),
                    ("label", string("Label"), true),
                ],
            ),
        ),
        ("Hanime1SeedCreate", create_model("Hanime1SeedCreate", "seed", "Seed")),
        (
            "Hanime1SeedDetail",
            model(
                "Hanime1SeedDetail",
                None,
                true,
                vec![
                    ("video_id", string("Video Id"), true),
                    ("title", string("Title"), true),
                    ("label", string("Label"), true),
                    ("added_from_video_id", string("Added From Video Id"), true),
                    ("video_count", integer("Video Count"), true),
                    ("created_at", nullable(Type::String, "Created At"), false),
                    ("updated_at", nullable(Type::String, "Updated At"), false),
                    ("last_scanned_at", nullable(Type::String, "Last Scanned At"), false),
                    (
                        "last_scan_error",
                        with_default(typed(Type::String, "Last Scan Error"), json!("")),
                        false,
                    ),
                    ("watch_url", string("Watch Url"), true),
                ],
            ),
        ),
        (
            "Hanime1SeedListResponse",
            list_model("Hanime1SeedListResponse", "Hanime1SeedDetail"),
        ),
        (
            "Hanime1Video",
            model(
                "Hanime1Video",
                None,
                true,
                vec![
                    ("video_id", string("Video Id"), true),
                    ("title", string("Title"), true),
                    ("downloaded", boolean("Downloaded"), true),
                    ("uploader", nullable(Type::String, "Uploader"), false),
                    ("release_date", nullable(Type::String, "Release Date"), false),
                    ("plot", nullable(Type::String, "Plot"), false),
                    ("watch_url", string("Watch Url"), true),
                ],
            ),
        ),
    ]
}

fn schemas_c2() -> Vec<(&'static str, RefOr<Schema>)> {
    vec![
        (
            "ReadinessResponse",
            model(
                "ReadinessResponse",
                None,
                true,
                vec![
                    (
                        "status",
                        typed(Type::String, "Status").enum_values(Some(["ok", "degraded"])).into(),
                        true,
                    ),
                    ("generated_at", string("Generated At"), true),
                    (
                        "checks",
                        typed(Type::Object, "Checks")
                            .additional_properties(Some(AdditionalProperties::RefOr(reference("ComponentReadiness"))))
                            .into(),
                        true,
                    ),
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
    /// The response model; empty for a response without a body (a 204).
    response: &'static str,
    /// Further responses with the same model (`responses={503: ...}`): status, description.
    extra_responses: Vec<(&'static str, &'static str)>,
    /// Further responses without a body (`responses={304: {'description': ...}}`).
    bare_responses: Vec<(&'static str, &'static str)>,
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
            extra_responses: Vec::new(),
            bare_responses: Vec::new(),
            parameters: Vec::new(),
            body: None,
        }
    }

    fn build(self) -> utoipa::openapi::path::Operation {
        let json_content = |schema: RefOr<Schema>| ContentBuilder::new().schema(Some(schema)).build();
        let validates = !self.parameters.is_empty() || self.body.is_some();
        let success = ResponseBuilder::new().description("Successful Response");
        let success = if self.response.is_empty() {
            success
        } else {
            success.content("application/json", json_content(reference(self.response)))
        };
        let mut builder = OperationBuilder::new()
            .tag(self.tag)
            .summary(Some(self.summary))
            .description(self.description)
            .operation_id(Some(self.id))
            .response(self.status, success);
        for (status, description) in self.extra_responses {
            builder = builder.response(
                status,
                ResponseBuilder::new()
                    .description(description)
                    .content("application/json", json_content(reference(self.response))),
            );
        }
        for (status, description) in self.bare_responses {
            builder = builder.response(status, ResponseBuilder::new().description(description));
        }
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
    paths.extend(paths_hanime1());
    paths.extend(paths_kemono());
    paths.extend(paths_bd2());
    paths.extend(paths_nikke());
    paths.extend(paths_c());
    paths
}

fn search_parameter() -> utoipa::openapi::path::Parameter {
    parameter(
        "q",
        ParameterIn::Query,
        false,
        AnyOfBuilder::new()
            .item(ObjectBuilder::new().schema_type(Type::String).min_length(Some(1)))
            .item(ObjectBuilder::new().schema_type(Type::Null))
            .extensions(Some(extensions(&[("title", json!("Q"))])))
            .into(),
    )
}

fn content_id_parameter() -> utoipa::openapi::path::Parameter {
    parameter(
        "content_id",
        ParameterIn::Path,
        true,
        typed(Type::Integer, "Content Id").exclusive_minimum(Some(0)).into(),
    )
}

fn override_parameters() -> Vec<utoipa::openapi::path::Parameter> {
    vec![
        content_id_parameter(),
        parameter(
            "model_id",
            ParameterIn::Path,
            true,
            typed(Type::String, "Model Id")
                .min_length(Some(1))
                .max_length(Some(160))
                .pattern(Some(MODEL_ID_PATTERN))
                .into(),
        ),
        parameter(
            "profile",
            ParameterIn::Path,
            true,
            typed(Type::String, "Profile")
                .min_length(Some(1))
                .max_length(Some(64))
                .pattern(Some(PROFILE_PATTERN))
                .into(),
        ),
    ]
}

/// The BD2 paths.
fn paths_bd2() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
        (
            "/api/v2/bd2/characters",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![
                        search_parameter(),
                        parameter("limit", ParameterIn::Query, false, bounded_int("Limit", Some(1), Some(500), 200)),
                        parameter("offset", ParameterIn::Query, false, bounded_int("Offset", Some(0), None, 0)),
                    ],
                    ..Op::new("bd2", "List Bd2 Characters", "listBD2Characters", "BD2CharacterListResponse")
                },
            )],
        ),
        (
            "/api/v2/bd2/sidebar/characters",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![search_parameter()],
                    bare_responses: vec![("304", "Not modified")],
                    ..Op::new(
                        "bd2",
                        "List Bd2 Sidebar Characters",
                        "listBD2SidebarCharacters",
                        "BD2SidebarCharacterListResponse",
                    )
                },
            )],
        ),
        (
            "/api/v2/bd2/characters/{content_id}",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![content_id_parameter()],
                    ..Op::new("bd2", "Get Bd2 Character", "getBD2Character", "BD2CharacterDetail")
                },
            )],
        ),
        (
            "/api/v2/bd2/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}",
            override_ops(
                "bd2",
                [
                    ("Get Bd2 Live2D View Override", "getBD2Live2DViewOverride"),
                    ("Put Bd2 Live2D View Override", "putBD2Live2DViewOverride"),
                    ("Delete Bd2 Live2D View Override", "deleteBD2Live2DViewOverride"),
                ],
            ),
        ),
    ]
}

/// The get, put and delete of a game's view-override path: `(summary, operationId)` each.
fn override_ops(tag: &'static str, names: [(&'static str, &'static str); 3]) -> Vec<(HttpMethod, Op)> {
    let [(get, get_id), (put, put_id), (delete, delete_id)] = names;
    vec![
        (
            HttpMethod::Get,
            Op {
                parameters: override_parameters(),
                ..Op::new(tag, get, get_id, "Live2DViewOverride")
            },
        ),
        (
            HttpMethod::Put,
            Op {
                parameters: override_parameters(),
                body: Some(reference("Live2DViewOverrideUpsert")),
                ..Op::new(tag, put, put_id, "Live2DViewOverride")
            },
        ),
        (
            HttpMethod::Delete,
            Op {
                status: "204",
                parameters: override_parameters(),
                ..Op::new(tag, delete, delete_id, "")
            },
        ),
    ]
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
    ]
}

fn numeric_id_parameter(name: &str, title: &str) -> utoipa::openapi::path::Parameter {
    parameter(
        name,
        ParameterIn::Path,
        true,
        typed(Type::String, title)
            .min_length(Some(1))
            .max_length(Some(32))
            .pattern(Some(NUMERIC_ID_PATTERN))
            .into(),
    )
}

/// The Hanime1 paths. Python's document lists `get` before `post` on the seed and author
/// paths although the `POST` routes are registered first, so they are written that way.
fn paths_hanime1() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
        (
            "/api/v2/hanime1/videos",
            vec![(
                HttpMethod::Get,
                Op::new("hanime1", "List Hanime1 Videos", "listHanime1Videos", "Hanime1ListResponse"),
            )],
        ),
        (
            "/api/v2/hanime1/seeds",
            vec![
                (
                    HttpMethod::Get,
                    Op::new("hanime1", "List Hanime1 Seeds", "listHanime1Seeds", "Hanime1SeedListResponse"),
                ),
                (
                    HttpMethod::Post,
                    Op {
                        status: "201",
                        body: Some(reference("Hanime1SeedCreate")),
                        ..Op::new("hanime1", "Create Hanime1 Seed", "createHanime1Seed", "Hanime1Seed")
                    },
                ),
            ],
        ),
        (
            "/api/v2/hanime1/seeds/{canonical_video_id}",
            vec![(
                HttpMethod::Delete,
                Op {
                    status: "204",
                    parameters: vec![numeric_id_parameter("canonical_video_id", "Canonical Video Id")],
                    ..Op::new("hanime1", "Delete Hanime1 Seed", "deleteHanime1Seed", "")
                },
            )],
        ),
        (
            "/api/v2/hanime1/authors",
            vec![
                (
                    HttpMethod::Get,
                    Op::new("hanime1", "List Hanime1 Authors", "listHanime1Authors", "Hanime1AuthorListResponse"),
                ),
                (
                    HttpMethod::Post,
                    Op {
                        status: "201",
                        body: Some(reference("Hanime1AuthorCreate")),
                        ..Op::new("hanime1", "Create Hanime1 Author", "createHanime1Author", "Hanime1Author")
                    },
                ),
            ],
        ),
        (
            "/api/v2/hanime1/authors/{author_id}",
            vec![(
                HttpMethod::Delete,
                Op {
                    status: "204",
                    parameters: vec![numeric_id_parameter("author_id", "Author Id")],
                    ..Op::new("hanime1", "Delete Hanime1 Author", "deleteHanime1Author", "")
                },
            )],
        ),
    ]
}

fn paths_kemono() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![(
        "/api/v2/kemono/creators/resolve",
        vec![(
            HttpMethod::Post,
            Op {
                body: Some(reference("KemonoCreatorResolveRequest")),
                ..Op::new("kemono", "Resolve Kemono Creator", "resolveKemonoCreator", "KemonoCreatorResolved")
            },
        )],
    )]
}

/// The `q` parameter of the Nikke lists (`NikkeSearchQuery`).
fn nikke_query_parameter() -> utoipa::openapi::path::Parameter {
    parameter(
        "q",
        ParameterIn::Query,
        false,
        AnyOfBuilder::new()
            .item(ObjectBuilder::new().schema_type(Type::String).min_length(Some(1)))
            .item(ObjectBuilder::new().schema_type(Type::Null))
            .extensions(Some(extensions(&[("title", json!("Q"))])))
            .into(),
    )
}

fn paths_nikke() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
        (
            "/api/v2/nikke/characters",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![
                        nikke_query_parameter(),
                        parameter("limit", ParameterIn::Query, false, bounded_int("Limit", Some(1), Some(500), 200)),
                        parameter("offset", ParameterIn::Query, false, bounded_int("Offset", Some(0), None, 0)),
                    ],
                    ..Op::new(
                        "nikke",
                        "List Nikke Characters",
                        "listNikkeCharacters",
                        "NikkeCharacterListResponse",
                    )
                },
            )],
        ),
        (
            "/api/v2/nikke/sidebar/characters",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![nikke_query_parameter()],
                    bare_responses: vec![("304", "Not modified")],
                    ..Op::new(
                        "nikke",
                        "List Nikke Sidebar Characters",
                        "listNikkeSidebarCharacters",
                        "NikkeSidebarCharacterListResponse",
                    )
                },
            )],
        ),
        (
            "/api/v2/nikke/characters/{content_id}",
            vec![(
                HttpMethod::Get,
                Op {
                    parameters: vec![parameter(
                        "content_id",
                        ParameterIn::Path,
                        true,
                        typed(Type::Integer, "Content Id").exclusive_minimum(Some(0)).into(),
                    )],
                    ..Op::new("nikke", "Get Nikke Character", "getNikkeCharacter", "NikkeCharacterDetail")
                },
            )],
        ),
        (
            "/api/v2/nikke/characters/{content_id}/live2d-models/{model_id}/view-overrides/{profile}",
            override_ops(
                "nikke",
                [
                    ("Get Nikke Live2D View Override", "getNikkeLive2DViewOverride"),
                    ("Put Nikke Live2D View Override", "putNikkeLive2DViewOverride"),
                    ("Delete Nikke Live2D View Override", "deleteNikkeLive2DViewOverride"),
                ],
            ),
        ),
    ]
}

fn paths_c() -> Vec<(&'static str, Vec<(HttpMethod, Op)>)> {
    vec![
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
        (
            "/readyz",
            vec![(
                HttpMethod::Get,
                Op {
                    extra_responses: vec![("503", "Service Unavailable")],
                    ..Op::new("system", "Get Readiness", "getReadiness", "ReadinessResponse")
                },
            )],
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
