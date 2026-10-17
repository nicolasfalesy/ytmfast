//! The Explore and Related pages (`fixtures/explore`, hand-made: see `EXPLORE_FIXTURES.md`) through
//! the same `browse` parser every other page uses, plus the Related tab of a song's `next`.
//! Nothing here talks to YouTube.

use serde_json::{Value, json};
use ytmfast::browse::{Kind, Page, parse_browse, parse_related_tab};

fn fixture(dir: &str, name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/{dir}/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn titles(page: &Page) -> Vec<&str> {
    page.sections.iter().map(|s| s.title.as_str()).collect()
}

#[test]
fn explore_buttons_are_page_rows_that_open_with_browse() {
    let page = parse_browse(&fixture("explore", "browse_explore"));
    // The three buttons come first, in a section of their own (they have no shelf title).
    let buttons = &page.sections[0];
    assert_eq!(buttons.title, "");
    let got: Vec<(&str, &str, Kind)> = buttons
        .items
        .iter()
        .map(|r| (r.title.as_str(), r.browse_id.as_str(), r.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("New releases", "FEmusic_new_releases", Kind::Page),
            ("Charts", "FEmusic_charts", Kind::Page),
            ("Moods & genres", "FEmusic_moods_and_genres", Kind::Page),
        ]
    );
    // A button opens a page: it plays nothing and names no song.
    for r in &buttons.items {
        assert!(r.play.is_none(), "{} must not play", r.title);
        assert_eq!(r.video_id, "");
        assert_eq!(r.params, "");
    }
}

#[test]
fn explore_shelves_keep_their_rows_and_show_all_links() {
    let page = parse_browse(&fixture("explore", "browse_explore"));
    assert_eq!(
        titles(&page),
        ["", "New albums & singles", "Trending", "New music videos"]
    );
    let albums = &page.sections[1];
    assert_eq!(albums.items.len(), 2);
    assert_eq!(albums.items[0].kind, Kind::Album);
    assert_eq!(albums.items[0].browse_id, "MPREb_fakeExploreA");
    assert!(albums.items[0].play.is_some(), "an album tile plays");
    let more = albums.more.as_ref().expect("a Show all link");
    assert_eq!(more.browse_id, "FEmusic_new_releases_albums");
    let trending = &page.sections[2];
    assert_eq!(trending.items[0].kind, Kind::Song);
    assert_eq!(trending.items[0].video_id, "fakeTrend01");
    assert_eq!(page.sections[3].items[0].video_id, "fakeVideo01");
}

#[test]
fn moods_and_genres_buttons_keep_their_params() {
    let page = parse_browse(&fixture("explore", "browse_moods"));
    assert_eq!(titles(&page), ["Moods & moments", "Genres"]);
    let chill = &page.sections[0].items[0];
    assert_eq!(chill.title, "Chill");
    assert_eq!(chill.browse_id, "FEmusic_moods_and_genres_category");
    assert_eq!(chill.params, "ggMPOg1uXzFmYWtlQ2hpbGw%3D");
    assert_eq!(chill.kind, Kind::Page);
    assert_eq!(page.sections[1].items[0].title, "Jazz");
}

#[test]
fn the_related_page_lists_songs_playlists_and_artists_but_not_the_about_text() {
    let page = parse_browse(&fixture("explore", "browse_related"));
    assert_eq!(
        titles(&page),
        [
            "You might also like",
            "Recommended playlists",
            "Similar artists"
        ]
    );
    let songs = &page.sections[0].items;
    assert_eq!(songs.len(), 2);
    assert_eq!(songs[0].kind, Kind::Song);
    assert_eq!(songs[0].video_id, "fakeRelat01");
    assert_eq!(page.sections[1].items[0].kind, Kind::Playlist);
    assert_eq!(page.sections[2].items[0].kind, Kind::Artist);
}

/// A `next` answer with the given tab endpoints (the lyrics and related ones are both there in a
/// real answer; each reader picks its own by page type).
fn next_with(tabs: Vec<Value>) -> Value {
    let tabs: Vec<Value> = tabs
        .into_iter()
        .map(|endpoint| json!({"tabRenderer": {"endpoint": endpoint, "title": "Tab"}}))
        .collect();
    json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
        "watchNextTabbedResultsRenderer": {"tabs": tabs}}}}})
}

fn tab(browse_id: &str, page_type: &str) -> Value {
    json!({"browseEndpoint": {"browseId": browse_id, "browseEndpointContextSupportedConfigs": {
        "browseEndpointContextMusicConfig": {"pageType": page_type}}}})
}

#[test]
fn the_related_tab_comes_from_the_real_next_fixture() {
    // `next_song_for_lyrics.json` is a real (scrubbed) answer; its fourth tab is the Related one.
    let next = fixture("browse", "next_song_for_lyrics");
    assert_eq!(parse_related_tab(&next).as_deref(), Some("fake000804"));
}

#[test]
fn the_related_tab_is_found_by_page_type_not_by_position() {
    let next = next_with(vec![
        tab("MPLYtfakeLyrics", "MUSIC_PAGE_TYPE_TRACK_LYRICS"),
        tab("MPTRt_fakeRelated", "MUSIC_PAGE_TYPE_TRACK_RELATED"),
    ]);
    assert_eq!(
        parse_related_tab(&next).as_deref(),
        Some("MPTRt_fakeRelated")
    );
    // The lyrics tab alone is no related tab.
    let only_lyrics = next_with(vec![tab("MPLYtfakeLyrics", "MUSIC_PAGE_TYPE_TRACK_LYRICS")]);
    assert_eq!(parse_related_tab(&only_lyrics), None);
    // No tabs at all, and not an answer at all.
    assert_eq!(parse_related_tab(&next_with(Vec::new())), None);
    assert_eq!(parse_related_tab(&json!({})), None);
}

#[test]
fn a_related_tab_id_that_fails_the_shape_check_is_no_tab() {
    // A browse id with a space or a quote never goes back out to YouTube.
    for bad in ["MPTRt fake", "MPTRt\"x", "x", ""] {
        let next = next_with(vec![tab(bad, "MUSIC_PAGE_TYPE_TRACK_RELATED")]);
        assert_eq!(parse_related_tab(&next), None, "{bad:?}");
    }
}
