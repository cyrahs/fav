use std::sync::Arc;

use fav_core::pyjson::{Dict, Value};
use fav_runtime::notifications::NewNotification;

use super::*;
use crate::testkit::{Answer, FakeServer, RecordingNotifier};

fn titles(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn source(base_url: &str, path: &str, notifier: Arc<RecordingNotifier>) -> StellaSora {
    StellaSora::new(build_client().expect("client"), base_url, path, notifier).with_backoff(Arc::new(|_| Duration::ZERO))
}

fn temp_root() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().to_str().expect("utf-8 path").to_owned();
    (dir, path)
}

// ---------- tests/test_stellasora.py ----------

// py: tests/test_stellasora.py::test_parse_characters_page_extracts_character_links
#[test]
fn parse_characters_page_extracts_character_links() {
    let html = r#"
    <html><body>
      <table id="trekker-table">
        <tr><th>Profile</th><th>Name</th></tr>
        <tr>
          <td><a href="/wiki/File:Foo.png">Foo</a></td>
          <td><a href="/wiki/Freesia" title="Freesia">Freesia</a></td>
          <td><a href="/wiki/Post_Haste" title="Post Haste">Post Haste</a></td>
        </tr>
        <tr>
          <td>...</td>
          <td><span><a href="/wiki/Snowish_Laru" title="Snowish Laru">Snowish Laru</a></span></td>
          <td>...</td>
        </tr>
      </table>
    </body></html>
    "#;

    let characters = parse_characters_page(html, BASE_URL).unwrap();
    assert_eq!(
        characters.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["Freesia", "Snowish Laru"]
    );
    assert_eq!(
        characters.iter().map(|c| c.url.as_str()).collect::<Vec<_>>(),
        [format!("{BASE_URL}/wiki/Freesia"), format!("{BASE_URL}/wiki/Snowish_Laru")]
    );
}

// py: tests/test_stellasora.py::test_parse_character_infobox_images_extracts_profile_and_artwork
#[test]
fn parse_character_infobox_images_extracts_profile_and_artwork() {
    let html = r#"
    <aside class="portable-infobox pi-theme-default">
      <div class="pi-media-collection">
        <figure class="pi-item pi-media pi-image">
          <a href="//static.wikitide.net/stellasorawiki/2/2f/Freesia.png" class="image image-thumbnail" title="Profile Image">
            <img src="//static.wikitide.net/stellasorawiki/thumb/2/2f/Freesia.png/300px-Freesia.png" />
          </a>
        </figure>
        <figure class="pi-item pi-media pi-image">
          <a href="//static.wikitide.net/stellasorawiki/d/d1/Freesia_Artwork.png" class="image image-thumbnail" title="Artwork">
            <img src="//static.wikitide.net/stellasorawiki/thumb/d/d1/Freesia_Artwork.png/300px-Freesia_Artwork.png" />
          </a>
        </figure>
      </div>
    </aside>
    "#;

    let images = parse_character_infobox_images(html, BASE_URL).unwrap();
    let get = |key: &str| images.iter().find(|(title, _)| title == key).map(|(_, url)| url.as_str());
    assert_eq!(
        get("Profile Image"),
        Some("https://static.wikitide.net/stellasorawiki/2/2f/Freesia.png")
    );
    assert_eq!(
        get("Artwork"),
        Some("https://static.wikitide.net/stellasorawiki/d/d1/Freesia_Artwork.png")
    );
}

// py: tests/test_stellasora.py::test_parse_gallery_cg_file_titles_limits_to_cgs_section
#[test]
fn parse_gallery_cg_file_titles_limits_to_cgs_section() {
    let html = r#"
    <div class="mw-heading"><h2 id="Intro">Intro</h2></div>
    <a href="/wiki/File:Intro.png">Intro</a>
    <div class="mw-heading"><h2 id="CGs">CGs</h2></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Disc_Good_Night.png">Disc Good Night</a></li>
      <li><a href="/wiki/File:Story_tales_07_001.png">Story</a></li>
      <li><a href="/wiki/File:Disc_Good_Night.png">duplicate</a></li>
    </ul>
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <a href="/wiki/File:Sprite.png">Sprite</a>
    "#;

    assert_eq!(
        parse_gallery_cg_file_titles(html),
        ["File:Disc_Good_Night.png", "File:Story_tales_07_001.png"]
    );
}

// py: tests/test_stellasora.py::test_parse_gallery_memory_snapshot_file_titles_limits_to_images_section
#[test]
fn parse_gallery_memory_snapshot_file_titles_limits_to_images_section() {
    let html = r#"
    <div class="mw-heading"><h2 id="Images">Images</h2></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_Intro_EN.jpg" title="Foo's English Introduction">Intro</a></li>
      <li><a href="/wiki/File:Foo_Memory_Snapshot.png" title="Memory Snapshot">Memory Snapshot</a></li>
      <li><a href="/wiki/File:Foo_Memory_Snapshot_Old.png" title="Memory Snapshot (Old)">Memory Snapshot (Old)</a></li>
    </ul>
    <div class="mw-heading"><h2 id="CGs">CGs</h2></div>
    <a href="/wiki/File:Disc_Good_Night.png">Disc Good Night</a>
    "#;

    assert_eq!(
        parse_gallery_memory_snapshot_file_titles(html),
        ["File:Foo_Memory_Snapshot.png", "File:Foo_Memory_Snapshot_Old.png"]
    );
}

// py: tests/test_stellasora.py::test_parse_gallery_awakened_file_titles_limits_to_sprites_awakened_subsection
#[test]
fn parse_gallery_awakened_file_titles_limits_to_sprites_awakened_subsection() {
    let html = r#"
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Default">Default</h3></div>
    <a href="/wiki/File:Foo_a_02.png">Default</a>
    <div class="mw-heading mw-heading3"><h3 id="Awakened">Awakened</h3></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_awakened_02.png">Awakened 1</a></li>
      <li><a href="/wiki/File:Foo_awakened_03.png">Awakened 2</a></li>
    </ul>
    <div class="mw-heading mw-heading3"><h3 id="Variant_B">Variant B</h3></div>
    <a href="/wiki/File:Foo_b_02.png">Variant</a>
    <div class="mw-heading"><h2 id="Other">Other</h2></div>
    <a href="/wiki/File:Other.png">Other</a>
    "#;

    assert_eq!(
        parse_gallery_awakened_file_titles(html),
        ["File:Foo_awakened_02.png", "File:Foo_awakened_03.png"]
    );
}

// py: tests/test_stellasora.py::test_parse_gallery_skin_file_titles_limits_to_sprites_skin_subsections
#[test]
fn parse_gallery_skin_file_titles_limits_to_sprites_skin_subsections() {
    let html = r#"
    <div class="mw-heading"><h2 id="Sprites">Sprites</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Default">Default</h3></div>
    <a href="/wiki/File:Foo_a_02.png">Default</a>
    <div class="mw-heading mw-heading3"><h3 id="Skin1">Skin1</h3></div>
    <ul class="gallery">
      <li><a href="/wiki/File:Foo_skin1_02.png">Skin1 1</a></li>
      <li><a href="/wiki/File:Foo_skin1_03.png">Skin1 2</a></li>
    </ul>
    <div class="mw-heading mw-heading3"><h3 id="Variant_B">Variant B</h3></div>
    <a href="/wiki/File:Foo_b_02.png">Variant</a>
    <div class="mw-heading mw-heading3"><h3 id="Skin2">Skin2</h3></div>
    <a href="/wiki/File:Foo_skin2_02.png">Skin2</a>
    <div class="mw-heading"><h2 id="Other">Other</h2></div>
    <div class="mw-heading mw-heading3"><h3 id="Skin3">Skin3</h3></div>
    <a href="/wiki/File:Foo_skin3_02.png">Other</a>
    "#;

    assert_eq!(
        parse_gallery_skin_file_titles(html),
        ["File:Foo_skin1_02.png", "File:Foo_skin1_03.png", "File:Foo_skin2_02.png"]
    );
}

// py: tests/test_stellasora.py::test_parse_list_of_discs_page_file_titles_extracts_disc_images
#[test]
fn parse_list_of_discs_page_file_titles_extracts_disc_images() {
    let html = r#"
    <table id="disc-table">
      <tr>
        <td><a href="/wiki/File:Disc_Good_Night.png">Disc</a></td>
        <td><a href="/wiki/File:BGM_Good_Night.ogg">BGM</a></td>
        <td><a href="/wiki/Element">Aqua</a></td>
      </tr>
    </table>
    "#;

    assert_eq!(parse_list_of_discs_page_file_titles(html).unwrap(), ["File:Disc_Good_Night.png"]);
}

// py: tests/test_stellasora.py::test_dedupe_wiki_file_titles_normalizes_spaces_and_case
#[test]
fn dedupe_wiki_file_titles_normalizes_spaces_and_case() {
    let input = titles(&[
        "File:Disc Good Night.png",
        "file:disc_good_night.png",
        "/wiki/File:Disc_Good_Night.png",
        "https://stellasora.miraheze.org/wiki/File:Disc_Good_Night.png",
        "File:Story_tales_07_001.png",
    ]);

    assert_eq!(
        dedupe_wiki_file_titles(&input).unwrap(),
        ["File:Disc_Good_Night.png", "File:Story_tales_07_001.png"]
    );
}

// py: tests/test_stellasora.py::test_build_download_destination_map_disc_prefers_character_dir
#[test]
fn build_download_destination_map_disc_prefers_character_dir() {
    let (_dir, root) = temp_root();
    let base = pypath::join(&root, "stellasora");
    let character_titles = vec![
        (
            "Freesia".to_owned(),
            titles(&["File:Disc_Good_Night.png", "File:Story_tales_07_001.png"]),
        ),
        ("Chitose".to_owned(), titles(&["File:Disc_Good_Night.png"])),
    ];
    let disc_titles = titles(&["File:Disc_Good_Night.png", "File:Disc_Sunlit_Blossom.png"]);

    let title_to_dir = build_download_destination_map(&base, &disc_titles, &character_titles).unwrap();

    assert_eq!(
        title_to_dir.get("File:Disc_Good_Night.png"),
        Some(pypath::join(&base, "Freesia").as_str())
    );
    assert_eq!(
        title_to_dir.get("File:Disc_Sunlit_Blossom.png"),
        Some(pypath::join(&base, "disc").as_str())
    );
}

// py: tests/test_stellasora.py::test_download_targets_skips_when_path_missing
#[tokio::test]
async fn download_targets_skips_when_path_missing() {
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::html("")).await;
    let missing = pypath::join(&root, "missing");
    let ss = source(&server.base_url, &missing, Arc::default());

    ss.download_targets().await.unwrap();

    assert!(server.received().is_empty(), "the target map is never built");
    assert!(!Path::new(&missing).exists());
}

// ---------- tests/test_stellasora_notifications.py ----------

async fn image_server() -> FakeServer {
    FakeServer::start(|_| Answer::bytes(200, b"img")).await
}

fn foo_destinations(root: &str) -> DestinationMap {
    let mut map = DestinationMap::default();
    map.set_default("File:Foo.png".to_owned(), &pypath::join(root, "disc"));
    map
}

// py: tests/test_stellasora_notifications.py::test_download_resolved_files_enqueues_notification
#[tokio::test]
async fn download_resolved_files_enqueues_notification() {
    let (_dir, root) = temp_root();
    let server = image_server().await;
    let notifier = Arc::new(RecordingNotifier::default());
    let ss = source(&server.base_url, &root, Arc::clone(&notifier));
    let image_url = server.url("/Foo.png");
    let resolved = vec![(
        "File:Foo.png".to_owned(),
        WikiFile {
            title: "File:Foo.png".to_owned(),
            url: image_url.clone(),
            description_url: Some("https://wiki.example.com/Foo".to_owned()),
        },
    )];
    let mut stats = DownloadStats::default();

    ss.download_resolved_files(&resolved, &foo_destinations(&root), &mut ExistingIndex::default(), &mut stats)
        .await
        .unwrap();

    assert_eq!(stats.downloaded, 1);
    let saved = pypath::join(&pypath::join(&root, "disc"), "Foo.png");
    assert_eq!(std::fs::read(&saved).unwrap(), b"img");
    let mut payload = Dict::new();
    payload.insert("saved_path", Value::from(saved.as_str()));
    payload.insert("image_path", Value::from(saved.as_str()));
    assert_eq!(
        notifier.sent(),
        [NewNotification {
            kind: "download_completed".to_owned(),
            source: "stellasora".to_owned(),
            header: "Stella Sora".to_owned(),
            title: "Foo.png".to_owned(),
            body: "disc/Foo.png".to_owned(),
            link_url: "https://wiki.example.com/Foo".to_owned(),
            image_url,
            payload: Some(payload),
            ..NewNotification::default()
        }]
    );
}

// py: tests/test_stellasora_notifications.py::test_download_resolved_files_continues_when_notification_fails
#[tokio::test]
async fn download_resolved_files_continues_when_notification_fails() {
    let (_dir, root) = temp_root();
    let server = image_server().await;
    let ss = source(&server.base_url, &root, Arc::new(RecordingNotifier::failing()));
    let resolved = vec![(
        "File:Foo.png".to_owned(),
        WikiFile {
            title: "File:Foo.png".to_owned(),
            url: server.url("/Foo.png"),
            description_url: None,
        },
    )];
    let mut stats = DownloadStats::default();

    ss.download_resolved_files(&resolved, &foo_destinations(&root), &mut ExistingIndex::default(), &mut stats)
        .await
        .unwrap();

    assert_eq!(stats.downloaded, 1);
}

// py: tests/test_stellasora_notifications.py::test_notify_download_uses_relative_path_from_stellasora_root
#[tokio::test]
async fn notify_download_uses_relative_path_from_stellasora_root() {
    let (_dir, root) = temp_root();
    let notifier = Arc::new(RecordingNotifier::default());
    let ss = source("http://127.0.0.1:9", &root, Arc::clone(&notifier));
    let saved_path = pypath::join(&pypath::join(&root, "Wraith"), "Wraith_awakened_02.png");

    ss.notify_download(
        "File:Wraith_awakened_02.png",
        "https://example.com/Wraith_awakened_02.png",
        &saved_path,
        "",
    )
    .await;

    assert_eq!(notifier.sent()[0].body, "Wraith/Wraith_awakened_02.png");
}

// ---------- beyond the Python tests ----------

#[tokio::test]
async fn a_whole_run_resolves_downloads_and_moves_disc_art_into_character_folders() {
    let (_dir, root) = temp_root();
    std::fs::create_dir_all(pypath::join(&root, "disc")).unwrap();
    // Already in disc/, but now listed in a character's gallery: moved, not fetched.
    std::fs::write(pypath::join(&pypath::join(&root, "disc"), "Disc_Good_Night.png"), b"old").unwrap();
    let server = FakeServer::start(|request| match request.path.as_str() {
        "/wiki/List_of_Discs" => Answer::html(
            r#"<table id="disc-table"><tr><td><a href="/wiki/File:Disc_Good_Night.png">D</a></td>
            <td><a href="/wiki/File:Disc_Sunlit.png">S</a></td></tr></table>"#,
        ),
        "/wiki/Characters" => {
            Answer::html(r#"<table id="trekker-table"><tr><td>p</td><td><a href="/wiki/Freesia">Freesia</a></td><td>x</td></tr></table>"#)
        }
        "/wiki/Freesia/gallery" => Answer::html(
            r#"<div class="mw-heading"><h2 id="CGs">CGs</h2></div><a href="/wiki/File:Disc_Good_Night.png">x</a>
            <div class="mw-heading"><h2 id="Images">Images</h2></div>
            <a href="/wiki/File:Freesia_Memory_Snapshot.png" title="Memory Snapshot">m</a>"#,
        ),
        "/w/api.php" => {
            let titles: Vec<&str> = request.param("titles").unwrap_or_default().split('|').collect();
            let pages: Vec<String> = titles
                .iter()
                .map(|title| {
                    let name = title.trim_start_matches("File:");
                    format!(r#"{{"title":"{title}","imageinfo":[{{"url":"/img/{name}","descriptionurl":"/wiki/{title}"}}]}}"#)
                })
                .collect();
            Answer::json(200, &format!(r#"{{"query":{{"pages":[{}]}}}}"#, pages.join(",")))
        }
        path if path.starts_with("/img/") => Answer::bytes(200, path.as_bytes()),
        _ => Answer::json(404, "{}"),
    })
    .await;
    let notifier = Arc::new(RecordingNotifier::default());
    let ss = source(&server.base_url, &root, Arc::clone(&notifier));

    ss.download_targets().await.unwrap();

    let freesia = pypath::join(&root, "Freesia");
    assert_eq!(std::fs::read(pypath::join(&freesia, "Disc_Good_Night.png")).unwrap(), b"old");
    assert!(!Path::new(&pypath::join(&pypath::join(&root, "disc"), "Disc_Good_Night.png")).exists());
    assert_eq!(
        std::fs::read(pypath::join(&freesia, "Freesia_Memory_Snapshot.png")).unwrap(),
        b"/img/Freesia_Memory_Snapshot.png"
    );
    assert_eq!(
        std::fs::read(pypath::join(&pypath::join(&root, "disc"), "Disc_Sunlit.png")).unwrap(),
        b"/img/Disc_Sunlit.png"
    );
    let bodies: Vec<String> = notifier.sent().into_iter().map(|sent| sent.body).collect();
    assert_eq!(bodies.len(), 2);
    assert!(bodies.contains(&"Freesia/Freesia_Memory_Snapshot.png".to_owned()));
    assert!(bodies.contains(&"disc/Disc_Sunlit.png".to_owned()));
}

#[tokio::test]
async fn a_download_that_keeps_failing_ends_the_run_and_leaves_no_part_file() {
    let (_dir, root) = temp_root();
    let server = FakeServer::start(|_| Answer::bytes(503, b"busy")).await;
    let ss = source(&server.base_url, &root, Arc::default());
    let resolved = vec![(
        "File:Foo.png".to_owned(),
        WikiFile {
            title: "File:Foo.png".to_owned(),
            url: server.url("/Foo.png"),
            description_url: None,
        },
    )];

    let error = ss
        .download_resolved_files(
            &resolved,
            &foo_destinations(&root),
            &mut ExistingIndex::default(),
            &mut DownloadStats::default(),
        )
        .await
        .unwrap_err();

    assert_eq!(error.class_name(), "RuntimeError");
    assert_eq!(error.message(), "Failed to download after 3 attempts: Foo.png");
    assert_eq!(server.received().len(), 3);
    let disc = pypath::join(&root, "disc");
    assert!(!Path::new(&pypath::join(&disc, "Foo.png")).exists());
}

// ---------- goldens from the Python module ----------

use fav_core::golden;

/// `{"result": ...}` or `{"error": [class, message]}` from the golden, as a comparable value.
fn outcome<T>(result: Result<T, (String, String)>, to_value: impl Fn(T) -> Value) -> Value {
    let mut dict = Dict::new();
    match result {
        Ok(value) => dict.insert("result", to_value(value)),
        Err((class, message)) => dict.insert("error", Value::List(vec![Value::from(class), Value::from(message)])),
    }
    Value::Dict(dict)
}

fn str_list(items: &[String]) -> Value {
    Value::List(items.iter().map(|item| Value::from(item.as_str())).collect())
}

fn strings(value: &Value) -> Vec<String> {
    golden::list(value).iter().map(golden::string).collect()
}

fn url_error(error: &UrlError) -> (String, String) {
    ("ValueError".to_owned(), error.to_string())
}

fn job_error(error: &JobError) -> (String, String) {
    (error.class_name().to_owned(), error.message().to_owned())
}

#[test]
fn title_normalization_matches_python() {
    let document = golden::load("stellasora/titles.json");
    for case in golden::cases(&document) {
        let title = golden::string(golden::get(case, "title"));
        let normalized = outcome(normalize_wiki_file_title(&title).map_err(|error| url_error(&error)), Value::from);
        assert_eq!(&normalized, golden::get(case, "normalized"), "{title:?}");
        let local = outcome(
            safe_local_filename_from_wiki_title(&title).map_err(|error| url_error(&error)),
            Value::from,
        );
        assert_eq!(&local, golden::get(case, "local_name"), "{title:?}");
    }
}

#[test]
fn dedupe_matches_python() {
    let document = golden::load("stellasora/dedupe.json");
    for case in golden::cases(&document) {
        let input = strings(golden::get(case, "titles"));
        assert_eq!(
            dedupe_wiki_file_titles(&input).unwrap(),
            strings(golden::get(case, "result")),
            "{input:?}"
        );
    }
}

#[test]
fn destination_map_matches_python() {
    let document = golden::load("stellasora/destinations.json");
    for case in golden::cases(&document) {
        let discs = strings(golden::get(case, "disc_titles"));
        let characters: Vec<(String, Vec<String>)> = golden::list(golden::get(case, "character_titles"))
            .iter()
            .map(|pair| {
                let pair = golden::list(pair);
                (golden::string(&pair[0]), strings(&pair[1]))
            })
            .collect();
        let map = build_download_destination_map("/data/stellasora", &discs, &characters).unwrap();
        let actual: Vec<Vec<String>> = map.iter().map(|(title, dir)| vec![title.to_owned(), dir.to_owned()]).collect();
        let expected: Vec<Vec<String>> = golden::list(golden::get(case, "result")).iter().map(strings).collect();
        assert_eq!(actual, expected);
    }
}

#[test]
fn page_parsers_match_python() {
    let document = golden::load("stellasora/pages.json");
    for case in golden::cases(&document) {
        let html = golden::string(golden::get(case, "html"));
        let characters = outcome(
            parse_characters_page(&html, BASE_URL).map_err(|error| url_error(&error)),
            |characters| {
                Value::List(
                    characters
                        .into_iter()
                        .map(|c| Value::List(vec![Value::from(c.name), Value::from(c.url)]))
                        .collect(),
                )
            },
        );
        assert_eq!(&characters, golden::get(case, "characters"), "characters of {html}");
        let infobox = outcome(
            parse_character_infobox_images(&html, BASE_URL).map_err(|error| url_error(&error)),
            |images| {
                Value::List(
                    images
                        .into_iter()
                        .map(|(k, v)| Value::List(vec![Value::from(k), Value::from(v)]))
                        .collect(),
                )
            },
        );
        assert_eq!(&infobox, golden::get(case, "infobox"), "infobox of {html}");
        assert_eq!(
            str_list(&parse_gallery_cg_file_titles(&html)),
            *golden::get(case, "cgs"),
            "cgs of {html}"
        );
        assert_eq!(
            str_list(&parse_gallery_images_file_titles(&html)),
            *golden::get(case, "images"),
            "images of {html}"
        );
        assert_eq!(
            str_list(&parse_gallery_memory_snapshot_file_titles(&html)),
            *golden::get(case, "memory_snapshots"),
            "memory snapshots of {html}"
        );
        assert_eq!(
            str_list(&parse_gallery_awakened_file_titles(&html)),
            *golden::get(case, "awakened"),
            "awakened of {html}"
        );
        assert_eq!(
            str_list(&parse_gallery_skin_file_titles(&html)),
            *golden::get(case, "skins"),
            "skins of {html}"
        );
        let discs = outcome(
            parse_list_of_discs_page_file_titles(&html).map_err(|error| url_error(&error)),
            |titles| str_list(&titles),
        );
        assert_eq!(&discs, golden::get(case, "discs"), "discs of {html}");
    }
}

#[test]
fn imageinfo_answers_read_like_python() {
    let document = golden::load("stellasora/imageinfo.json");
    for case in golden::cases(&document) {
        let mut expected = case.as_dict().expect("a case").clone();
        let payload = expected.remove("payload").expect("a payload");
        let actual = outcome(parse_imageinfo_payload(&payload).map_err(|error| job_error(&error)), |files| {
            Value::List(
                files
                    .into_iter()
                    .map(|file| {
                        Value::List(vec![
                            Value::from(file.title),
                            Value::from(file.url),
                            file.description_url.map_or(Value::Null, Value::from),
                        ])
                    })
                    .collect(),
            )
        });
        assert_eq!(actual, Value::Dict(expected), "{payload:?}");
    }
}
