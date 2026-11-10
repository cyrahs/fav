//! `update()` over a scripted CDN: what it archives, reuses and records.

use serde_json::json;

use super::{
    Harness, SeedModel, Web, common_reply, has_suffix, index_payload, index_payload_for, javelin_catalog, javelin_live2d_reply,
    javelin_moc_url, javelin_texture_url, live2d_reply, live2d_url, model3_payload, nagami_mapping, not_found, ok, painting_index_payload,
    painting_reply, painting_url, ship, skin, skin_with, sorted, spine_skin, spine_url, unique_sorted, with_header,
};
use crate::azurlane::l2d_sources::{
    L2D_SU_AUDIO_BASE_URL, L2D_SU_ENGLISH_REGION, L2D_SU_LEGACY_STATIC_BASE_URL, L2D_SU_STATIC_BASE_URL, NAGAMI_MAPPING_URL,
    l2d_su_ship_detail_url,
};

fn detail_url(ship_id: i128) -> String {
    l2d_su_ship_detail_url(ship_id, crate::azurlane::l2d_sources::L2D_SU_PRIMARY_REGION)
}

// py: tests/test_azurlane.py::test_azurlane_update_downloads_live2d_assets
#[tokio::test]
async fn azurlane_update_downloads_live2d_assets() {
    let harness = Harness::new().await;
    let catalog = javelin_catalog();
    let web = Web::start(move |_, url| {
        common_reply(&catalog, url)
            .or_else(|| javelin_live2d_reply(url))
            .unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    assert!(harness.character_metadata("javelin").await.0);
    assert!(harness.model("azurlane:live2d:javelin:javelin").await.completed);
    assert!(harness.model("azurlane:painting:javelin:javelin").await.completed);
    let root = "javelin - Javelin";
    assert!(harness.path(&format!("{root}/assets/live2d/javelin/javelin.model3.json")).exists());
    assert_eq!(harness.read(&format!("{root}/assets/live2d/javelin/javelin.moc3")), b"moc-bytes");
    assert_eq!(
        harness.read(&format!("{root}/assets/live2d/javelin/textures/texture_00.webp")),
        b"webp-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/javelin/javelin.webp")),
        b"painting-bytes"
    );
    assert_eq!(
        unique_sorted(harness.model_asset_kinds().await),
        sorted(&[
            "live2d.model3",
            "live2d.moc3",
            "live2d.texture",
            "painting.index",
            "painting.image",
            "painting.mesh"
        ])
    );
    assert_eq!(harness.asset(&javelin_texture_url()).await.status, "downloaded");
}

// py: tests/test_azurlane.py::test_azurlane_update_resolves_model_path_from_ship_detail_when_derived_path_is_missing
#[tokio::test]
async fn azurlane_update_resolves_model_path_from_ship_detail_when_derived_path_is_missing() {
    let harness = Harness::new().await;
    let derived_url = live2d_url("bisimaiz");
    let real_url = live2d_url("bisimaiZ");
    let detail = detail_url(40_505);
    let moc_url = format!("{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/javelin.moc3");
    let texture_url = format!("{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/textures/texture_00.webp");
    let catalog = index_payload(&[ship(40_505, "bisimaiz", "Bismarck Zwei", &[skin(405_050, "Zwei", "bisimaiz")])]);
    let detail_payload = json!({"ship": {"shipGroupId": 40_505, "skins": [
        {"id": 405_050, "model": {"type": "live2d", "path": "live2d/bisimaiZ/bisimaiZ.model3.json"}}
    ]}})
    .to_string();
    let (answer_detail, answer_real) = (detail.clone(), real_url.clone());
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == answer_detail {
            return ok(detail_payload.clone(), "application/json");
        }
        if url == answer_real {
            return ok(model3_payload(), "application/json");
        }
        if url == moc_url {
            return ok("moc-bytes", "application/octet-stream");
        }
        if url == texture_url {
            return ok("webp-bytes", "image/webp");
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    let model = harness.model("azurlane:live2d:bisimaiz:bisimaiz").await;
    assert_eq!(model.primary_url, real_url);
    assert!(model.completed);
    let requested = web.requests();
    assert!(requested.contains(&format!("HEAD {derived_url}")), "{requested:?}");
    assert_eq!(requested.iter().filter(|entry| **entry == format!("GET {detail}")).count(), 1);
    assert_eq!(
        harness.read("bisimaiz - Bismarck Zwei/assets/live2d/bisimaiz/javelin.moc3"),
        b"moc-bytes"
    );
}

// py: tests/test_azurlane.py::test_azurlane_update_reuses_stored_model_path_without_probing
#[tokio::test]
async fn azurlane_update_reuses_stored_model_path_without_probing() {
    let harness = Harness::new().await;
    let real_url = live2d_url("bisimaiZ");
    let moc_url = format!("{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/javelin.moc3");
    let texture_url = format!("{L2D_SU_STATIC_BASE_URL}/live2d/bisimaiZ/textures/texture_00.webp");
    harness
        .seed_model(&SeedModel {
            costume_id: Some(405_050),
            completed: true,
            ..SeedModel::new("azurlane:live2d:bisimaiz:bisimaiz", "live2d", "bisimaiz", &real_url)
        })
        .await;
    let catalog = index_payload(&[ship(40_505, "bisimaiz", "Bismarck Zwei", &[skin(405_050, "Zwei", "bisimaiz")])]);
    harness.seed_ship_detail(&catalog, None).await;
    let answer_real = real_url.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == answer_real {
            return ok(model3_payload(), "application/json");
        }
        if url == moc_url {
            return ok("moc-bytes", "application/octet-stream");
        }
        if url == texture_url {
            return ok("webp-bytes", "image/webp");
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    assert_eq!(harness.model("azurlane:live2d:bisimaiz:bisimaiz").await.primary_url, real_url);
    let requested = web.requests();
    assert!(!requested.iter().any(|entry| entry.starts_with("HEAD ")), "{requested:?}");
    assert!(!requested.iter().any(|entry| entry.contains("/data/ships/")), "{requested:?}");
}

// py: tests/test_azurlane.py::test_azurlane_update_ignores_a_stored_path_archived_under_another_model_type
#[tokio::test]
async fn azurlane_update_ignores_a_stored_path_archived_under_another_model_type() {
    // A costume re-released as spine must not inherit the model3.json it was archived under as Live2D.
    let harness = Harness::new().await;
    let stale_live2d_url = live2d_url("antu_2");
    harness
        .seed_model(&SeedModel {
            costume_id: Some(304_092),
            completed: true,
            active: false,
            ..SeedModel::new("azurlane:live2d:antu:antu_3", "live2d", "antu", &stale_live2d_url)
        })
        .await;
    let spine = spine_url("antu_3");
    let atlas_text = "\nantu_3.webp\nsize: 4096,4096\nformat: RGBA8888\n";
    let catalog = index_payload(&[ship(30_409, "antu", "Azuchi", &[spine_skin(304_092, "Elevator", "antu_3")])]);
    let answer_spine = spine.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == format!("{answer_spine}/antu_3.skel") {
            return ok("skel-bytes", "application/octet-stream");
        }
        if url == format!("{answer_spine}/antu_3.atlas") {
            return ok(atlas_text, "text/plain");
        }
        if url == format!("{answer_spine}/antu_3.webp") {
            return ok("texture-bytes", "image/webp");
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    let spine_row = harness.model("azurlane:spine:antu:antu_3").await;
    assert_eq!(spine_row.primary_url, spine);
    assert!(spine_row.completed);
    assert!(!web.urls().contains(&stale_live2d_url));
}

// py: tests/test_azurlane.py::test_azurlane_update_writes_backend_manifests_and_source_artifacts
#[tokio::test]
async fn azurlane_update_writes_backend_manifests_and_source_artifacts() {
    let harness = Harness::new().await;
    let model3_url = live2d_url("javelin");
    let texture_url = javelin_texture_url();
    let model3 = model3_payload();
    let catalog = index_payload(&[ship(1, "javelin", "标枪", &[skin(1, "默认", "javelin")])]);
    let english_catalog = index_payload_for(
        &[ship(1, "javelin", "Javelin", &[skin(1, "Default", "javelin")])],
        L2D_SU_ENGLISH_REGION,
    );
    let web = Web::start(move |_, url| {
        if url == super::primary_index_url() {
            return with_header(ok(catalog.clone(), "application/json"), "etag", "\"l2d\"");
        }
        if url == super::english_index_url() {
            return ok(english_catalog.clone(), "application/json");
        }
        if url == NAGAMI_MAPPING_URL {
            return with_header(nagami_mapping(&json!({"javelin": "Javelin"})), "etag", "\"nagami\"");
        }
        painting_reply(url).or_else(|| javelin_live2d_reply(url)).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    let l2d_snapshot = harness.read_json("_source/l2d-su-snapshot.json");
    let nagami_snapshot = harness.read_json("_source/nagami-snapshot.json");
    let health_report = harness.read_json("_source/health-report.json");
    let manifest = harness.read_json("javelin - Javelin/manifest.json");
    let character = harness.read_json("javelin - Javelin/character.json");

    assert_eq!(l2d_snapshot["metadata"]["etag"], "\"l2d\"");
    assert_eq!(nagami_snapshot["metadata"]["etag"], "\"nagami\"");
    assert_eq!(health_report["catalog_health"]["entry_count"], 2);
    assert_eq!(manifest["schema_version"], 1);
    assert_eq!(manifest["character_key"], "javelin");
    assert_eq!(
        manifest["model_counts"],
        json!({"live2d": 1, "spine": 0, "painting": 1, "total": 2})
    );
    assert_eq!(character["models"][0]["model_id"], "azurlane:live2d:javelin:javelin");

    let model = &manifest["models"][0];
    let nagami_model3 = "https://cdn.nagami.moe/live2d/javelin/javelin.model3.json";
    assert_eq!(model["source"], "merged");
    assert_eq!(
        model["source_urls"],
        json!({"primary": model3_url, "fallback": nagami_model3, "display_info": ""})
    );
    assert_eq!(model["availability"]["archive_state"], "complete");
    assert_eq!(model["availability"]["asset_status_counts"], json!({"downloaded": 3}));

    let assets = model["assets"].as_array().expect("assets");
    let by_kind = |kind: &str| assets.iter().find(|asset| asset["kind"] == kind).expect(kind);
    assert_eq!(
        by_kind("live2d.model3")["sha256"],
        crate::bd2::gamekee::sha256_hex(model3.as_bytes())
    );
    assert_eq!(by_kind("live2d.model3")["fallback_url"], nagami_model3);
    assert_eq!(
        by_kind("live2d.moc3")["fallback_url"],
        "https://cdn.nagami.moe/live2d/javelin/javelin.moc3"
    );
    let texture = by_kind("live2d.texture");
    assert_eq!(texture["content_type"], "image/webp");
    assert_eq!(texture["source_urls"]["primary"], texture_url);
    assert_eq!(texture["available"], true);
    assert_eq!(texture["contexts"][0]["fallback_model_url"], nagami_model3);
}

// py: tests/test_azurlane.py::test_azurlane_update_downloads_spine_assets
#[tokio::test]
async fn azurlane_update_downloads_spine_assets() {
    let harness = Harness::new().await;
    let spine = spine_url("iris_2");
    let atlas_text = "\niris_2.webp\nsize: 4096,4096\nformat: RGBA8888\n";
    let catalog = index_payload(&[ship(2, "iris", "Iris", &[spine_skin(2, "Afternoon", "iris_2")])]);
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == format!("{spine}/iris_2.skel") {
            return ok("skel-bytes", "application/octet-stream");
        }
        if url == format!("{spine}/iris_2.atlas") {
            return ok(atlas_text, "text/plain");
        }
        if url == format!("{spine}/iris_2.webp") {
            return ok("texture-bytes", "image/webp");
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    assert!(harness.model("azurlane:spine:iris:iris_2").await.completed);
    assert_eq!(harness.read("iris - Iris/assets/spine/iris_2/iris_2.skel"), b"skel-bytes");
    assert_eq!(harness.read("iris - Iris/assets/spine/iris_2/iris_2.atlas"), atlas_text.as_bytes());
    assert_eq!(harness.read("iris - Iris/assets/spine/iris_2/iris_2.webp"), b"texture-bytes");
    assert_eq!(
        unique_sorted(harness.model_asset_kinds().await),
        sorted(&[
            "spine.skel",
            "spine.atlas",
            "spine.texture",
            "painting.index",
            "painting.image",
            "painting.mesh"
        ])
    );
}

// py: tests/test_azurlane.py::test_azurlane_update_downloads_multi_part_spine_model
#[tokio::test]
async fn azurlane_update_downloads_multi_part_spine_model() {
    let harness = Harness::new().await;
    let spine = spine_url("iris_2");
    let parts_payload = json!({"version": 1, "models": [
        {"name": "iris_2B", "skeleton": "iris_2B.skel", "atlases": ["iris_2B.atlas"], "animation": "normal", "loop": true},
        {"name": "iris_2T", "skeleton": "iris_2T.skel", "atlases": ["iris_2T.atlas"], "animation": "normal", "loop": true},
    ]})
    .to_string();
    let catalog = index_payload(&[ship(2, "iris", "Iris", &[spine_skin(2, "Afternoon", "iris_2")])]);
    let served_parts = parts_payload.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        let Some(name) = url.strip_prefix(&format!("{spine}/")) else {
            return not_found();
        };
        match name {
            "iris_2.json" => ok(served_parts.clone(), "application/json"),
            "iris_2B.skel" | "iris_2T.skel" => ok("skel-bytes", "application/octet-stream"),
            "iris_2B.atlas" => ok("\niris_2B.webp\nsize: 4096,4096\nformat: RGBA8888\n", "text/plain"),
            "iris_2T.atlas" => ok("\niris_2T.webp\nsize: 4096,4096\nformat: RGBA8888\n", "text/plain"),
            "iris_2B.webp" | "iris_2T.webp" => ok("texture-bytes", "image/webp"),
            _ => not_found(),
        }
    })
    .await;
    harness.run(&web).await.expect("update");

    let root = "iris - Iris/assets/spine/iris_2";
    assert!(harness.model("azurlane:spine:iris:iris_2").await.completed);
    assert_eq!(harness.read(&format!("{root}/iris_2.json")), parts_payload.as_bytes());
    assert_eq!(harness.read(&format!("{root}/iris_2B.skel")), b"skel-bytes");
    assert_eq!(harness.read(&format!("{root}/iris_2T.webp")), b"texture-bytes");
    assert!(!harness.path(&format!("{root}/iris_2.skel")).exists());
    let kinds = harness.model_asset_kinds().await;
    assert_eq!(
        unique_sorted(kinds.clone()),
        sorted(&[
            "spine.parts",
            "spine.skel",
            "spine.atlas",
            "spine.texture",
            "painting.index",
            "painting.image",
            "painting.mesh"
        ])
    );
    assert_eq!(kinds.iter().filter(|kind| *kind == "spine.skel").count(), 2);
}

// py: tests/test_azurlane.py::test_azurlane_update_reuses_completed_blob_for_archived_url
#[tokio::test]
async fn azurlane_update_reuses_completed_blob_for_archived_url() {
    let harness = Harness::new().await;
    let texture_url = javelin_texture_url();
    harness
        .seed_downloaded_blob(&texture_url, "live2d.texture", b"already-archived-texture", "image/webp")
        .await;
    let catalog = javelin_catalog();
    let answer_texture = texture_url.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == answer_texture {
            // The texture is to be reused from the archived blob; a 500 would fail the model.
            return Reply::status(500);
        }
        javelin_live2d_reply(url).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    assert_eq!(
        harness.read("javelin - Javelin/assets/live2d/javelin/textures/texture_00.webp"),
        b"already-archived-texture"
    );
    assert!(!web.urls().contains(&texture_url));
    assert_eq!(harness.asset(&texture_url).await.status, "downloaded");
}

use crate::testutil::Reply;

/// `test_azurlane_update_reuses_blobs_archived_under_the_old_cdn_root[archived_base_url]`.
async fn reuses_blobs_archived_under(archived_base_url: &str) {
    // The CDN moved every asset from /azurlane/ to /al/assets/webp/, /al-b/assets/ and
    // /al/assets/; nothing archived is fetched again.
    let harness = Harness::new().await;
    let model3_url = live2d_url("javelin");
    let legacy_contents = [
        (
            model3_url.clone(),
            model3_payload().into_bytes(),
            "live2d.model3",
            "application/json",
        ),
        (
            javelin_moc_url(),
            b"archived-moc".to_vec(),
            "live2d.moc3",
            "application/octet-stream",
        ),
        (javelin_texture_url(), b"archived-texture".to_vec(), "live2d.texture", "image/webp"),
    ];
    for (url, content, kind, content_type) in &legacy_contents {
        let legacy_url = url.replace(L2D_SU_STATIC_BASE_URL, archived_base_url);
        harness.seed_downloaded_blob(&legacy_url, kind, content, content_type).await;
    }
    let catalog = javelin_catalog();
    let legacy_urls: Vec<String> = legacy_contents.iter().map(|(url, ..)| url.clone()).collect();
    let answer_urls = legacy_urls.clone();
    let web = Web::start(move |method, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if answer_urls.iter().any(|legacy| legacy == url) && method == "GET" {
            // Reused from the blob archived under the old root, never downloaded.
            return Reply::status(500);
        }
        if url == model3_url {
            return Reply::status(200);
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    let root = "javelin - Javelin/assets/live2d/javelin";
    assert_eq!(harness.read(&format!("{root}/javelin.moc3")), b"archived-moc");
    assert_eq!(harness.read(&format!("{root}/textures/texture_00.webp")), b"archived-texture");
    for url in &legacy_urls {
        assert_eq!(harness.asset(url).await.status, "downloaded", "{url}");
        assert!(!web.requests().contains(&format!("GET {url}")), "{url}");
    }
}

// py: tests/test_azurlane.py::test_azurlane_update_reuses_blobs_archived_under_the_old_cdn_root
#[tokio::test]
async fn azurlane_update_reuses_blobs_archived_under_the_old_cdn_root() {
    for base in [
        "https://static.l2d.su/al-b/assets",
        "https://static.l2d.su/al/assets/webp",
        L2D_SU_LEGACY_STATIC_BASE_URL,
    ] {
        reuses_blobs_archived_under(base).await;
    }
}

// py: tests/test_azurlane.py::test_azurlane_update_records_failed_asset_state
#[tokio::test]
async fn azurlane_update_records_failed_asset_state() {
    let harness = Harness::new().await;
    let texture_url = javelin_texture_url();
    let catalog = javelin_catalog();
    let answer_texture = texture_url.clone();
    let web = Web::start(move |_, url| {
        if url == answer_texture {
            return not_found();
        }
        common_reply(&catalog, url)
            .or_else(|| javelin_live2d_reply(url))
            .unwrap_or_else(not_found)
    })
    .await;
    let error = harness.run(&web).await.expect_err("the run fails");
    assert_eq!(error.class_name(), "CrawlRunError");

    let failed = harness.asset(&texture_url).await;
    assert_eq!(failed.status, "failed");
    assert_eq!(failed.failed_count, 1);
    assert!(failed.last_error.contains("HTTP 404"), "{}", failed.last_error);
    assert_eq!(failed.cooling, Some(true));
    let linked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM azurlane_model_assets WHERE url = $1 AND kind = 'live2d.texture'")
        .bind(&texture_url)
        .fetch_one(harness.db.pool())
        .await
        .expect("count");
    assert_eq!(linked, 1);
    let model = harness.model("azurlane:live2d:javelin:javelin").await;
    assert_eq!(model.failed_count, 1);
    assert_eq!(model.cooling, Some(true));
    assert!(model.last_error.contains("Azur Lane assets failed"), "{}", model.last_error);
}

/// `_run_javelin_missing_texture_crawl(...)`: one `update()` over a Javelin whose texture the
/// CDN does not host until it does; the asset URLs it asked for, and how it ended.
async fn run_javelin_missing_texture_crawl(harness: &Harness, texture_hosted: bool) -> (Vec<String>, Result<(), String>) {
    let catalog = javelin_catalog();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorder = std::sync::Arc::clone(&seen);
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        recorder.lock().expect("lock").push(url.to_owned());
        if url == live2d_url("javelin") {
            return ok(model3_payload(), "application/json");
        }
        if url == javelin_moc_url() {
            return ok("moc-bytes", "application/octet-stream");
        }
        if texture_hosted && url == javelin_texture_url() {
            return ok("texture-bytes", "image/webp");
        }
        not_found()
    })
    .await;
    let result = harness.run(&web).await.map_err(|error| error.class_name().to_owned());
    let urls = seen.lock().expect("lock").clone();
    (urls, result)
}

const JAVELIN_LIVE2D: &str = "azurlane:live2d:javelin:javelin";

// py: tests/test_azurlane.py::test_azurlane_update_stays_quiet_while_a_failed_model_is_in_cooldown
#[tokio::test]
async fn azurlane_update_stays_quiet_while_a_failed_model_is_in_cooldown() {
    let harness = Harness::new().await;
    let (first_run_urls, first) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(first, Err("CrawlRunError".to_owned()));
    assert!(!first_run_urls.is_empty());

    // The source has not changed, so the next run has nothing to report -- and nothing to
    // download either. The cheap existence probe that would notice a corrected path survives;
    // re-fetching the model's assets to watch them fail again does not.
    let (second_run_urls, second) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(second, Ok(()));
    assert!(!second_run_urls.contains(&javelin_texture_url()));
    assert!(!second_run_urls.contains(&javelin_moc_url()));
    assert!(second_run_urls.len() < first_run_urls.len());
    assert_eq!(harness.asset(&javelin_texture_url()).await.failed_count, 1);
    assert_eq!(harness.model(JAVELIN_LIVE2D).await.failed_count, 1);
}

// py: tests/test_azurlane.py::test_azurlane_update_reports_a_failed_model_again_once_its_cooldown_lapses
#[tokio::test]
async fn azurlane_update_reports_a_failed_model_again_once_its_cooldown_lapses() {
    let harness = Harness::new().await;
    let (_, first) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(first, Err("CrawlRunError".to_owned()));

    harness.lapse_cooldowns().await;
    let (retry_urls, retry) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(retry, Err("CrawlRunError".to_owned()));
    assert!(retry_urls.contains(&javelin_texture_url()));
    assert_eq!(harness.model(JAVELIN_LIVE2D).await.failed_count, 2);
}

// py: tests/test_azurlane.py::test_azurlane_update_retries_a_failed_model_when_the_source_moves_its_url
#[tokio::test]
async fn azurlane_update_retries_a_failed_model_when_the_source_moves_its_url() {
    let harness = Harness::new().await;
    let (_, first) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(first, Err("CrawlRunError".to_owned()));

    assert_eq!(harness.model(JAVELIN_LIVE2D).await.cooling, Some(true));
    harness
        .sql(&format!(
            "UPDATE azurlane_models SET primary_url = '{}' WHERE model_id = '{JAVELIN_LIVE2D}';
            UPDATE azurlane_assets SET next_retry_at = NOW() - INTERVAL '1 minute' WHERE next_retry_at IS NOT NULL;",
            live2d_url("javelinOld")
        ))
        .await;
    // The model's own wait has not lapsed: only the moved URL lets it be tried again.
    assert_eq!(harness.model(JAVELIN_LIVE2D).await.cooling, Some(true));
    let (retry_urls, retry) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(retry, Err("CrawlRunError".to_owned()));
    assert!(retry_urls.contains(&javelin_texture_url()));
}

// py: tests/test_azurlane.py::test_azurlane_update_clears_the_retry_state_once_the_source_serves_the_model
#[tokio::test]
async fn azurlane_update_clears_the_retry_state_once_the_source_serves_the_model() {
    let harness = Harness::new().await;
    let (_, first) = run_javelin_missing_texture_crawl(&harness, false).await;
    assert_eq!(first, Err("CrawlRunError".to_owned()));

    harness.lapse_cooldowns().await;
    let (_, served) = run_javelin_missing_texture_crawl(&harness, true).await;
    assert_eq!(served, Ok(()));
    let model = harness.model(JAVELIN_LIVE2D).await;
    assert_eq!(model.failed_count, 0);
    assert_eq!(model.cooling, None);
    assert_eq!(model.last_error, "");
}

// py: tests/test_azurlane.py::test_azurlane_update_recovers_a_painting_stored_under_a_different_case
#[tokio::test]
async fn azurlane_update_recovers_a_painting_stored_under_a_different_case() {
    // The index says painting/u47_5.webp; only painting/U47_5.webp exists on the CDN.
    let harness = Harness::new().await;
    let catalog = index_payload(&[ship(1, "u47", "U-47", &[skin(1, "Default", "u47_5")])]);
    harness.seed_ship_detail(&catalog, None).await;
    let indexed = format!("{L2D_SU_STATIC_BASE_URL}/painting/u47_5.webp");
    let stored = format!("{L2D_SU_STATIC_BASE_URL}/painting/U47_5.webp");
    let answer_stored = stored.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if url == answer_stored {
            return ok("painting-bytes", "image/webp");
        }
        if url == live2d_url("u47_5") {
            return ok(model3_payload(), "application/json");
        }
        // Names come from the shared model3 payload.
        if url == format!("{L2D_SU_STATIC_BASE_URL}/live2d/u47_5/javelin.moc3") {
            return Reply::bytes(200, "moc");
        }
        if url == format!("{L2D_SU_STATIC_BASE_URL}/live2d/u47_5/textures/texture_00.webp") {
            return Reply::bytes(200, "tex");
        }
        not_found()
    })
    .await;
    harness.run(&web).await.expect("update");

    // The URL the source named is tried first and only then the case variant.
    let urls = web.urls();
    let position = |url: &str| {
        urls.iter()
            .position(|seen| seen == url)
            .unwrap_or_else(|| panic!("{url} not requested"))
    };
    assert!(position(&indexed) < position(&stored));
    let asset = harness.asset(&indexed).await;
    assert_eq!(asset.status, "downloaded");
    assert_eq!(asset.downloaded_url, stored);
    assert!(harness.model("azurlane:painting:u47:u47_5").await.completed);
}

// py: tests/test_azurlane.py::test_azurlane_update_completes_a_model_whose_optional_assets_are_missing
#[tokio::test]
async fn azurlane_update_completes_a_model_whose_optional_assets_are_missing() {
    // The source advertises companion files it does not host; losing one must not fail the model.
    let harness = Harness::new().await;
    let catalog = index_payload(&[ship(
        1,
        "javelin",
        "Javelin",
        &[skin_with(1, "Default", "javelin", "live2d", Some(&["1"]), true)],
    )]);
    harness.seed_ship_detail(&catalog, None).await;
    let web = Web::start(move |_, url| {
        // Every icon and face 404s, exactly as the live CDN does for these skins.
        common_reply(&catalog, url)
            .or_else(|| {
                (url == javelin_texture_url())
                    .then(|| ok("texture-bytes", "image/webp"))
                    .or_else(|| javelin_live2d_reply(url))
            })
            .unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    assert!(harness.model("azurlane:painting:javelin:javelin").await.completed);
    assert_eq!(
        harness
            .asset(&format!("{L2D_SU_STATIC_BASE_URL}/squareicon/javelin.webp"))
            .await
            .status,
        "failed"
    );
    // No qicon is ever requested: the index offers one for every skin and the CDN has none.
    assert!(!harness.asset_urls().await.iter().any(|url| url.contains("/qicon/")));
}

// py: tests/test_azurlane.py::test_azurlane_update_downloads_painting_faces_icons_and_voices
#[tokio::test]
async fn azurlane_update_downloads_painting_faces_icons_and_voices() {
    let harness = Harness::new().await;
    let detail = detail_url(1);
    let voice_url = format!("{L2D_SU_AUDIO_BASE_URL}/cue/cv-1/detail.ogg");
    let face_url = format!("{L2D_SU_STATIC_BASE_URL}/paintingface/javelin/1.webp");
    let icon_urls = ["squareicon", "shipyardicon", "qicon"].map(|kind| format!("{L2D_SU_STATIC_BASE_URL}/{kind}/javelin.webp"));
    let catalog = index_payload(&[ship(
        1,
        "javelin",
        "Javelin",
        &[skin_with(1, "Default", "javelin", "live2d", Some(&["1"]), true)],
    )]);
    let detail_payload = json!({"ship": {"shipGroupId": 1, "skins": [
        {"id": 1, "words": [{"key": "detail", "text": "嗯", "voicePath": "cue/cv-1/detail"}]}
    ]}})
    .to_string();
    let answer_detail = detail.clone();
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if url == answer_detail {
            return ok(detail_payload.clone(), "application/json");
        }
        if let Some(reply) = painting_reply(url) {
            return reply;
        }
        if url == face_url || icon_urls.iter().any(|icon| icon == url) {
            return ok("webp-bytes", "image/webp");
        }
        if url == voice_url {
            return ok("ogg-bytes", "audio/ogg");
        }
        javelin_live2d_reply(url).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    let root = "javelin - Javelin";
    assert_eq!(web.requests().iter().filter(|entry| **entry == format!("GET {detail}")).count(), 1);
    let (fingerprint, payload) = harness.ship_detail_payload(1).await.expect("a stored detail");
    assert!(!fingerprint.is_empty());
    assert_eq!(payload["ship"]["shipGroupId"], 1);
    assert!(harness.model("azurlane:painting:javelin:javelin").await.completed);
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/javelin/javelin.webp")),
        b"painting-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/javelin/paintingface/javelin/1.webp")),
        b"webp-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/javelin/squareicon/javelin.webp")),
        b"webp-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/javelin/cue/cv-1/detail.ogg")),
        b"ogg-bytes"
    );
    assert!(harness.path(&format!("{root}/detail.json")).exists());
    let contexts: Vec<String> =
        sqlx::query_scalar("SELECT context_json::text FROM azurlane_model_assets WHERE kind = 'voice.audio' ORDER BY context_hash")
            .fetch_all(harness.db.pool())
            .await
            .expect("voice rows");
    let context: serde_json::Value = serde_json::from_str(&contexts[0]).expect("context");
    assert_eq!(context["text"], "嗯");
    assert_eq!(context["voice_path"], "cue/cv-1/detail");
}

fn xiafei_layers() -> serde_json::Value {
    json!({
        "xiafei_4": {"size": [4574, 2866], "pivot": [0.49, 0.66], "position": [0, 0], "rawSize": [2048, 1283], "raw": true},
        "xiafei_4_rw": {"size": [2000, 2048], "pivot": [0.49, 0.66], "position": [10, 272], "rawSize": [2000, 2048]},
    })
}

// py: tests/test_azurlane.py::test_azurlane_update_archives_painting_layers_and_meshes
#[tokio::test]
async fn azurlane_update_archives_painting_layers_and_meshes() {
    // The packed sheet alone is not artwork: the index, the sibling layers and their meshes reassemble it.
    let harness = Harness::new().await;
    let index_url = format!("{L2D_SU_STATIC_BASE_URL}/painting/xiafei_4.json");
    let catalog = index_payload(&[ship(1, "xiafei", "Richelieu", &[skin(1, "Skin", "xiafei_4")])]);
    harness.seed_ship_detail(&catalog, None).await;
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if url == index_url {
            return ok(painting_index_payload("xiafei_4", Some(xiafei_layers())), "application/json");
        }
        painting_reply(url).or_else(|| live2d_reply(url)).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    let root = "xiafei - Richelieu";
    let model = harness.model("azurlane:painting:xiafei:xiafei_4").await;
    assert!(model.completed);
    assert!(harness.path(&format!("{root}/assets/painting/xiafei_4/xiafei_4.json")).exists());
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/xiafei_4/xiafei_4.webp")),
        b"painting-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/xiafei_4/painting/xiafei_4_rw.webp")),
        b"painting-bytes"
    );
    assert_eq!(
        harness.read(&format!("{root}/assets/painting/xiafei_4/painting/xiafei_4_rw-mesh.obj")),
        b"g mesh\nv 0 0 0\n"
    );
    // xiafei_4 is `raw: true`, so it is stored unpacked and has no mesh to ask for.
    assert!(
        !web.requests()
            .contains(&format!("GET {L2D_SU_STATIC_BASE_URL}/painting/xiafei_4-mesh.obj"))
    );

    let painting_index = &model.source_metadata["painting_index"];
    assert_eq!(painting_index["painting_key"], "xiafei_4");
    let names: Vec<&str> = painting_index["layers"]
        .as_array()
        .expect("layers")
        .iter()
        .map(|layer| layer["name"].as_str().expect("name"))
        .collect();
    assert_eq!(names, ["xiafei_4", "xiafei_4_rw"]);
    assert_eq!(painting_index["layers"][0]["raw"], true);
    assert_eq!(painting_index["layers"][1]["rawSize"], json!([2000.0, 2048.0]));
    assert_eq!(painting_index["face"]["size"], json!([100.0, 100.0]));

    let manifest = harness.read_json(&format!("{root}/manifest.json"));
    let painting_model = manifest["models"]
        .as_array()
        .expect("models")
        .iter()
        .find(|item| item["type"] == "painting")
        .expect("the painting");
    assert_eq!(
        painting_model["source_metadata"]["painting_index"]["layers"][1]["position"],
        json!([10.0, 272.0])
    );
    assert_eq!(painting_model["asset_counts"]["painting.mesh"], 1);
}

// py: tests/test_azurlane.py::test_azurlane_update_rejects_a_mesh_answered_with_the_site_shell
#[tokio::test]
async fn azurlane_update_rejects_a_mesh_answered_with_the_site_shell() {
    // A mesh the CDN does not host comes back as 200 text/html, which must never be stored as an .obj.
    let harness = Harness::new().await;
    let mesh_url = format!("{L2D_SU_STATIC_BASE_URL}/painting/biaoqiang-mesh.obj");
    let catalog = index_payload(&[ship(1, "biaoqiang", "Javelin", &[skin(1, "Default", "biaoqiang")])]);
    harness.seed_ship_detail(&catalog, None).await;
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if url.ends_with("-mesh.obj") {
            return ok("<!doctype html><html><body>l2d.su</body></html>", "text/html");
        }
        painting_reply(url).or_else(|| live2d_reply(url)).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    assert!(
        !harness
            .path("biaoqiang - Javelin/assets/painting/biaoqiang/painting/biaoqiang-mesh.obj")
            .exists()
    );
    let mesh = harness.asset(&mesh_url).await;
    assert_eq!(mesh.status, "failed");
    assert!(mesh.last_error.contains("Wavefront OBJ"), "{}", mesh.last_error);
    // The answer is definite, so it is not retried -- and it must not read as a CDN block
    // either, or five missing meshes would pause the whole run.
    assert_eq!(
        web.requests().iter().filter(|entry| **entry == format!("GET {mesh_url}")).count(),
        1
    );
    // A mesh is a companion file: losing it costs the reassembly, not the model.
    assert!(harness.model("azurlane:painting:biaoqiang:biaoqiang").await.completed);
}

// py: tests/test_azurlane.py::test_azurlane_update_keeps_a_stored_painting_index_when_the_cdn_copy_is_gone
#[tokio::test]
async fn azurlane_update_keeps_a_stored_painting_index_when_the_cdn_copy_is_gone() {
    let harness = Harness::new().await;
    let catalog = index_payload(&[ship(1, "biaoqiang", "Javelin", &[skin(1, "Default", "biaoqiang")])]);
    harness.seed_ship_detail(&catalog, None).await;
    let model_id = "azurlane:painting:biaoqiang:biaoqiang";
    let primary = painting_url("biaoqiang");
    harness
        .seed_model(&SeedModel {
            costume_key: "biaoqiang",
            costume_id: Some(1),
            completed: true,
            source_metadata: json!({"painting_index": {"painting_key": "biaoqiang", "layers": [{"name": "biaoqiang"}]}}),
            ..SeedModel::new(model_id, "painting", "biaoqiang", &primary)
        })
        .await;
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return nagami_mapping(&json!({}));
        }
        if let Some(reply) = live2d_reply(url) {
            return reply;
        }
        if url.starts_with(&format!("{L2D_SU_STATIC_BASE_URL}/painting/")) && has_suffix(url, ".json") {
            return not_found();
        }
        painting_reply(url).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    // The catalog refresh rewrites source_metadata every run; geometry it cannot refetch must survive.
    let stored = harness.model(model_id).await.source_metadata;
    assert_eq!(stored["painting_index"]["painting_key"], "biaoqiang");
    // The rest of source_metadata is the refreshed catalog entry.
    assert_eq!(stored["id"], model_id);
}

// py: tests/test_azurlane.py::test_azurlane_update_uses_stored_detail_for_voices_without_origin_requests
#[tokio::test]
async fn azurlane_update_uses_stored_detail_for_voices_without_origin_requests() {
    let harness = Harness::new().await;
    let voice_url = format!("{L2D_SU_AUDIO_BASE_URL}/cue/cv-1/detail.ogg");
    let catalog = javelin_catalog();
    harness
        .seed_ship_detail(
            &catalog,
            Some(json!({"ship": {"shipGroupId": 1, "skins": [{"id": 1, "words": [{"key": "detail", "voicePath": "cue/cv-1/detail"}]}]}})),
        )
        .await;
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url == voice_url {
            return ok("ogg-bytes", "audio/ogg");
        }
        javelin_live2d_reply(url).unwrap_or_else(not_found)
    })
    .await;
    harness.run(&web).await.expect("update");

    assert!(!web.requests().iter().any(|entry| entry.contains("/data/ships/")));
    assert_eq!(
        harness.read("javelin - Javelin/assets/painting/javelin/cue/cv-1/detail.ogg"),
        b"ogg-bytes"
    );
}

// py: tests/test_azurlane.py::test_azurlane_update_records_list_level_character_metadata
#[tokio::test]
async fn azurlane_update_records_list_level_character_metadata() {
    let harness = Harness::new().await;
    let mut swimsuit = skin(2, "泳装", "javelin_2");
    swimsuit["shopTypeName"] = json!("Swimsuits");
    let mut javelin = ship(1, "javelin", "Javelin", &[skin(1, "Default", "javelin"), swimsuit]);
    javelin["defaultSkinId"] = json!(1);
    let catalog = index_payload(&[javelin]);
    harness
        .seed_ship_detail(
            &catalog,
            Some(json!({"ship": {"shipGroupId": 1, "className": "J Class", "skins": []}})),
        )
        .await;
    let web = Web::start(move |_, url| {
        if let Some(reply) = common_reply(&catalog, url) {
            return reply;
        }
        if url.ends_with(".model3.json") {
            return ok(model3_payload(), "application/json");
        }
        ok("bytes", "application/octet-stream")
    })
    .await;
    harness.run(&web).await.expect("update");

    let (_, metadata) = harness.character_metadata("javelin").await;
    // A detail-only field, recovered from the stored payload.
    assert_eq!(metadata["class_name"], "J Class");
    assert_eq!(metadata["default_skin_id"], 1);
    assert_eq!(metadata["skin_series"], json!(["Skin", "Swimsuits"]));
}

// py: tests/test_azurlane.py::test_azurlane_update_fails_loudly_when_the_catalog_comes_back_empty
#[tokio::test]
async fn azurlane_update_fails_loudly_when_the_catalog_comes_back_empty() {
    // Regression: an empty catalog used to return early, hiding which source had failed.
    let harness = Harness::new().await;
    let empty_catalog = index_payload(&[]);
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&empty_catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            return not_found();
        }
        // No asset download runs without a catalog.
        Reply::status(500)
    })
    .await;
    let error = harness.run(&web).await.expect_err("the run fails");
    assert_eq!(error.class_name(), "CrawlRunError");
    assert!(error.message().contains("no model entries"), "{}", error.message());
    assert!(error.message().contains(NAGAMI_MAPPING_URL), "{}", error.message());
    assert!(web.urls().iter().all(|url| url.contains("/data/") || url == NAGAMI_MAPPING_URL));
}

// py: tests/test_azurlane.py::test_azurlane_update_preserves_catalog_state_when_source_snapshots_incomplete
#[tokio::test]
async fn azurlane_update_preserves_catalog_state_when_source_snapshots_incomplete() {
    let harness = Harness::new().await;
    let model_id = JAVELIN_LIVE2D;
    let primary_url = live2d_url("javelin");
    let fallback_url = "https://cdn.nagami.moe/live2d/javelin/javelin.model3.json";
    let source_metadata = json!({
        "id": model_id,
        "source": "merged",
        "resources": {"primary_url": primary_url, "fallback_url": fallback_url},
    });
    harness
        .seed_character(
            "javelin",
            None,
            "",
            "",
            "",
            &json!({"model_ids": [model_id], "sources": ["merged"]}),
        )
        .await;
    harness
        .seed_model(&SeedModel {
            source: "merged",
            fallback_url,
            completed: true,
            source_metadata: source_metadata.clone(),
            ..SeedModel::new(model_id, "live2d", "javelin", &primary_url)
        })
        .await;
    let catalog = javelin_catalog();
    let web = Web::start(move |_, url| {
        if let Some(reply) = super::index_reply(&catalog, url) {
            return reply;
        }
        if url == NAGAMI_MAPPING_URL {
            // httpx.ConnectError
            return Reply::Reset;
        }
        // No asset download runs while the source snapshots are incomplete.
        Reply::status(500)
    })
    .await;
    // The catalog is preserved, but the run reports failure: a source that fails quietly is
    // how this crawler stayed broken for a month with every run recorded as completed.
    let error = harness.run(&web).await.expect_err("the run fails");
    assert_eq!(error.class_name(), "CrawlRunError");
    assert!(error.message().contains("preserving existing catalog state"), "{}", error.message());
    // The message names the source that actually failed, so the log says what to fix.
    assert!(error.message().contains(NAGAMI_MAPPING_URL), "{}", error.message());
    assert_eq!(
        web.urls(),
        [
            super::primary_index_url(),
            super::english_index_url(),
            NAGAMI_MAPPING_URL.to_owned()
        ]
    );
    assert!(harness.character_metadata("javelin").await.0);
    let model = harness.model(model_id).await;
    assert!(model.active);
    assert_eq!(model.source, "merged");
    assert_eq!(model.fallback_url, fallback_url);
    assert_eq!(model.source_metadata, source_metadata);
    assert_eq!(model.model_type, "live2d");
}
