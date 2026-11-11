use super::{RuntimeSeriesSeed, extract_hanime1_playlist_blocks, extract_hanime1_series_ids, is_parser_incompatible, parser_incompatible};

#[test]
fn playlist_blocks_close_at_their_matching_div() {
    let page = r#"<div id="video-playlist-wrapper"><div><a href="/watch?v=1">a</a></div></div><div>tail</div>"#;
    assert_eq!(
        extract_hanime1_playlist_blocks(page),
        vec![r#"<div id="video-playlist-wrapper"><div><a href="/watch?v=1">a</a></div></div>"#.to_owned()]
    );
    assert!(extract_hanime1_playlist_blocks(r#"<div class="video-playlist-wrapper"><div>"#).is_empty());
}

#[test]
fn series_ids_read_escaped_links() {
    let html = r#"<a href="https:\/\/hanime1.me\/watch?v=0012&list=1"></a><a href="/watch?v=12"></a><a href="watch?v=0"></a>"#;
    assert_eq!(extract_hanime1_series_ids(html).unwrap(), vec!["12".to_owned()]);
}

#[test]
fn seed_label_and_parser_error() {
    let seed = RuntimeSeriesSeed {
        video_id: "1".into(),
        title: "戀愛".into(),
    };
    assert_eq!(seed.label().unwrap(), "恋爱 {id-1}");
    let error = parser_incompatible("x");
    assert!(is_parser_incompatible(&error));
    assert_eq!(error.notification_dedupe_key(), Some("hanime1:parser:playlist-wrapper"));
}
