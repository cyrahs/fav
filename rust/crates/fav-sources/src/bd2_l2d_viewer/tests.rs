use std::time::Duration;

use fav_core::pyjson::{self, Dict, Value};
use fav_net::{ClientOptions, HttpClient};

use super::*;
use crate::testutil::{FakeWeb, Reply};

fn dict(text: &str) -> Dict {
    match pyjson::loads(text).expect("JSON") {
        Value::Dict(dict) => dict,
        other => panic!("expected an object, got {other:?}"),
    }
}

fn characters(text: &str) -> Vec<(String, Dict)> {
    dict(text)
        .iter()
        .map(|(key, value)| (fav_runtime::pystr::str_key(key), value.as_dict().expect("object").clone()))
        .collect()
}

fn entry<'a>(characters: &'a [(String, Dict)], id: &str) -> &'a Dict {
    &characters.iter().find(|(entry_id, _)| entry_id == id).expect("entry").1
}

// py: tests/test_bd2_l2d_viewer.py::test_parse_viewer_character_list_handles_typescript_suffix
#[test]
fn parse_viewer_character_list_handles_typescript_suffix() {
    let source = r#"
export default {
  "000101": {
    "charName": "Lathel",
    "costumeName": "Herb Tracker",
    "spine": "char000101",
    "cutscene": "cutscene_char000101",
    "dating": ""
  }
} as { [key: string]: {
  charName: string
} }
"#;
    let characters = parse_viewer_character_list(source).unwrap();
    assert_eq!(
        entry(&characters, "000101").get("spine"),
        Some(&Value::Str("char000101".to_owned()))
    );
}

// py: tests/test_bd2_l2d_viewer.py::test_parse_viewer_character_list_handles_trailing_commas
#[test]
fn parse_viewer_character_list_handles_trailing_commas() {
    let source = r#"
export default {
  "000296": {
    "charName": "Justia",
    "costumeName": "Hot Summer Dream",
    "spine": "char000296",
    "cutscene": "cutscene_char000296",
    "dating": "illust_dating7",
    "datingUsesTracks": true,
  },
}
"#;
    let characters = parse_viewer_character_list(source).unwrap();
    let justia = entry(&characters, "000296");
    assert_eq!(justia.get("datingUsesTracks"), Some(&Value::Bool(true)));
    assert_eq!(justia.get("dating"), Some(&Value::Str("illust_dating7".to_owned())));
}

// py: tests/test_bd2_l2d_viewer.py::test_viewer_resources_use_tree_paths_and_report_missing_core_files
#[test]
fn viewer_resources_use_tree_paths_and_report_missing_core_files() {
    let characters = characters(
        r#"{"000101": {"charName": "Lathel", "costumeName": "Herb Tracker", "spine": "char000101",
            "cutscene": "cutscene_char000101", "dating": "illust_dating7"}}"#,
    );
    let tree = dict(
        r#"{"tree": [
            {"type": "blob", "path": "src/assets/spines/000101/char000101.atlas"},
            {"type": "blob", "path": "src/assets/spines/000101/char000101.skel"},
            {"type": "blob", "path": "src/assets/spines/000101/char000101.png"},
            {"type": "blob", "path": "src/assets/spines/000101/cutscene/cutscene_char000101.atlas"},
            {"type": "blob", "path": "src/assets/spines/000101/dating/illust_dating7.png"},
            {"type": "tree", "path": "src/assets/spines/000101/dating"}
        ]}"#,
    );
    let paths = viewer_asset_paths_from_tree_payload(&tree).unwrap();
    let resources = viewer_resources_from_character_list(&characters, Some(&paths));
    let by_stem = |stem: &str| resources.iter().find(|resource| resource.stem == stem).expect("resource");
    assert_eq!(
        by_stem("char000101").files,
        ["000101/char000101.atlas", "000101/char000101.png", "000101/char000101.skel"]
    );
    assert_eq!(
        by_stem("cutscene_char000101").missing_core_files,
        ["000101/cutscene/cutscene_char000101.skel"]
    );
    assert_eq!(
        by_stem("illust_dating7").missing_core_files,
        ["000101/dating/illust_dating7.atlas", "000101/dating/illust_dating7.skel"]
    );
}

// py: tests/test_bd2_l2d_viewer.py::test_gamekee_manifest_resources_match_viewer_stems
#[test]
fn gamekee_manifest_resources_match_viewer_stems() {
    let manifest = dict(
        r#"{"content_id": 701014, "title": "斑鸠", "live2d_models": [{"live2d_key": "xgp6bt8h", "urls": {
            "atlas": "https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.atlas",
            "json": "https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.json",
            "image": ["https://cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/xgp6bt8h/char021001.png"]}}]}"#,
    );
    let viewer = viewer_resources_from_character_list(
        &characters(
            r#"{"021001": {"charName": "Ikaruga", "costumeName": "Noble Flame", "spine": "char021001",
                "cutscene": "", "dating": ""}}"#,
        ),
        None,
    );
    let gamekee = gamekee_resources_from_manifest(&manifest, "manifest.json").unwrap();
    let result = compare_resources(&gamekee, &viewer);
    assert_eq!(result.summary().get("matched_unique_stem_count"), Some(&Value::Int(1)));
    assert!(result.viewer_only.is_empty());
    assert!(result.gamekee_only.is_empty());
}

// py: tests/test_bd2_l2d_viewer.py::test_resource_stem_from_url_strips_gamekee_nested_skel_marker
#[test]
fn resource_stem_from_url_strips_gamekee_nested_skel_marker() {
    assert_eq!(
        resource_stem_from_url("https://cdn.example.com/live2d/char000102.skel.atlas").unwrap(),
        "char000102"
    );
}

// py: tests/test_bd2_l2d_viewer.py::test_viewer_asset_url_quotes_spine_path_segments
#[test]
fn viewer_asset_url_quotes_spine_path_segments() {
    assert_eq!(
        viewer_asset_url("061492/dating/illust dating12.atlas"),
        "https://raw.githubusercontent.com/Jelosus2/BD2-L2D-Viewer/main/src/assets/spines/061492/dating/illust%20dating12.atlas"
    );
}

// py: tests/test_bd2_l2d_viewer.py::test_gamekee_detail_payload_extracts_nested_live2d_values
#[test]
fn gamekee_detail_payload_extracts_nested_live2d_values() {
    let content_json = r#"
{
  "styleData": [
    {
      "data": [
        [
          {
            "type": "live2d",
            "value": {
              "live2dKey": "48w3h3yu",
              "atlas": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.atlas",
              "json": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.json",
              "image": "//cdnimg-v2.gamekee.com/wiki2.0/live2d/50118/48w3h3yu/cutscene_char021001.png"
            }
          }
        ]
      ]
    }
  ]
}
"#;
    let mut data = Dict::new();
    data.insert("id", Value::Int(701_014));
    data.insert("title", Value::Str("斑鸠".to_owned()));
    data.insert("content_json", Value::Str(content_json.to_owned()));
    let mut payload = Dict::new();
    payload.insert("data", Value::Dict(data));
    let resources = gamekee_resources_from_detail_payload(&payload, None).unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].stem, "cutscene_char021001");
    assert_eq!(resources[0].category, Category::Ultimate);
}

/// `fetch_viewer_resources` reads the list as text and the tree as JSON, and a failed
/// request raises with httpx's status text.
#[tokio::test]
async fn fetch_viewer_resources_reads_list_and_tree() {
    let web = FakeWeb::start(|seen| match seen.path() {
        "/list.ts" => Reply::bytes(
            200,
            r#"export default {"000101": {"charName": "A", "costumeName": "B", "spine": "char000101", "cutscene": "", "dating": ""},}"#,
        ),
        "/tree" => Reply::bytes(
            200,
            r#"{"tree": [{"type": "blob", "path": "src/assets/spines/000101/char000101.atlas"}]}"#,
        ),
        "/tree-list" => Reply::bytes(200, "[1]"),
        _ => Reply::status(404),
    })
    .await;
    let client = HttpClient::new(&ClientOptions {
        proxy: Some(web.proxy.clone()),
        timeout: Duration::from_secs(5),
        follow_redirects: true,
        ..ClientOptions::default()
    })
    .unwrap();
    let resources = fetch_viewer_resources(&client, "http://viewer.test/list.ts", "http://viewer.test/tree")
        .await
        .unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].files, ["000101/char000101.atlas"]);
    assert_eq!(resources[0].missing_core_files, ["000101/char000101.skel"]);

    let error = fetch_viewer_resources(&client, "http://viewer.test/list.ts", "http://viewer.test/tree-list")
        .await
        .unwrap_err();
    assert_eq!(error.message, "GitHub tree response did not contain an object");
    let error = fetch_viewer_resources(&client, "http://viewer.test/missing", "http://viewer.test/tree")
        .await
        .unwrap_err();
    assert_eq!(error.class, "HTTPStatusError");
    assert!(
        error
            .message
            .starts_with("Client error '404 Not Found' for url 'http://viewer.test/missing'")
    );
}
