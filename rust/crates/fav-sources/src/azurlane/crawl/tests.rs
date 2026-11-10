//! `tests/test_azurlane.py`.
//!
//! Python's tests run `AzurLane.update()` over httpx mock transports and a fake database
//! that answers the crawler's SQL by pattern. These run the real crawler over a scripted web
//! (an HTTP proxy, with the crawler sending `https://` URLs as `http://`) and a test
//! PostgreSQL database, and check the same outcomes. The fake database never reports an
//! asset in retry cooldown; PostgreSQL does, so the tests that play out a lapsed wait lapse
//! the asset's wait along with the model's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fav_core::settings::Settings;
use fav_db::Db;
use fav_db::testdb::TestDatabase;
use fav_net::retry::RecordingSleeper;
use fav_net::{ClientOptions, HttpClient};
use fav_runtime::source::JobError;
use serde_json::{Value as Json, json};
use sqlx::Row as _;

use super::{AzurLaneCrawler, Clients, Pacing, asset_client_options};
use crate::azurlane::l2d_sources::{
    L2D_SU_ENGLISH_REGION, L2D_SU_PRIMARY_REGION, L2D_SU_STATIC_BASE_URL, NAGAMI_MAPPING_URL, l2d_su_character_fingerprint,
    l2d_su_ship_index_url, parse_l2d_su_ship_index, source_client_options,
};
use crate::testutil::{FakeWeb, Reply, Seen};

mod clients;
mod manifests;
mod origin;
mod update;

// ---------- payloads ----------

fn primary_index_url() -> String {
    l2d_su_ship_index_url(L2D_SU_PRIMARY_REGION)
}

fn english_index_url() -> String {
    l2d_su_ship_index_url(L2D_SU_ENGLISH_REGION)
}

/// `_ship_index_payload(ships, region=...)`.
fn index_payload_for(ships: &[Json], region: &str) -> String {
    json!({"locale": region, "version": "9.7.295", "ships": ships}).to_string()
}

fn index_payload(ships: &[Json]) -> String {
    index_payload_for(ships, L2D_SU_PRIMARY_REGION)
}

/// `_ship(ship_group_id, resource_key, name, skins=...)`.
fn ship(ship_group_id: i64, resource_key: &str, name: &str, skins: &[Json]) -> Json {
    json!({
        "shipGroupId": ship_group_id,
        "name": name,
        "englishName": name,
        "nationName": "Royal Navy",
        "typeName": "Destroyer",
        "rarityName": "SR",
        "resourceKey": resource_key,
        "skins": skins,
    })
}

/// `_skin(skin_id, name, key=..., kind=..., face_ids=..., icons=...)`.
fn skin_with(skin_id: i64, name: &str, key: &str, kind: &str, face_ids: Option<&[&str]>, icons: bool) -> Json {
    let mut payload = json!({
        "id": skin_id,
        "name": name,
        "skinTypeName": "Skin",
        "featureTags": [if kind == "spine" { "spine" } else { "Live2D" }],
        "painting": key,
        "prefab": key,
        "dynamicType": kind,
        "isLive2d": kind == "live2d",
        "isLive2dPlus": false,
        "isDynamic": true,
        "isSpine": kind == "spine",
    });
    if let Some(face_ids) = face_ids {
        payload["paintingFaceIds"] = json!(face_ids);
    }
    if icons {
        payload["assetPaths"] = json!({
            "squareIcon": format!("squareicon/{key}"),
            "shipyardIcon": format!("shipyardicon/{key}"),
            "qIcon": format!("qicon/{key}"),
        });
    }
    payload
}

fn skin(skin_id: i64, name: &str, key: &str) -> Json {
    skin_with(skin_id, name, key, "live2d", None, false)
}

fn spine_skin(skin_id: i64, name: &str, key: &str) -> Json {
    skin_with(skin_id, name, key, "spine", None, false)
}

/// The one-ship, one-skin Javelin catalog most tests crawl.
fn javelin_catalog() -> String {
    index_payload(&[ship(1, "javelin", "Javelin", &[skin(1, "Default", "javelin")])])
}

fn live2d_url(key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/live2d/{key}/{key}.model3.json")
}

fn spine_url(key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/spinepainting/{key}")
}

fn painting_url(key: &str) -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/painting/{key}.webp")
}

fn javelin_moc_url() -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/live2d/javelin/javelin.moc3")
}

fn javelin_texture_url() -> String {
    format!("{L2D_SU_STATIC_BASE_URL}/live2d/javelin/textures/texture_00.webp")
}

/// `_painting_index_payload(painting_key, layers=...)`.
fn painting_index_payload(painting_key: &str, layers: Option<Json>) -> String {
    let mut payload = layers.unwrap_or_else(|| json!({painting_key: {"size": [1020, 992], "rawSize": [1024, 682], "position": [0, 0]}}));
    payload["face"] = json!({"size": [100, 100], "pivot": [0.5, 0.5], "position": [0, 0]});
    payload.to_string()
}

/// `_live2d_model3_payload()`.
fn model3_payload() -> String {
    json!({"Version": 3, "FileReferences": {"Moc": "javelin.moc3", "Textures": ["textures/texture_00.webp"]}}).to_string()
}

fn nagami_mapping(mapping: &Json) -> Reply {
    Reply::bytes(200, mapping.to_string())
}

// ---------- replies ----------

fn ok(body: impl Into<Vec<u8>>, content_type: &str) -> Reply {
    Reply::Respond {
        status: 200,
        headers: vec![("content-type".to_owned(), content_type.to_owned())],
        body: body.into(),
    }
}

fn with_header(reply: Reply, name: &str, value: &str) -> Reply {
    match reply {
        Reply::Respond { status, mut headers, body } => {
            headers.push((name.to_owned(), value.to_owned()));
            Reply::Respond { status, headers, body }
        }
        other => other,
    }
}

/// `str.endswith(suffix)`: the fake CDN matches names exactly, as Python's handlers do.
fn has_suffix(text: &str, suffix: &str) -> bool {
    text.ends_with(suffix)
}

fn not_found() -> Reply {
    Reply::status(404)
}

/// `_source_index_response(payload, url=...)`.
fn index_reply(catalog: &str, url: &str) -> Option<Reply> {
    (url == primary_index_url() || url == english_index_url()).then(|| Reply::bytes(200, catalog))
}

/// `_painting_response(url)`: the three files a painting is made of, as the CDN serves them.
fn painting_reply(url: &str) -> Option<Reply> {
    let name = url.strip_prefix(&format!("{L2D_SU_STATIC_BASE_URL}/painting/"))?;
    if name.ends_with("-mesh.obj") {
        return Some(ok("g mesh\nv 0 0 0\n", "application/x-tgif"));
    }
    if has_suffix(name, ".webp") {
        return Some(ok("painting-bytes", "image/webp"));
    }
    if let Some(key) = name.strip_suffix(".json") {
        return Some(ok(painting_index_payload(key, None), "application/json"));
    }
    None
}

/// `_live2d_response(url)`: whatever a Live2D skin needs.
fn live2d_reply(url: &str) -> Option<Reply> {
    if !url.starts_with(&format!("{L2D_SU_STATIC_BASE_URL}/live2d/")) {
        return None;
    }
    if url.ends_with(".model3.json") {
        return Some(ok(model3_payload(), "application/json"));
    }
    Some(ok("live2d-bytes", "application/octet-stream"))
}

/// The index, an empty nagami mapping and the painting files: what every crawl answers first.
fn common_reply(catalog: &str, url: &str) -> Option<Reply> {
    if let Some(reply) = index_reply(catalog, url) {
        return Some(reply);
    }
    if url == NAGAMI_MAPPING_URL {
        return Some(nagami_mapping(&json!({})));
    }
    painting_reply(url)
}

/// The Javelin Live2D files the CDN hosts.
fn javelin_live2d_reply(url: &str) -> Option<Reply> {
    if url == live2d_url("javelin") {
        return Some(ok(model3_payload(), "application/json"));
    }
    if url == javelin_moc_url() {
        return Some(ok("moc-bytes", "application/octet-stream"));
    }
    if url == javelin_texture_url() {
        return Some(ok("webp-bytes", "image/webp"));
    }
    None
}

// ---------- the scripted web ----------

/// A scripted web: answers by method and `https://` URL, recording `"METHOD url"`.
struct Web {
    fake: FakeWeb,
    seen: Arc<Mutex<Vec<(String, String)>>>,
}

impl Web {
    async fn start(answer: impl Fn(&str, &str) -> Reply + Send + Sync + 'static) -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let fake = FakeWeb::start(move |request: &Seen| {
            let url = request
                .url
                .strip_prefix("http://")
                .map_or_else(|| request.url.clone(), |rest| format!("https://{rest}"));
            recorder.lock().expect("lock").push((request.method.clone(), url.clone()));
            answer(&request.method, &url)
        })
        .await;
        Self { fake, seen }
    }

    /// `"METHOD url"`, as the Python tests record them.
    fn requests(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .map(|(method, url)| format!("{method} {url}"))
            .collect()
    }

    fn urls(&self) -> Vec<String> {
        self.seen.lock().expect("lock").iter().map(|(_, url)| url.clone()).collect()
    }

    fn client(&self, options: ClientOptions) -> HttpClient {
        HttpClient::new(&ClientOptions {
            proxy: Some(self.fake.proxy.clone()),
            ..options
        })
        .expect("client")
    }

    /// The clients of a run without an origin proxy: everything through this web.
    fn clients(&self) -> Clients {
        Clients {
            assets: self.client(asset_client_options()),
            source: self.client(source_client_options(Duration::from_secs(30))),
            origin_source: None,
            origin: None,
        }
    }
}

/// `AzurLane(..., api/cdn/origin_request_interval_seconds=0, asset_process_concurrency=1)`.
pub(super) fn test_pacing() -> Pacing {
    Pacing {
        api_request_interval_seconds: 0.0,
        cdn_request_interval_seconds: 0.0,
        origin_request_interval_seconds: 0.0,
        origin_attempts: 3,
        asset_process_concurrency: 1,
    }
}

// ---------- the database ----------

/// A model row as the tests read it back.
#[derive(Debug)]
struct ModelState {
    model_type: String,
    source: String,
    primary_url: String,
    fallback_url: String,
    active: bool,
    completed: bool,
    failed_count: i32,
    last_error: String,
    /// `next_retry_at > NOW()`; `None` for no `next_retry_at`.
    cooling: Option<bool>,
    source_metadata: Json,
}

/// An asset row as the tests read it back.
#[derive(Debug)]
struct AssetState {
    status: String,
    failed_count: i32,
    last_error: String,
    cooling: Option<bool>,
    downloaded_url: String,
}

fn parse_json(text: Option<String>) -> Json {
    text.map_or(Json::Null, |text| serde_json::from_str(&text).expect("JSON column"))
}

struct Harness {
    _database: TestDatabase,
    db: Db,
    tmp: tempfile::TempDir,
}

impl Harness {
    async fn new() -> Self {
        let database = crate::testutil::database().await;
        let db = database.db().clone();
        Self {
            _database: database,
            db,
            tmp: tempfile::tempdir().expect("tempdir"),
        }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root().join(relative)
    }

    fn read(&self, relative: &str) -> Vec<u8> {
        std::fs::read(self.path(relative)).unwrap_or_else(|error| panic!("{relative}: {error}"))
    }

    fn read_json(&self, relative: &str) -> Json {
        serde_json::from_slice(&self.read(relative)).expect("JSON file")
    }

    fn crawler_with(&self, clients: Clients, pacing: Pacing) -> AzurLaneCrawler {
        AzurLaneCrawler::new(&self.root().to_string_lossy(), clients, self.db.clone(), pacing)
            .with_sleeper(RecordingSleeper::new().sleeper())
            .with_jitter(Arc::new(|| 0), Arc::new(|_| 0.0))
            .with_plain_http()
    }

    fn crawler(&self, web: &Web) -> AzurLaneCrawler {
        self.crawler_with(web.clients(), test_pacing())
    }

    /// One `update()` over `web`.
    async fn run(&self, web: &Web) -> Result<(), JobError> {
        self.crawler(web).update().await
    }

    async fn sql(&self, sql: &str) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(self.db.pool())
            .await
            .expect(sql);
    }

    async fn model(&self, model_id: &str) -> ModelState {
        let row = sqlx::query(
            "SELECT model_type, source, primary_url, fallback_url, active, completed_at IS NOT NULL AS completed,
                failed_count, last_error, next_retry_at > NOW() AS cooling, source_metadata::text AS source_metadata
            FROM azurlane_models WHERE model_id = $1",
        )
        .bind(model_id)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or_else(|error| panic!("{model_id}: {error}"));
        ModelState {
            model_type: row.get("model_type"),
            source: row.get("source"),
            primary_url: row.get("primary_url"),
            fallback_url: row.get("fallback_url"),
            active: row.get("active"),
            completed: row.get("completed"),
            failed_count: row.get("failed_count"),
            last_error: row.get("last_error"),
            cooling: row.get("cooling"),
            source_metadata: parse_json(row.get("source_metadata")),
        }
    }

    async fn asset(&self, url: &str) -> AssetState {
        let row = sqlx::query(
            "SELECT status, failed_count, last_error, next_retry_at > NOW() AS cooling, downloaded_url
            FROM azurlane_assets WHERE url = $1",
        )
        .bind(url)
        .fetch_one(self.db.pool())
        .await
        .unwrap_or_else(|error| panic!("{url}: {error}"));
        AssetState {
            status: row.get("status"),
            failed_count: row.get("failed_count"),
            last_error: row.get("last_error"),
            cooling: row.get("cooling"),
            downloaded_url: row.get("downloaded_url"),
        }
    }

    async fn asset_urls(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT url FROM azurlane_assets ORDER BY url")
            .fetch_all(self.db.pool())
            .await
            .expect("asset urls")
    }

    /// The kinds of every model-asset row (`fake_db.model_assets`), sorted.
    async fn model_asset_kinds(&self) -> Vec<String> {
        sqlx::query_scalar("SELECT kind FROM azurlane_model_assets ORDER BY kind")
            .fetch_all(self.db.pool())
            .await
            .expect("model asset kinds")
    }

    async fn character_metadata(&self, key: &str) -> (bool, Json) {
        let row = sqlx::query("SELECT active, source_metadata::text AS source_metadata FROM azurlane_characters WHERE character_key = $1")
            .bind(key)
            .fetch_one(self.db.pool())
            .await
            .expect("character");
        (row.get("active"), parse_json(row.get("source_metadata")))
    }

    async fn ship_detail_payload(&self, ship_group_id: i64) -> Option<(String, Json)> {
        let row = sqlx::query("SELECT fingerprint, payload::text AS payload FROM azurlane_ship_details WHERE ship_group_id = $1")
            .bind(ship_group_id)
            .fetch_optional(self.db.pool())
            .await
            .expect("ship detail")?;
        Some((row.get("fingerprint"), parse_json(row.get("payload"))))
    }

    async fn ship_detail_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM azurlane_ship_details")
            .fetch_one(self.db.pool())
            .await
            .expect("count")
    }

    /// `_seed_ship_detail(fake_db, index_payload, payload=...)`: a stored detail whose
    /// fingerprint matches the index, so `update()` skips the origin fetch.
    async fn seed_ship_detail(&self, index: &str, payload: Option<Json>) {
        let parsed = parse_l2d_su_ship_index(index, L2D_SU_PRIMARY_REGION).expect("index");
        for character in &parsed.characters {
            let ship_id = i64::try_from(character.char_id).expect("ship id");
            let payload = payload
                .clone()
                .unwrap_or_else(|| json!({"ship": {"shipGroupId": ship_id, "skins": []}}));
            sqlx::query(
                "INSERT INTO azurlane_ship_details (ship_group_id, region, fingerprint, payload)
                VALUES ($1, $2, $3, $4::jsonb)",
            )
            .bind(ship_id)
            .bind(L2D_SU_PRIMARY_REGION)
            .bind(l2d_su_character_fingerprint(character))
            .bind(payload.to_string())
            .execute(self.db.pool())
            .await
            .expect("seed ship detail");
        }
    }

    /// `_seed_downloaded_blob(...)`: a blob in the store and a downloaded asset row for it.
    async fn seed_downloaded_blob(&self, url: &str, kind: &str, content: &[u8], content_type: &str) {
        let sha256 = crate::gamekee::sha256_hex(content);
        let relative = format!("_blobs/sha256/{}/{sha256}", &sha256[..2]);
        let path = self.path(&relative);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("blob dir");
        std::fs::write(&path, content).expect("blob");
        let size = i64::try_from(content.len()).expect("size");
        sqlx::query(
            "INSERT INTO azurlane_blobs (sha256, size, content_type, blob_path) VALUES ($1, $2, $3, $4)
            ON CONFLICT DO NOTHING",
        )
        .bind(&sha256)
        .bind(size)
        .bind(content_type)
        .bind(&relative)
        .execute(self.db.pool())
        .await
        .expect("seed blob");
        self.seed_asset(url, kind, "downloaded", &sha256, size, content_type).await;
    }

    async fn seed_asset(&self, url: &str, kind: &str, status: &str, sha256: &str, size: i64, content_type: &str) {
        sqlx::query(
            "INSERT INTO azurlane_assets (url, normalized_url, kind, status, sha256, size, content_type, downloaded_url)
            VALUES ($1, $1, $2, $3, $4, $5, $6, $1)",
        )
        .bind(url)
        .bind(kind)
        .bind(status)
        .bind(sha256)
        .bind(size)
        .bind(content_type)
        .execute(self.db.pool())
        .await
        .expect("seed asset");
    }

    async fn seed_character(&self, key: &str, source_id: Option<i64>, name_zh: &str, name_en: &str, manifest_path: &str, metadata: &Json) {
        sqlx::query(
            "INSERT INTO azurlane_characters (character_key, source_id, name_zh, name_en, manifest_path, active, source_metadata)
            VALUES ($1, $2, $3, $4, $5, TRUE, $6::jsonb)",
        )
        .bind(key)
        .bind(source_id)
        .bind(name_zh)
        .bind(name_en)
        .bind(manifest_path)
        .bind(metadata.to_string())
        .execute(self.db.pool())
        .await
        .expect("seed character");
    }

    /// A model row; the character row it needs is added when missing.
    async fn seed_model(&self, model: &SeedModel<'_>) {
        sqlx::query("INSERT INTO azurlane_characters (character_key) VALUES ($1) ON CONFLICT DO NOTHING")
            .bind(model.character_key)
            .execute(self.db.pool())
            .await
            .expect("seed character");
        sqlx::query(
            "INSERT INTO azurlane_models (
                model_id, model_type, source, character_key, costume_key, costume_id, costume_name_zh,
                costume_name_en, primary_url, fallback_url, active, source_metadata, completed_at
            )
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12::jsonb, CASE WHEN $13 THEN NOW() END)",
        )
        .bind(model.model_id)
        .bind(model.model_type)
        .bind(model.source)
        .bind(model.character_key)
        .bind(model.costume_key)
        .bind(model.costume_id)
        .bind(model.costume_name_zh)
        .bind(model.costume_name_en)
        .bind(model.primary_url)
        .bind(model.fallback_url)
        .bind(model.active)
        .bind(model.source_metadata.to_string())
        .bind(model.completed)
        .execute(self.db.pool())
        .await
        .expect("seed model");
    }

    #[allow(clippy::too_many_arguments)] // One `fake_db.model_assets` row, field for field.
    async fn seed_model_asset(
        &self,
        model_id: &str,
        url: &str,
        kind: &str,
        context_hash: &str,
        local_path: &str,
        original_filename: &str,
        context: &Json,
    ) {
        sqlx::query(
            "INSERT INTO azurlane_model_assets (model_id, url, kind, context_hash, local_path, original_filename, context_json)
            VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(model_id)
        .bind(url)
        .bind(kind)
        .bind(context_hash)
        .bind(local_path)
        .bind(original_filename)
        .bind(context.to_string())
        .execute(self.db.pool())
        .await
        .expect("seed model asset");
    }

    /// Plays out the run after a failed model's wait (and its assets') has lapsed.
    async fn lapse_cooldowns(&self) {
        self.sql(
            "UPDATE azurlane_models SET next_retry_at = NOW() - INTERVAL '1 minute' WHERE next_retry_at IS NOT NULL;
            UPDATE azurlane_assets SET next_retry_at = NOW() - INTERVAL '1 minute' WHERE next_retry_at IS NOT NULL;",
        )
        .await;
    }
}

/// A seeded `fake_db.models[...]` row.
struct SeedModel<'a> {
    model_id: &'a str,
    model_type: &'a str,
    source: &'a str,
    character_key: &'a str,
    costume_key: &'a str,
    costume_id: Option<i64>,
    costume_name_zh: &'a str,
    costume_name_en: &'a str,
    primary_url: &'a str,
    fallback_url: &'a str,
    active: bool,
    completed: bool,
    source_metadata: Json,
}

impl<'a> SeedModel<'a> {
    fn new(model_id: &'a str, model_type: &'a str, character_key: &'a str, primary_url: &'a str) -> Self {
        Self {
            model_id,
            model_type,
            source: "",
            character_key,
            costume_key: "",
            costume_id: None,
            costume_name_zh: "",
            costume_name_en: "",
            primary_url,
            fallback_url: "",
            active: true,
            completed: false,
            source_metadata: json!({}),
        }
    }
}

fn sorted(values: &[&str]) -> Vec<String> {
    let mut values: Vec<String> = values.iter().map(|value| (*value).to_owned()).collect();
    values.sort();
    values
}

fn unique_sorted(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values.dedup();
    values
}

fn unused_db() -> Db {
    Db::connect_lazy("postgresql://tests@127.0.0.1:1/unused").expect("lazy pool")
}

// ---------- configuration and registration ----------

// py: tests/test_azurlane.py::test_azurlane_config_defaults_to_disabled_collection_path
#[test]
fn azurlane_config_defaults_to_disabled_collection_path() {
    let cfg = Settings::default().web.azurlane;
    assert!(!cfg.schedule.enabled);
    assert_eq!(cfg.schedule.cron, "0 */6 * * *");
    assert_eq!(cfg.path.as_str(), "collection/azurlane");
}

// py: tests/test_azurlane.py::test_scheduler_registration_includes_azurlane
#[tokio::test]
async fn scheduler_registration_includes_azurlane() {
    let mut settings = Settings::default();
    "5 1 * * *".clone_into(&mut settings.web.azurlane.schedule.cron);
    settings.web.azurlane.schedule.enabled = false;
    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "azurlane").expect("an azurlane job");
    assert_eq!(job.name, "Azur Lane");
    assert_eq!(job.cron, "5 1 * * *");
    assert!(!job.enabled);
    assert!(job.required_commands.is_empty());
    let db = unused_db();
    let ctx = crate::SourceContext {
        settings: Arc::new(fav_db::SettingsStore::new(db.clone())),
        db,
    };
    assert!(crate::registry(&ctx).contains("azurlane"));
}

// py: tests/test_azurlane.py::test_azurlane_job_runs_without_an_origin_proxy
#[test]
fn azurlane_job_runs_without_an_origin_proxy() {
    let mut settings = Settings::default();
    settings.web.azurlane.schedule.enabled = true;
    let jobs = fav_runtime::jobs::build_jobs(&settings);
    let job = jobs.iter().find(|job| job.key == "azurlane").expect("an azurlane job");
    // The ship data is on the CDN, which answers direct requests, so the proxy is optional.
    assert!(job.missing_fields.is_empty());
    assert!(job.enabled);
}

// py: tests/test_azurlane.py::test_azurlane_connect_host_defaults_to_the_sibling_cdn_hostname
#[test]
fn azurlane_connect_host_defaults_to_the_sibling_cdn_hostname() {
    // l2d.su lost its DNS record while its Cloudflare zone kept serving it; static.l2d.su is
    // in the same zone and still resolves, so the fallback works without any configuration.
    assert_eq!(Settings::default().web.azurlane.origin_connect_host, "static.l2d.su");
}
