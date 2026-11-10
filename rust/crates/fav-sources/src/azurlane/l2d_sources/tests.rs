//! Ports of `tests/test_azurlane_l2d_sources.py` and `tests/test_azurlane_hashed_build.py`.
//! The fixtures mirror the Python helpers (`_skin`, `_ship`, `_source_snapshots`, ...), and
//! [`FakeFetch`] stands in for `httpx.MockTransport`.

use std::sync::Mutex;

use fav_core::pyjson::{DumpOptions, Value, dumps};
use fav_net::{ErrorClass, TransportError};
use fav_runtime::BoxFuture;

use super::{
    AzurLaneModelCatalog, AzurLaneSourceSnapshots, Fetch, FetchedResponse, L2D_SU_PRIMARY_REGION, L2D_SU_STATIC_BASE_URL,
    L2DSuCharacterSnapshot, L2DSuModelSnapshot, L2DSuSourceSnapshot, ModelEntry, ModelKind, NAGAMI_MAPPING_URL, NagamiSourceSnapshot,
    SourceFetchMetadata, l2d_su_ship_index_url, parse_nagami_mapping,
};

/// A JSON value from a Rust literal, for the `obj!` / `list!` fixtures.
pub(super) trait J {
    fn j(self) -> Value;
}

impl J for Value {
    fn j(self) -> Value {
        self
    }
}

impl J for &str {
    fn j(self) -> Value {
        Value::Str(self.to_owned())
    }
}

impl J for String {
    fn j(self) -> Value {
        Value::Str(self)
    }
}

impl J for bool {
    fn j(self) -> Value {
        Value::Bool(self)
    }
}

impl J for i32 {
    fn j(self) -> Value {
        Value::Int(i128::from(self))
    }
}

impl J for i128 {
    fn j(self) -> Value {
        Value::Int(self)
    }
}

impl J for f64 {
    fn j(self) -> Value {
        Value::Float(self)
    }
}

impl<T: J> J for Vec<T> {
    fn j(self) -> Value {
        Value::List(self.into_iter().map(J::j).collect())
    }
}

/// An insertion-ordered dict literal: `obj! { "key" => value, ... }`.
macro_rules! obj {
    ($($key:expr => $value:expr),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut dict = fav_core::pyjson::Dict::new();
        $(dict.insert($key, $crate::azurlane::l2d_sources::tests::J::j($value));)*
        fav_core::pyjson::Value::Dict(dict)
    }};
}

/// A list literal: `list![value, ...]`.
macro_rules! list {
    ($($value:expr),* $(,)?) => {
        fav_core::pyjson::Value::List(vec![$($crate::azurlane::l2d_sources::tests::J::j($value)),*])
    };
}

mod catalog;
mod enumerate;
mod fetch;
pub(crate) mod golden;
mod hashed;
mod health;
mod index;
mod painting;
mod validate;

pub(super) const FETCHED_AT: &str = "2026-11-10T00:00:00+00:00";

/// The [`super::Clock`] of every test.
pub(super) fn clock() -> String {
    FETCHED_AT.to_owned()
}

/// `set(payload)`: the keys of a rendered dict.
pub(super) fn keys(value: &Value) -> std::collections::BTreeSet<String> {
    let Value::Dict(dict) = value else { panic!("not a dict") };
    dict.iter()
        .map(|(key, _)| match key {
            fav_core::pyjson::Key::Str(text) => text.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

/// `json.dumps(value, ensure_ascii=False)`.
pub(super) fn json(value: &Value) -> String {
    dumps(value, &DumpOptions::plain()).expect("a fixture serialises")
}

fn insert(value: &mut Value, key: &str, item: Value) {
    let Value::Dict(dict) = value else { panic!("not a dict") };
    dict.insert(key, item);
}

/// `value[key] = item` on a fixture dict.
pub(super) fn set(mut value: Value, key: &str, item: impl J) -> Value {
    insert(&mut value, key, item.j());
    value
}

pub(super) fn primary_index_url() -> String {
    l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
}

pub(super) fn english_index_url() -> String {
    l2d_su_ship_index_url(super::L2D_SU_ENGLISH_REGION)
}

/// `_skin(skin_id, name, key=key, kind=kind)`.
pub(super) fn skin(skin_id: i32, name: &str, key: &str, kind: &str) -> Value {
    let feature_tag = if kind == "spine" { "spine" } else { "Live2D" };
    obj! {
        "id" => skin_id,
        "name" => name,
        "groupIndex" => 1,
        "skinType" => 4,
        "skinTypeName" => "Skin",
        "featureTags" => vec![feature_tag],
        "painting" => key,
        "prefab" => key,
        "dynamicType" => kind,
        "isLive2d" => kind == "live2d",
        "isLive2dPlus" => false,
        "isDynamic" => true,
        "isSpine" => kind == "spine",
    }
}

/// `_static_skin`.
pub(super) fn static_skin(skin_id: i32, name: &str, key: &str) -> Value {
    obj! {
        "id" => skin_id,
        "name" => name,
        "skinTypeName" => "Default",
        "featureTags" => Vec::<Value>::new(),
        "painting" => key,
        "prefab" => key,
        "dynamicType" => "painting",
        "isLive2d" => false,
        "isLive2dPlus" => false,
        "isDynamic" => false,
        "isSpine" => false,
        "paintingFaceIds" => vec!["1", "2"],
        "assetPaths" => obj! {
            "squareIcon" => format!("squareicon/{key}"),
            "shipyardIcon" => format!("shipyardicon/{key}"),
            "qIcon" => format!("qicon/{key}"),
            "painting" => format!("painting/{key}"),
        },
    }
}

/// `_ship(ship_group_id, resource_key, name, english_name=..., skins=...)`.
pub(super) fn ship(ship_group_id: i32, resource_key: &str, name: &str, english_name: &str, skins: Vec<Value>) -> Value {
    obj! {
        "shipGroupId" => ship_group_id,
        "name" => name,
        "englishName" => english_name,
        "nationName" => "Royal Navy",
        "typeName" => "Destroyer",
        "rarityName" => "SR",
        "resourceKey" => resource_key,
        "skins" => skins,
    }
}

/// `_ship_index_payload(ships, region=region, new_skins=new_skins)`.
pub(super) fn ship_index_payload(ships: Vec<Value>, region: &str, new_skins: Vec<Value>) -> String {
    json(&obj! {
        "generatedAt" => "2026-08-12T04:33:18.041Z",
        "locale" => region,
        "version" => "9.7.295",
        "ships" => ships,
        "newSkins" => new_skins,
    })
}

/// `_ship_index_payload(ships)`.
pub(super) fn index_payload(ships: Vec<Value>) -> String {
    ship_index_payload(ships, "CN", Vec::new())
}

/// `_spine_parts_payload`.
pub(super) fn spine_parts_payload(key: &str, suffixes: &[&str]) -> String {
    let models: Vec<Value> = suffixes
        .iter()
        .map(|suffix| {
            obj! {
                "name" => format!("{key}{suffix}"),
                "skeleton" => format!("{key}{suffix}.skel"),
                "atlases" => vec![format!("{key}{suffix}.atlas")],
                "animation" => "normal",
                "loop" => true,
            }
        })
        .collect();
    json(&obj! { "version" => 1, "models" => models })
}

pub(super) fn live2d_url(key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/live2d/{key}/{key}.model3.json")
}

pub(super) fn spine_url(key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/spinepainting/{key}")
}

/// `_model(kind, {...})`.
pub(super) fn model(kind: ModelKind, costume_id: i128, costume_name: &str, costume_name_en: &str, path: &str) -> L2DSuModelSnapshot {
    let mut model = L2DSuModelSnapshot::new(kind, costume_id, costume_name.to_owned(), path.to_owned());
    costume_name_en.clone_into(&mut model.costume_name_en);
    "Skin".clone_into(&mut model.skin_type);
    model
}

/// One item of `_characters([...])`.
pub(super) fn character(
    char_id: i128,
    char_key: &str,
    char_name: &str,
    char_name_en: &str,
    live2d: Vec<L2DSuModelSnapshot>,
    spine: Vec<L2DSuModelSnapshot>,
) -> L2DSuCharacterSnapshot {
    let mut character = L2DSuCharacterSnapshot::new(char_id, char_key, char_name);
    char_name_en.clone_into(&mut character.char_name_en);
    character.live2d = live2d;
    character.spine = spine;
    character
}

/// `_live2d_model3_payload(display_info=...)`.
pub(super) fn live2d_model3_payload(display_info: Option<&str>) -> String {
    let mut file_references = obj! {
        "Moc" => "test_model.moc3",
        "Textures" => vec!["textures/texture_00.webp", "textures/texture_01.webp"],
        "Physics" => "test_model.physics3.json",
        "Expressions" => list![obj! { "Name" => "smile", "File" => "expressions/smile.exp3.json" }],
        "Motions" => obj! {
            "Idle" => list![obj! { "File" => "motions/idle.motion3.json", "Sound" => "voice/idle.wav" }],
            "TapBody" => list![obj! { "File" => "motions/tap_body.motion3.json", "Text" => "tap-body" }],
        },
    };
    if let Some(display_info) = display_info {
        insert(&mut file_references, "DisplayInfo", display_info.j());
    }
    json(&obj! { "Version" => 3, "FileReferences" => file_references })
}

/// `SourceFetchMetadata.for_url(url, http_status=200)`.
pub(super) fn metadata_200(url: &str) -> SourceFetchMetadata {
    let mut metadata = SourceFetchMetadata::for_url(url, clock());
    metadata.http_status = Some(200);
    metadata
}

/// `_source_snapshots(characters, nagami_mapping)`.
pub(super) fn source_snapshots(characters: Vec<L2DSuCharacterSnapshot>, nagami_mapping: &[(&str, &str)]) -> AzurLaneSourceSnapshots {
    let mut mapping = fav_core::pyjson::Dict::new();
    for (key, name) in nagami_mapping {
        mapping.insert(*key, Value::Str((*name).to_owned()));
    }
    let nagami = parse_nagami_mapping(&json(&Value::Dict(mapping))).expect("the mapping parses");
    let mut l2d_su = L2DSuSourceSnapshot::new(metadata_200(&primary_index_url()));
    L2D_SU_PRIMARY_REGION.clone_into(&mut l2d_su.region);
    "9.7.295".clone_into(&mut l2d_su.game_version);
    l2d_su.ship_count = i128::try_from(characters.len()).expect("a small count");
    l2d_su.characters = characters;
    let mut nagami_snapshot = NagamiSourceSnapshot::new(metadata_200(NAGAMI_MAPPING_URL));
    nagami_snapshot.entries = nagami.entries;
    AzurLaneSourceSnapshots {
        l2d_su,
        nagami: nagami_snapshot,
    }
}

/// `_catalog_entry_by_costume_key`.
pub(super) fn entry_ids_with_costume_key(catalog: &AzurLaneModelCatalog, costume_key: &str) -> Vec<String> {
    catalog
        .entries
        .iter()
        .filter(|entry| entry.costume.key == costume_key)
        .map(|entry| entry.id.clone())
        .collect()
}

/// `_catalog_entry`.
pub(super) fn catalog_entry<'a>(catalog: &'a AzurLaneModelCatalog, entry_id: &str) -> &'a ModelEntry {
    catalog
        .entries
        .iter()
        .find(|entry| entry.id == entry_id)
        .unwrap_or_else(|| panic!("no catalog entry {entry_id}"))
}

/// What a [`FakeFetch`] handler answers.
pub(super) type Reply = Result<FetchedResponse, TransportError>;

/// `httpx.Response(status, text=body)`.
pub(super) fn respond(status: u16, body: &str) -> Reply {
    respond_with(status, body, &[])
}

/// `httpx.Response(status, text=body, headers=headers)`.
pub(super) fn respond_with(status: u16, body: &str, headers: &[(&str, &str)]) -> Reply {
    Ok(FetchedResponse {
        status,
        url: String::new(),
        headers: headers
            .iter()
            .map(|(name, value)| (name.to_ascii_lowercase(), (*value).to_owned()))
            .collect(),
        content: body.as_bytes().to_vec(),
    })
}

/// `raise httpx.ConnectError(message)`.
pub(super) fn connect_error(message: &str) -> Reply {
    Err(TransportError {
        class: ErrorClass::ConnectError,
        host: String::new(),
        message: message.to_owned(),
    })
}

type Handler = Box<dyn Fn(&str, &str) -> Reply + Send + Sync>;

/// `httpx.Client(transport=httpx.MockTransport(handler))`: every request goes to `handler`
/// with its method and URL, and is logged.
pub(super) struct FakeFetch {
    handler: Handler,
    requests: Mutex<Vec<(String, String)>>,
}

impl FakeFetch {
    pub(super) fn new(handler: impl Fn(&str, &str) -> Reply + Send + Sync + 'static) -> Self {
        Self {
            handler: Box::new(handler),
            requests: Mutex::new(Vec::new()),
        }
    }

    /// `(method, url)` of every request so far.
    pub(super) fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().expect("the log").clone()
    }
}

impl Fetch for FakeFetch {
    fn request<'a>(
        &'a self,
        method: &'a str,
        url: &'a str,
        _timeout: Option<std::time::Duration>,
    ) -> BoxFuture<'a, Result<FetchedResponse, TransportError>> {
        self.requests
            .lock()
            .expect("the log")
            .push((method.to_owned(), url.to_owned()));
        let reply = (self.handler)(method, url).map(|mut response| {
            if response.url.is_empty() {
                url.clone_into(&mut response.url);
            }
            response
        });
        Box::pin(async move { reply })
    }
}
