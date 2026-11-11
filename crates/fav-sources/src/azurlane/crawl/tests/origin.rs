//! The l2d.su origin split, the ship-detail retries and the hashed (AZP2) build.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;

use super::super::{Clients, Pacing, asset_client_options, origin_client_options};
use super::{
    Harness, Web, common_reply, english_index_url, has_suffix, index_payload, index_reply, javelin_catalog, live2d_url, model3_payload,
    nagami_mapping, not_found, ok, painting_reply, primary_index_url, ship, test_pacing,
};
use crate::azurlane::l2d_sources::{
    L2D_SU_PRIMARY_REGION, L2D_SU_STATIC_BASE_URL, NAGAMI_MAPPING_URL, l2d_su_ship_detail_url, source_client_options,
};
use crate::azurlane::protected::tests::azp2_wrap;
use crate::azurlane::protected::{Azp2Descriptor, data_descriptor};
use crate::testutil::Reply;

/// `_origin_split_handlers(...)`: the origin web (ship index and details, `failures` of them
/// refused first) and the direct web (everything else).
async fn origin_split_webs(catalog: &str, detail_payload: &str, failures: usize) -> (Web, Web) {
    let (catalog, detail_payload) = (catalog.to_owned(), detail_payload.to_owned());
    let remaining = Arc::new(AtomicUsize::new(failures));
    let origin = Web::start(move |_, url| {
        if let Some(reply) = index_reply(&catalog, url) {
            return reply;
        }
        if url.contains("/data/ships/") {
            if remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1))
                .is_ok()
            {
                return Reply::status(503);
            }
            return ok(detail_payload.clone(), "application/json");
        }
        not_found()
    })
    .await;
    let direct = Web::start(|_, url| {
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if let Some(reply) = painting_reply(url) {
            return reply;
        }
        if url.ends_with(".model3.json") {
            return ok(model3_payload(), "application/json");
        }
        ok("bytes", "application/octet-stream")
    })
    .await;
    (origin, direct)
}

/// `_run_with_origin_split(...)`.
async fn run_with_origin_split(harness: &Harness, origin: &Web, direct: &Web, origin_attempts: u32) {
    let clients = Clients {
        assets: direct.client(asset_client_options()),
        source: direct.client(source_client_options(Duration::from_secs(30))),
        origin_source: Some(origin.client(source_client_options(Duration::from_secs(30)))),
        origin: Some(origin.client(origin_client_options("", ""))),
    };
    harness
        .crawler_with(
            clients,
            Pacing {
                origin_attempts,
                ..test_pacing()
            },
        )
        .update()
        .await
        .expect("update");
}

fn javelin_detail() -> String {
    json!({"ship": {"shipGroupId": 1, "className": "J Class", "skins": []}}).to_string()
}

// py: tests/test_azurlane.py::test_azurlane_sends_only_l2d_su_origin_traffic_through_the_origin_client
#[tokio::test]
async fn azurlane_sends_only_l2d_su_origin_traffic_through_the_origin_client() {
    let harness = Harness::new().await;
    let (origin, direct) = origin_split_webs(&javelin_catalog(), &javelin_detail(), 0).await;
    run_with_origin_split(&harness, &origin, &direct, 3).await;

    // The origin client carries the l2d.su ship data and nothing else: routing CDN assets
    // through a metered proxy would multiply its bandwidth by orders of magnitude.
    let data_prefix = "https://static.l2d.su/al/data/";
    let (origin_urls, direct_urls) = (origin.urls(), direct.urls());
    assert!(!origin_urls.is_empty(), "expected the origin client to be used");
    assert!(origin_urls.iter().all(|url| url.starts_with(data_prefix)), "{origin_urls:?}");
    assert!(!direct_urls.iter().any(|url| url.starts_with(data_prefix)), "{direct_urls:?}");
    assert!(origin_urls.contains(&primary_index_url()));
    assert!(direct_urls.iter().any(|url| url == NAGAMI_MAPPING_URL));
    assert!(direct_urls.iter().any(|url| url.starts_with(L2D_SU_STATIC_BASE_URL)));
}

// py: tests/test_azurlane.py::test_azurlane_retries_ship_detail_on_another_proxy_exit
#[tokio::test]
async fn azurlane_retries_ship_detail_on_another_proxy_exit() {
    let harness = Harness::new().await;
    let (origin, direct) = origin_split_webs(&javelin_catalog(), &javelin_detail(), 2).await;
    run_with_origin_split(&harness, &origin, &direct, 3).await;

    // Two exits refused before a third served the payload; the detail still lands.
    let detail_requests = origin.urls().into_iter().filter(|url| url.contains("/data/ships/")).count();
    assert_eq!(detail_requests, 3);
    let (_, payload) = harness.ship_detail_payload(1).await.expect("a stored detail");
    assert_eq!(payload["ship"]["className"], "J Class");
}

// py: tests/test_azurlane.py::test_azurlane_gives_up_on_ship_detail_after_the_attempt_budget
#[tokio::test]
async fn azurlane_gives_up_on_ship_detail_after_the_attempt_budget() {
    let harness = Harness::new().await;
    let (origin, direct) = origin_split_webs(&javelin_catalog(), "", 99).await;
    run_with_origin_split(&harness, &origin, &direct, 2).await;

    // A ship whose detail never arrives must not block the rest of the run.
    let detail_requests = origin.urls().into_iter().filter(|url| url.contains("/data/ships/")).count();
    assert_eq!(detail_requests, 2);
    assert_eq!(harness.ship_detail_count().await, 0);
    assert!(harness.model("azurlane:painting:javelin:javelin").await.completed);
}

fn hashed_skin(skin_id: i64, digest: &str) -> serde_json::Value {
    json!({
        "id": skin_id,
        "name": format!("Skin {skin_id}"),
        "painting": digest,
        "prefab": digest,
        "dynamicType": "live2d",
        "isLive2d": true,
        "directResources": true,
        "assetPaths": {"painting": format!("18266872a5d491dc/{digest}.webp")},
    })
}

// py: tests/test_azurlane.py::test_azurlane_hashed_build_keeps_the_archive_and_decrypts_new_skins
#[tokio::test]
async fn azurlane_hashed_build_keeps_the_archive_and_decrypts_new_skins() {
    // After l2d.su's switch to hashed, AZP2-encrypted files, an archived skin is left as it is
    // and a new one is fetched from the hashed tree, its encrypted model file opened before it
    // is stored.
    let harness = Harness::new().await;
    let legacy_catalog = javelin_catalog();
    let legacy_model3_url = live2d_url("javelin");
    let legacy = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&legacy_catalog, url) {
            return reply;
        }
        if url == live2d_url("javelin") {
            return ok(model3_payload(), "application/json");
        }
        if url.starts_with(&format!("{L2D_SU_STATIC_BASE_URL}/live2d/javelin/")) {
            return ok("legacy-bytes", "application/octet-stream");
        }
        not_found()
    })
    .await;
    harness.run(&legacy).await.expect("legacy update");
    assert!(harness.model("azurlane:live2d:javelin:javelin").await.completed);

    let descriptor = Azp2Descriptor {
        key: "ab".repeat(32),
        identity: "cd".repeat(32),
    };
    let fragment = format!("#azp2={}.{}", descriptor.key, descriptor.identity);
    let old_skin_model3 = format!("4306804565fc75b8/{}.model3.json{fragment}", "11".repeat(16));
    let new_skin_model3 = format!("4306804565fc75b8/{}.model3.json{fragment}", "22".repeat(16));
    let new_model3_payload = json!({"Version": 3, "FileReferences": {
        "Moc": format!("{}.moc3", "33".repeat(16)),
        "Textures": [format!("../a0722ee10fbfd588/{}.webp", "44".repeat(16))],
    }})
    .to_string()
    .into_bytes();
    let hashed_ship = ship(
        1,
        &"55".repeat(16),
        "Javelin",
        &[hashed_skin(1, &"66".repeat(16)), hashed_skin(2, &"77".repeat(16))],
    );
    let mut detail = json!({"ship": hashed_ship.clone()});
    let mut old_skin = hashed_skin(1, &"66".repeat(16));
    old_skin["model"] = json!({"type": "live2d", "path": old_skin_model3});
    let mut new_skin = hashed_skin(2, &"77".repeat(16));
    new_skin["model"] = json!({"type": "live2d", "path": new_skin_model3});
    detail["ship"]["skins"] = json!([old_skin, new_skin]);
    let hashed_index = index_payload(&[hashed_ship]).into_bytes();
    let detail_bytes = detail.to_string().into_bytes();
    let new_model3_url = format!("{L2D_SU_STATIC_BASE_URL}/{}", new_skin_model3.split('#').next().expect("path"));
    let served_payload = new_model3_payload.clone();
    let hashed = Web::start(move |_, url| {
        // The descriptor fragment stays on the URL but is never sent to the server.
        assert!(!url.contains('#'), "{url}");
        if url == primary_index_url() || url == english_index_url() {
            let path = url.rsplit_once("/data/").expect("a data URL").1;
            return Reply::bytes(200, azp2_wrap(&hashed_index, &data_descriptor(path).expect("descriptor"), 0));
        }
        if url == l2d_su_ship_detail_url(1, L2D_SU_PRIMARY_REGION) {
            return Reply::bytes(
                200,
                azp2_wrap(&detail_bytes, &data_descriptor("ships/CN/1.json").expect("descriptor"), 0),
            );
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if url == new_model3_url {
            return ok(azp2_wrap(&served_payload, &descriptor, 0), "application/json");
        }
        if url.starts_with(&format!("{L2D_SU_STATIC_BASE_URL}/")) && (has_suffix(url, ".moc3") || has_suffix(url, ".webp")) {
            return ok("hashed-bytes", "application/octet-stream");
        }
        not_found()
    })
    .await;
    harness.run(&hashed).await.expect("hashed update");

    // The archived skin keeps its id, its directory and its files; nothing of it is fetched again.
    let archived = harness.model("azurlane:live2d:javelin:javelin").await;
    assert!(archived.completed);
    assert_eq!(archived.primary_url, legacy_model3_url);
    let requested = hashed.urls();
    assert!(
        !requested
            .iter()
            .any(|url| url.contains(&"11".repeat(16)) || url.contains(&"66".repeat(16))),
        "{requested:?}"
    );

    // The new skin joins the ship's directory, laid out as the CDN has it, model file decrypted.
    assert!(harness.model("azurlane:live2d:javelin:skin-2").await.completed);
    let root = "javelin - Javelin/assets/live2d/skin-2";
    assert_eq!(
        harness.read(&format!("{root}/4306804565fc75b8/{}.model3.json", "22".repeat(16))),
        new_model3_payload
    );
    assert_eq!(
        harness.read(&format!("{root}/a0722ee10fbfd588/{}.webp", "44".repeat(16))),
        b"hashed-bytes"
    );
    let (_, payload) = harness.ship_detail_payload(1).await.expect("a stored detail");
    assert_eq!(payload["ship"]["skins"][1]["model"]["path"], new_skin_model3);
}
