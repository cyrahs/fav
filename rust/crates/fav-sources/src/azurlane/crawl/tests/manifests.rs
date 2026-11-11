//! The backend manifests written from the database.

use std::sync::atomic::Ordering;

use serde_json::json;

use super::{Harness, SeedModel, Web, javelin_moc_url, javelin_texture_url, live2d_url, spine_url};

fn title(key: &str) -> String {
    let mut chars = key.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// The crawler for manifest tests: it never goes to the web.
async fn offline_web() -> Web {
    Web::start(|_, _| super::not_found()).await
}

// py: tests/test_azurlane.py::test_azurlane_backend_manifests_are_deterministically_ordered
#[tokio::test]
async fn azurlane_backend_manifests_are_deterministically_ordered() {
    let harness = Harness::new().await;
    let manifest_path = harness.path("javelin - Javelin/manifest.json");
    let live2d_model_id = "azurlane:live2d:javelin:javelin";
    let spine_model_id = "azurlane:spine:javelin:javelin_spine";
    let model3_url = live2d_url("javelin");
    harness
        .seed_character(
            "javelin",
            Some(1),
            "标枪",
            "Javelin",
            &manifest_path.to_string_lossy(),
            &json!({"sources": ["l2d.su"], "model_ids": [spine_model_id, live2d_model_id]}),
        )
        .await;
    let spine_primary = spine_url("javelin_spine");
    harness
        .seed_model(&SeedModel {
            source: "l2d.su",
            costume_key: "javelin_spine",
            costume_id: Some(2),
            costume_name_zh: "动态",
            costume_name_en: "Dynamic",
            source_metadata: json!({"id": spine_model_id}),
            ..SeedModel::new(spine_model_id, "spine", "javelin", &spine_primary)
        })
        .await;
    harness
        .seed_model(&SeedModel {
            source: "l2d.su",
            costume_key: "javelin",
            costume_id: Some(1),
            costume_name_zh: "默认",
            costume_name_en: "Default",
            completed: true,
            source_metadata: json!({"id": live2d_model_id}),
            ..SeedModel::new(live2d_model_id, "live2d", "javelin", &model3_url)
        })
        .await;
    for (url, kind, local_path, content) in [
        (
            javelin_texture_url(),
            "live2d.texture",
            "assets/live2d/javelin/textures/texture_00.webp",
            &b"texture"[..],
        ),
        (javelin_moc_url(), "live2d.moc3", "assets/live2d/javelin/javelin.moc3", &b"moc"[..]),
        (
            model3_url.clone(),
            "live2d.model3",
            "assets/live2d/javelin/javelin.model3.json",
            &b"model3"[..],
        ),
    ] {
        let sha256 = crate::gamekee::sha256_hex(content);
        let size = i64::try_from(content.len()).expect("size");
        harness
            .seed_asset(&url, kind, "downloaded", &sha256, size, "application/octet-stream")
            .await;
        let filename = local_path.rsplit('/').next().expect("name");
        harness
            .seed_model_asset(
                live2d_model_id,
                &url,
                kind,
                &format!("hash-{kind}"),
                local_path,
                filename,
                &json!({"kind": kind, "model_id": live2d_model_id}),
            )
            .await;
    }

    let web = offline_web().await;
    let crawler = harness.crawler(&web);
    crawler.write_backend_manifests().await.expect("manifests");
    let first_manifest = std::fs::read_to_string(&manifest_path).expect("manifest");
    crawler.write_backend_manifests().await.expect("manifests");
    let second_manifest = std::fs::read_to_string(&manifest_path).expect("manifest");

    assert_eq!(second_manifest, first_manifest);
    let payload: serde_json::Value = serde_json::from_str(&first_manifest).expect("JSON");
    let models = payload["models"].as_array().expect("models");
    let ids: Vec<&str> = models.iter().map(|model| model["model_id"].as_str().expect("id")).collect();
    assert_eq!(ids, [live2d_model_id, spine_model_id]);
    let assets = models[0]["assets"].as_array().expect("assets");
    let kinds: Vec<&str> = assets.iter().map(|asset| asset["kind"].as_str().expect("kind")).collect();
    assert_eq!(kinds, ["live2d.model3", "live2d.moc3", "live2d.texture"]);
    let paths: Vec<&str> = assets.iter().map(|asset| asset["local_path"].as_str().expect("path")).collect();
    assert_eq!(
        paths,
        [
            "assets/live2d/javelin/javelin.model3.json",
            "assets/live2d/javelin/javelin.moc3",
            "assets/live2d/javelin/textures/texture_00.webp",
        ]
    );
}

// py: tests/test_azurlane.py::test_azurlane_backend_manifests_query_assets_one_character_batch_at_a_time
#[tokio::test]
async fn azurlane_backend_manifests_query_assets_one_character_batch_at_a_time() {
    // Loading every asset row at once took the worker to ~1.4 GiB, so each batch of characters
    // reads only its own models' assets, and every manifest still lists exactly its own.
    let harness = Harness::new().await;
    let keys = ["ayanami", "javelin", "laffey"];
    for (source_id, key) in (1_i64..).zip(keys) {
        let model_id = format!("azurlane:live2d:{key}:{key}");
        let manifest_path = harness.path(&format!("{key}/manifest.json"));
        harness
            .seed_character(key, Some(source_id), "", &title(key), &manifest_path.to_string_lossy(), &json!({}))
            .await;
        let url = live2d_url(key);
        harness
            .seed_model(&SeedModel {
                source: "l2d.su",
                costume_key: key,
                costume_id: Some(source_id),
                ..SeedModel::new(&model_id, "live2d", key, &url)
            })
            .await;
        harness.seed_asset(&url, "live2d.model3", "downloaded", "", 0, "").await;
        harness
            .seed_model_asset(
                &model_id,
                &url,
                "live2d.model3",
                &format!("hash-{key}"),
                &format!("assets/live2d/{key}/{key}.model3.json"),
                &format!("{key}.model3.json"),
                &json!({}),
            )
            .await;
    }

    let web = offline_web().await;
    let crawler = harness.crawler(&web).with_manifest_batch(1);
    crawler.write_backend_manifests().await.expect("manifests");

    assert_eq!(crawler.manifest_asset_queries.load(Ordering::Relaxed), keys.len());
    for key in keys {
        let payload = harness.read_json(&format!("{key}/manifest.json"));
        let models = payload["models"].as_array().expect("models");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0]["model_id"], format!("azurlane:live2d:{key}:{key}"));
        let paths: Vec<&str> = models[0]["assets"]
            .as_array()
            .expect("assets")
            .iter()
            .map(|asset| asset["local_path"].as_str().expect("path"))
            .collect();
        assert_eq!(paths, [format!("assets/live2d/{key}/{key}.model3.json")]);
    }
}
