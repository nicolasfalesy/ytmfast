//! The `browse` module against the widget's `Page.js`: every scrubbed fixture in `fixtures/browse` must
//! give exactly what `Page.js` gave for it (`fixtures/browse/expected`, made by `golden.mjs`; see
//! `BROWSE_FIXTURES.md`), plus hand-made answers for the rules the fixtures don't reach.

use serde_json::{Value, json};
use ytmfast::browse::{
    Endpoint, LikeStatus, parse_browse, parse_like_for, parse_like_status, parse_lyrics,
    parse_lyrics_tab, parse_more, parse_search,
};

fn fixture(name: &str) -> Value {
    let path = format!(
        "{}/tests/fixtures/browse/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn expected(name: &str) -> Value {
    let mut v = fixture(&format!("expected/{name}"));
    normalise(&mut v);
    v
}

/// Two places where ytmfast's output is knowingly not byte-for-byte `Page.js`, put right in the expected
/// side only (everything else must match as it is):
///
/// - `play`: `Page.js` passed YouTube's whole endpoint object through (`playerParams`,
///   `playlistSetVideoId`, `watchEndpointMusicSupportedConfigs`, ...). ytmfast keeps only the fields
///   the widget sends back to play (`videoId`, `playlistId`, `index`, `params`; `playlistId` and
///   `params` for a playlist), so no raw YouTube JSON reaches a client. The widget reads nothing else.
/// - Sections: `Page.js` built rows outside any shelf (search's loose rows) into a section with only
///   `title` and `items`. ytmfast always sends `cont: ""` and `more: null` too, the shape every other
///   section has. The widget only tests them for truth (`v.cont || ""`, `!!it.more`), and an absent
///   key and an empty one are both false there.
fn normalise(v: &mut Value) {
    match v {
        Value::Array(list) => list.iter_mut().for_each(normalise),
        Value::Object(map) => {
            if let Some(Value::Object(play)) = map.get_mut("play") {
                for (kind, keep) in [
                    (
                        "watchEndpoint",
                        &["videoId", "playlistId", "index", "params"][..],
                    ),
                    ("watchPlaylistEndpoint", &["playlistId", "params"][..]),
                ] {
                    if let Some(Value::Object(ep)) = play.get_mut(kind) {
                        ep.retain(|k, _| keep.contains(&k.as_str()));
                    }
                }
            }
            if map.contains_key("items") && map.contains_key("title") {
                map.entry("cont").or_insert(json!(""));
                map.entry("more").or_insert(Value::Null);
            }
            map.values_mut().for_each(normalise);
        }
        _ => {}
    }
}

fn to_value<T: serde::Serialize>(t: &T) -> Value {
    serde_json::to_value(t).unwrap()
}

#[test]
fn matches_page_js_golden() {
    let mut cases: Vec<(&str, Value)> = Vec::new();
    for name in [
        "browse_home",
        "browse_library_landing",
        "browse_liked_playlists",
        "browse_liked_albums",
        "browse_library_corpus_track_artists",
        "browse_playlist",
        "browse_album",
        "browse_artist",
        "browse_podcast",
    ] {
        cases.push((name, to_value(&parse_browse(&fixture(name)))));
    }
    for name in [
        "browse_home_cont",
        "browse_liked_playlists_cont",
        "browse_library_corpus_track_artists_cont",
        "browse_playlist_cont",
        "search_songs_cont",
    ] {
        cases.push((name, to_value(&parse_more(&fixture(name)))));
    }
    cases.push((
        "search_mixed",
        to_value(&parse_search(&fixture("search_mixed"), false)),
    ));
    for name in ["search_songs", "search_albums", "search_podcasts"] {
        cases.push((name, to_value(&parse_search(&fixture(name), true))));
    }
    let tab = parse_lyrics_tab(&fixture("next_song_for_lyrics"));
    assert!(tab.as_deref().is_some_and(|t| t.starts_with("MPLYt")));
    let lyrics = match parse_lyrics(&fixture("browse_lyrics")) {
        Some((text, source)) => json!({"text": text, "source": source}),
        None => json!({"none": true}),
    };
    cases.push(("lyrics", lyrics));

    assert_eq!(cases.len(), 19);
    let mut wrong = Vec::new();
    for (name, got) in &cases {
        let want = expected(name);
        if *got != want {
            // Both sides are scrubbed fixtures, so printing them leaks nothing.
            eprintln!(
                "{name}:\n got  {}\n want {}",
                serde_json::to_string(got).unwrap(),
                serde_json::to_string(&want).unwrap()
            );
            wrong.push(*name);
        }
    }
    assert!(wrong.is_empty(), "differs from Page.js: {wrong:?}");
}

fn two_row(title: &str, nav: Value) -> Value {
    json!({"musicTwoRowItemRenderer": {
        "title": {"runs": [{"text": title}]},
        "navigationEndpoint": nav,
    }})
}

fn list_row(title: &str, video: &str, set: Option<&str>) -> Value {
    let mut data = json!({"videoId": video});
    if let Some(set) = set {
        data["playlistSetVideoId"] = json!(set);
    }
    json!({"musicResponsiveListItemRenderer": {
        "flexColumns": [
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": title}]}}},
            {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Artist"}]}}},
        ],
        "fixedColumns": [
            {"musicResponsiveListItemFixedColumnRenderer": {"text": {"simpleText": "3:21"}}},
        ],
        "playlistItemData": data,
    }})
}

fn shelf(title: &str, contents: Vec<Value>) -> Value {
    json!({"musicShelfRenderer": {"title": {"runs": [{"text": title}]}, "contents": contents}})
}

fn single_column(sections: Vec<Value>) -> Value {
    json!({"contents": {"singleColumnBrowseResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
        "sectionListRenderer": {"contents": sections}
    }}}]}}})
}

#[test]
fn empty_and_nothing_playable_pages() {
    // Nothing at all, and things that are not even objects: an empty page, never a panic.
    for answer in [json!({}), json!(null), json!([]), json!("x"), json!(7)] {
        let page = parse_browse(&answer);
        assert_eq!(
            to_value(&page),
            json!({"header": {"title": "", "subtitle": "", "thumb": "", "play": null},
                   "sections": [], "cont": ""})
        );
        assert_eq!(
            to_value(&parse_search(&answer, false)),
            json!({"sections": [], "chips": []})
        );
        assert_eq!(
            to_value(&parse_more(&answer)),
            json!({"items": [], "sections": [], "cont": ""})
        );
        assert_eq!(parse_lyrics_tab(&answer), None);
        assert_eq!(parse_lyrics(&answer), None);
        assert_eq!(parse_like_status(&answer), None);
    }

    // A library grid holding only a "New playlist" tile (it opens a dialog: no browse, no watch)
    // and an untitled song: no rows, so no section either.
    let answer = single_column(vec![json!({"gridRenderer": {
        "header": {"gridHeaderRenderer": {"title": {"runs": [{"text": "Playlists"}]}}},
        "items": [
            two_row("New playlist", json!({"createPlaylistEndpoint": {}})),
            two_row("", json!({"watchEndpoint": {"videoId": "abcdefghijk"}})),
        ],
    }})]);
    let page = parse_browse(&answer);
    assert!(page.sections.is_empty());
    assert_eq!(page.cont, "");

    // A page with a header and nothing playable under it keeps the header.
    let mut answer = single_column(vec![]);
    answer["header"] = json!({"musicResponsiveHeaderRenderer": {
        "title": {"runs": [{"text": "Empty list"}]},
        "subtitle": {"runs": [{"text": "Playlist"}]},
    }});
    let page = parse_browse(&answer);
    assert_eq!(page.header.title, "Empty list");
    assert_eq!(page.header.subtitle, "Playlist");
    assert!(page.header.play.is_none());
    assert!(page.sections.is_empty());

    // Lyrics: a next answer with no lyrics tab, and a lyrics page with no text.
    let next = json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
        "watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"endpoint": {"browseEndpoint": {
            "browseId": "MPTRtfake01",
            "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {
                "pageType": "MUSIC_PAGE_TYPE_TRACK_RELATED"}}}}}}]}}}}});
    assert_eq!(parse_lyrics_tab(&next), None);
    let lyrics = json!({"contents": {"sectionListRenderer": {"contents": [
        {"musicDescriptionShelfRenderer": {"description": {"runs": []}}}]}}});
    assert_eq!(parse_lyrics(&lyrics), None);
}

#[test]
fn no_suggestions_or_reload_continuations() {
    // A playlist page: two columns. The section list's own next page there is "Suggestions" (songs that
    // are not in the playlist, and it never ends), so the page has no next page. The playlist shelf's
    // own token is the real one.
    let answer = json!({"contents": {"twoColumnBrowseResultsRenderer": {
        "secondaryContents": {"sectionListRenderer": {
            "contents": [{"musicPlaylistShelfRenderer": {"contents": [
                list_row("Song", "abcdefghijk", Some("SET1")),
                {"continuationItemRenderer": {"continuationEndpoint": {
                    "continuationCommand": {"token": "shelfToken"}}}},
            ]}}],
            "continuations": [{"nextContinuationData": {"continuation": "suggestionsToken"}}],
        }},
    }}});
    let page = parse_browse(&answer);
    assert_eq!(page.cont, "");
    assert_eq!(page.sections.len(), 1);
    assert_eq!(page.sections[0].cont, "shelfToken");

    // reloadContinuationData is a sort menu or a filter chip, not a next page.
    let answer = single_column(vec![json!({"musicShelfRenderer": {
        "contents": [list_row("Song", "abcdefghijk", None)],
        "continuations": [{"reloadContinuationData": {"continuation": "reloadToken"}}],
    }})]);
    let page = parse_browse(&answer);
    assert_eq!(page.sections[0].cont, "");

    // The one-column section list's next page (Home) is followed, in the older shape...
    let mut answer = single_column(vec![shelf(
        "Shelf",
        vec![list_row("Song", "abcdefghijk", None)],
    )]);
    answer["contents"]["singleColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["sectionListRenderer"]["continuations"] =
        json!([{"nextContinuationData": {"continuation": "homeToken"}}]);
    assert_eq!(parse_browse(&answer).cont, "homeToken");

    // ...and in the newer command-executor form.
    let answer = json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {
    "continuationItems": [
        list_row("Song", "abcdefghijk", None),
        {"continuationItemRenderer": {"continuationEndpoint": {"commandExecutorCommand": {
            "commands": [{"somethingElse": {}}, {"continuationCommand": {"token": "nextToken"}}]}}}},
    ]}}]});
    let more = parse_more(&answer);
    assert_eq!(more.items.len(), 1);
    assert_eq!(more.cont, "nextToken");

    // The last page: rows, and no token.
    let answer = json!({"continuationContents": {"musicShelfContinuation": {
        "contents": [list_row("Last", "abcdefghijk", None)],
    }}});
    let more = parse_more(&answer);
    assert_eq!(more.items.len(), 1);
    assert_eq!(more.cont, "");
}

fn thumb_row(thumbs: Value) -> Value {
    json!({"musicTwoRowItemRenderer": {
        "title": {"runs": [{"text": "Album"}]},
        "thumbnailRenderer": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": thumbs}}},
        "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_abc"}},
    }})
}

fn first_thumb(thumbs: Value) -> String {
    let page = parse_browse(&single_column(vec![json!({"gridRenderer": {
        "items": [thumb_row(thumbs)]}})]));
    page.sections[0].items[0].thumb.clone()
}

#[test]
fn thumbnail_rules() {
    // Near 120 px: the last one at most 226 px wide.
    let t = first_thumb(json!([
        {"url": "https://lh3.googleusercontent.com/a=w60", "width": 60},
        {"url": "https://lh3.googleusercontent.com/a=w120", "width": 120},
        {"url": "https://lh3.googleusercontent.com/a=w226", "width": 226},
        {"url": "https://lh3.googleusercontent.com/a=w544", "width": 544},
    ]));
    assert_eq!(t, "https://lh3.googleusercontent.com/a=w226");
    // All too big: the first one.
    let t = first_thumb(json!([
        {"url": "https://lh3.googleusercontent.com/a=w544", "width": 544},
        {"url": "https://lh3.googleusercontent.com/a=w1200", "width": 1200},
    ]));
    assert_eq!(t, "https://lh3.googleusercontent.com/a=w544");
    // Protocol-relative links become https.
    let t = first_thumb(json!([{"url": "//yt3.ggpht.com/abc=s120", "width": 120}]));
    assert_eq!(t, "https://yt3.ggpht.com/abc=s120");
    // A plain video still (4:3 with bars) becomes the 16:9 one.
    let t = first_thumb(
        json!([{"url": "https://i.ytimg.com/vi/abcdefghijk/hqdefault.jpg", "width": 120}]),
    );
    assert_eq!(t, "https://i.ytimg.com/vi/abcdefghijk/mqdefault.jpg");
    let t = first_thumb(
        json!([{"url": "https://i.ytimg.com/vi/abcdefghijk/sddefault.jpg?v=1", "width": 120}]),
    );
    assert_eq!(t, "https://i.ytimg.com/vi/abcdefghijk/mqdefault.jpg");
    // Search's cropped stills (sqp=) are already bar-free and stay.
    let crop = "https://i.ytimg.com/vi/abcdefghijk/hqdefault.jpg?sqp=abc&rs=def";
    assert_eq!(first_thumb(json!([{"url": crop, "width": 120}])), crop);
    // The link sent is the link checked: a backslash (a path separator to the url crate, but not to
    // the widget's QUrl, which would read the host as evil.example) comes out as a slash, and a
    // newline (dropped before the host check) is gone from what is sent too.
    let t = first_thumb(json!([{"url": "https://i.ytimg.com\\@evil.example/a.jpg", "width": 120}]));
    assert_eq!(t, "https://i.ytimg.com/@evil.example/a.jpg");
    let t =
        first_thumb(json!([{"url": "https://lh3.googleusercontent.com/a\n=w120", "width": 120}]));
    assert_eq!(t, "https://lh3.googleusercontent.com/a=w120");
    let t =
        first_thumb(json!([{"url": "//lh3.googleusercontent.com\\@evil.example/a", "width": 120}]));
    assert_eq!(t, "https://lh3.googleusercontent.com/@evil.example/a");
    // Other hosts, plain http and junk are dropped.
    for bad in [
        "https://example.com/a.jpg",
        "http://lh3.googleusercontent.com/a",
        "https://lh3.googleusercontent.com.example.com/a",
        "javascript:alert(1)",
        "not a url",
    ] {
        assert_eq!(
            first_thumb(json!([{"url": bad, "width": 120}])),
            "",
            "{bad}"
        );
    }
    // No pictures at all.
    assert_eq!(first_thumb(json!([])), "");
}

#[test]
fn dedupe_keeps_playlist_repeats_by_set_id() {
    // A playlist holding the same song twice: each entry has its own set id, so both stay.
    let answer = single_column(vec![json!({"musicPlaylistShelfRenderer": {"contents": [
        list_row("Song", "abcdefghijk", Some("SETONE")),
        list_row("Song", "abcdefghijk", Some("SETTWO")),
        list_row("Song", "abcdefghijk", Some("SETTWO")),
    ]}})]);
    let page = parse_browse(&answer);
    let sets: Vec<&str> = page.sections[0]
        .items
        .iter()
        .map(|r| r.set_id.as_str())
        .collect();
    assert_eq!(sets, ["SETONE", "SETTWO"]);

    // Without set ids, the same song twice (two shelves on one page) is listed once.
    let answer = single_column(vec![
        shelf("One", vec![list_row("Song", "abcdefghijk", None)]),
        shelf(
            "Two",
            vec![
                list_row("Song", "abcdefghijk", None),
                list_row("Other", "bcdefghijkl", None),
            ],
        ),
    ]);
    let page = parse_browse(&answer);
    assert_eq!(page.sections.len(), 2);
    assert_eq!(page.sections[0].items.len(), 1);
    assert_eq!(page.sections[1].items.len(), 1);
    assert_eq!(page.sections[1].items[0].title, "Other");
}

fn search_answer(sections: Vec<Value>) -> Value {
    json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
        "sectionListRenderer": {
            "header": {"chipCloudRenderer": {"chips": [
                {"chipCloudChipRenderer": {"text": {"runs": [{"text": "Songs"}]},
                    "navigationEndpoint": {"searchEndpoint": {"params": "songsParams"}}}},
                {"chipCloudChipRenderer": {"text": {"runs": [{"text": "Library"}]},
                    "navigationEndpoint": {"browseEndpoint": {"browseId": "FEmusic_library"}}}},
                {"chipCloudChipRenderer": {"text": {"runs": [{"text": "Bad"}]},
                    "navigationEndpoint": {"searchEndpoint": {"params": "bad params!"}}}},
            ]}},
            "contents": sections,
        }
    }}}]}}})
}

#[test]
fn search_names_untitled_shelves() {
    // The top-result card, an empty titled shelf, then loose rows: the card is "Top result" and the
    // loose rows take the empty shelf's title.
    let answer = search_answer(vec![
        json!({"musicCardShelfRenderer": {
            "title": {"runs": [{"text": "Artist name",
                "navigationEndpoint": {"browseEndpoint": {"browseId": "UCabc"}}}]},
            "subtitle": {"runs": [{"text": "Artist"}]},
            "buttons": [
                {"buttonRenderer": {"command": {"subscribeEndpoint": {}}}},
                {"buttonRenderer": {"command": {"watchPlaylistEndpoint": {
                    "playlistId": "RDabc", "params": "shuffle", "extra": 1}}}},
            ],
        }}),
        json!({"itemSectionRenderer": {"contents": [
            {"musicShelfRenderer": {"title": {"runs": [{"text": "Songs"}]}, "contents": []}},
        ]}}),
        json!({"itemSectionRenderer": {"contents": [list_row("Loose", "abcdefghijk", None)]}}),
    ]);
    let s = parse_search(&answer, false);
    let titles: Vec<&str> = s.sections.iter().map(|x| x.title.as_str()).collect();
    assert_eq!(titles, ["Top result", "Songs"]);
    let card = &s.sections[0].items[0];
    assert_eq!(card.title, "Artist name");
    assert_eq!(card.browse_id, "UCabc");
    assert_eq!(
        to_value(&card.play),
        json!({"watchPlaylistEndpoint": {"playlistId": "RDabc", "params": "shuffle"}})
    );
    assert_eq!(card.playlist_id, "RDabc");
    assert_eq!(to_value(&card.kind), json!("artist"));
    // Only search-filter chips with a well-formed params count.
    assert_eq!(
        to_value(&s.chips),
        json!([{"label": "Songs", "params": "songsParams"}])
    );

    // An untitled first shelf (podcasts get no card) is the top result; untitled loose rows after it
    // are the rest.
    let answer = search_answer(vec![
        json!({"musicShelfRenderer": {"contents": [list_row("First", "abcdefghijk", None)]}}),
        json!({"itemSectionRenderer": {"contents": [list_row("Loose", "bcdefghijkl", None)]}}),
    ]);
    let titles: Vec<String> = parse_search(&answer, false)
        .sections
        .into_iter()
        .map(|x| x.title)
        .collect();
    assert_eq!(titles, ["Top result", "More results"]);

    // One section alone stays untitled.
    let answer = search_answer(vec![json!({"musicShelfRenderer": {
        "contents": [list_row("Only", "abcdefghijk", None)]}})]);
    assert_eq!(parse_search(&answer, true).sections[0].title, "");

    // Mixed search keeps 30 rows a section, a filtered one 300.
    let rows: Vec<Value> = (0..40)
        .map(|i| list_row(&format!("Song {i}"), &format!("abcdefghi{i:02}"), None))
        .collect();
    let answer = search_answer(vec![json!({"musicShelfRenderer": {"contents": rows}})]);
    assert_eq!(parse_search(&answer, false).sections[0].items.len(), 30);
    assert_eq!(parse_search(&answer, true).sections[0].items.len(), 40);
}

#[test]
fn album_rows_take_header_art() {
    let art = "https://lh3.googleusercontent.com/cover=w226";
    let own = "https://lh3.googleusercontent.com/own=w120";
    let mut with_art = list_row("Has art", "bcdefghijkl", None);
    with_art["musicResponsiveListItemRenderer"]["thumbnail"] = json!({"musicThumbnailRenderer": {
        "thumbnail": {"thumbnails": [{"url": own, "width": 120}]}}});
    let mut answer = json!({"contents": {"twoColumnBrowseResultsRenderer": {
        "tabs": [{"tabRenderer": {"content": {"sectionListRenderer": {"contents": [
            {"musicResponsiveHeaderRenderer": {
                "title": {"runs": [{"text": "Album"}]},
                "straplineTextOne": {"runs": [{"text": "Artist"}]},
                "subtitle": {"runs": [{"text": "Album"}, {"text": " • "}, {"text": "2026"}]},
                "thumbnail": {"musicThumbnailRenderer": {"thumbnail": {"thumbnails": [
                    {"url": art, "width": 226}, {"url": "https://lh3.googleusercontent.com/big", "width": 544}]}}},
                "buttons": [
                    {"toggleButtonRenderer": {}},
                    {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchEndpoint": {
                        "videoId": "abcdefghijk", "playlistId": "OLAK5uy_abc",
                        "watchEndpointMusicSupportedConfigs": {}}}}},
                ],
            }},
        ]}}}}],
        "secondaryContents": {"sectionListRenderer": {"contents": [
            {"musicShelfRenderer": {"contents": [list_row("No art", "abcdefghijk", None), with_art]}},
        ]}},
    }}});
    let page = parse_browse(&answer);
    assert_eq!(page.header.title, "Album");
    assert_eq!(page.header.subtitle, "Artist • Album • 2026");
    assert_eq!(page.header.thumb, art);
    assert_eq!(
        to_value(&page.header.play),
        json!({"watchEndpoint": {"videoId": "abcdefghijk", "playlistId": "OLAK5uy_abc"}})
    );
    let thumbs: Vec<&str> = page.sections[0]
        .items
        .iter()
        .map(|r| r.thumb.as_str())
        .collect();
    assert_eq!(thumbs, [art, own]);

    // With no header art, rows stay without.
    answer["contents"]["twoColumnBrowseResultsRenderer"]["tabs"][0]["tabRenderer"]["content"]
        ["sectionListRenderer"]["contents"][0]["musicResponsiveHeaderRenderer"]
        .as_object_mut()
        .unwrap()
        .remove("thumbnail");
    assert_eq!(parse_browse(&answer).sections[0].items[0].thumb, "");
}

#[test]
fn ids_and_tokens_are_shape_checked() {
    let answer = single_column(vec![json!({"gridRenderer": {
        "header": {"gridHeaderRenderer": {"title": {"runs": [{"text": "Grid"}]}}},
        "items": [
            // A browse id with a slash: it could not be sent back, and the tile does nothing else.
            two_row("Bad browse", json!({"browseEndpoint": {"browseId": "../../x"}})),
            // A video id of the wrong length, and no other way to play.
            two_row("Bad video", json!({"watchEndpoint": {"videoId": "short"}})),
            // Good browse id, bad params: the params go, the row stays.
            two_row("Bad params", json!({"browseEndpoint": {"browseId": "MPREb_ok", "params": "a b"}})),
            // A watch endpoint with extra keys, a huge index and a bad params.
            two_row("Odd endpoint", json!({"watchEndpoint": {
                "videoId": "abcdefghijk", "playlistId": "PLabc", "index": 99_999_999_999u64,
                "params": "<script>", "playerParams": "x", "loggingContext": {"a": 1}}})),
            two_row("Fine endpoint", json!({"watchEndpoint": {
                "videoId": "bcdefghijkl", "playlistId": "PLabc", "index": 3, "params": "wAEB%3D"}})),
        ],
        "continuations": [{"nextContinuationData": {"continuation": "has spaces"}}],
    }})]);
    let page = parse_browse(&answer);
    let rows = &page.sections[0].items;
    let titles: Vec<&str> = rows.iter().map(|r| r.title.as_str()).collect();
    assert_eq!(titles, ["Bad params", "Odd endpoint", "Fine endpoint"]);
    assert_eq!(rows[0].browse_id, "MPREb_ok");
    assert_eq!(rows[0].params, "");
    assert!(rows[0].play.is_none());
    assert_eq!(
        to_value(&rows[1].play),
        json!({"watchEndpoint": {"videoId": "abcdefghijk", "playlistId": "PLabc"}})
    );
    assert_eq!(
        to_value(&rows[2].play),
        json!({"watchEndpoint": {"videoId": "bcdefghijkl", "playlistId": "PLabc", "index": 3, "params": "wAEB%3D"}})
    );
    assert_eq!(page.sections[0].cont, "");

    // Standard base64 (`+`, `/`) in params and tokens survives (ruling P3); a colon or a quote does not.
    let answer = single_column(vec![json!({"musicShelfRenderer": {
        "contents": [two_row("Std", json!({"browseEndpoint": {"browseId": "MPREb_ok", "params": "ab+c/d=="}}))],
        "continuations": [{"nextContinuationData": {"continuation": "4qmF+sgK/AQ%3D%3D"}}],
    }})]);
    let page = parse_browse(&answer);
    assert_eq!(page.sections[0].items[0].params, "ab+c/d==");
    assert_eq!(page.sections[0].cont, "4qmF+sgK/AQ%3D%3D");
    for bad in ["https://evil.example", "a\"b", "a b", ""] {
        let answer = single_column(vec![json!({"musicShelfRenderer": {
            "contents": [two_row("Std", json!({"browseEndpoint": {"browseId": "MPREb_ok", "params": bad}}))],
        }})]);
        assert_eq!(
            parse_browse(&answer).sections[0].items[0].params,
            "",
            "{bad}"
        );
    }

    // A "More" link with a bad browse id is no link; a good one with bad params keeps the link.
    let carousel = |browse_id: &str, params: &str| {
        single_column(vec![json!({"musicCarouselShelfRenderer": {
            "header": {"musicCarouselShelfBasicHeaderRenderer": {
                "title": {"runs": [{"text": "Albums"}]},
                "moreContentButton": {"buttonRenderer": {"navigationEndpoint": {"browseEndpoint": {
                    "browseId": browse_id, "params": params}}}},
            }},
            "contents": [two_row("Album", json!({"browseEndpoint": {"browseId": "MPREb_x"}}))],
        }})])
    };
    assert!(
        parse_browse(&carousel("UC x", "ok")).sections[0]
            .more
            .is_none()
    );
    assert_eq!(
        to_value(&parse_browse(&carousel("UCabc", "%%bad%%!")).sections[0].more),
        json!({"browseId": "UCabc", "params": ""})
    );

    // Endpoints coming back from a client go through the same checks.
    let ep: Endpoint = serde_json::from_value(json!({"watchEndpoint": {
        "videoId": "abcdefghijk", "index": -1, "params": "ok", "junk": true}}))
    .unwrap();
    assert_eq!(
        to_value(&ep),
        json!({"watchEndpoint": {"videoId": "abcdefghijk", "params": "ok"}})
    );
    assert!(
        serde_json::from_value::<Endpoint>(json!({"watchEndpoint": {"videoId": "bad"}})).is_err()
    );
    assert!(serde_json::from_value::<Endpoint>(json!({"watchPlaylistEndpoint": {}})).is_err());
    assert!(
        serde_json::from_value::<Endpoint>(json!({"browseEndpoint": {"browseId": "UCabc"}}))
            .is_err()
    );

    // A lyrics tab whose browse id is malformed is no lyrics tab.
    let next = |id: &str| {
        json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
        "watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"endpoint": {"browseEndpoint": {
            "browseId": id,
            "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {
                "pageType": "MUSIC_PAGE_TYPE_TRACK_LYRICS"}}}}}}]}}}}})
    };
    assert_eq!(
        parse_lyrics_tab(&next("MPLYt_ok")).as_deref(),
        Some("MPLYt_ok")
    );
    assert_eq!(parse_lyrics_tab(&next("MPLYt/../x")), None);
}

#[test]
fn like_status_from_next_answer() {
    assert_eq!(
        parse_like_status(&fixture("next_liked_song")),
        Some(LikeStatus::Like)
    );
    assert_eq!(
        parse_like_status(&fixture("next_not_liked_song")),
        Some(LikeStatus::Indifferent)
    );
    let dislike = json!({"playerOverlays": {"playerOverlayRenderer": {"actions": [
        {"somethingElse": {}},
        {"likeButtonRenderer": {"likeStatus": "DISLIKE"}},
    ]}}});
    assert_eq!(parse_like_status(&dislike), Some(LikeStatus::Dislike));
    let odd = json!({"playerOverlays": {"playerOverlayRenderer": {"actions": [
        {"likeButtonRenderer": {"likeStatus": "MAYBE"}}]}}});
    assert_eq!(parse_like_status(&odd), None);
    // The socket's words for it.
    assert_eq!(
        to_value(&[
            LikeStatus::Like,
            LikeStatus::Dislike,
            LikeStatus::Indifferent
        ]),
        json!(["like", "dislike", "none"])
    );
}

#[test]
fn like_status_for_one_song() {
    // The button names the song it is for: the status counts only for that song.
    let liked = fixture("next_liked_song");
    assert_eq!(
        parse_like_for(&liked, "fakeV000783"),
        Some(LikeStatus::Like)
    );
    assert_eq!(parse_like_for(&liked, "fakeV000790"), None);
    assert_eq!(
        parse_like_for(&fixture("next_not_liked_song"), "fakeV000790"),
        Some(LikeStatus::Indifferent)
    );
    // A button that names no song is taken for the song asked about (the answer is for it).
    let unnamed = json!({"playerOverlays": {"playerOverlayRenderer": {"actions": [
        {"likeButtonRenderer": {"likeStatus": "DISLIKE"}}]}}});
    assert_eq!(
        parse_like_for(&unnamed, "abcdefghijk"),
        Some(LikeStatus::Dislike)
    );
    // A target that is not a video id is no song at all.
    let odd = json!({"playerOverlays": {"playerOverlayRenderer": {"actions": [
        {"likeButtonRenderer": {"likeStatus": "LIKE", "target": {"videoId": "../x"}}}]}}});
    assert_eq!(parse_like_for(&odd, "../x"), None);
    assert_eq!(parse_like_for(&json!({}), "abcdefghijk"), None);
}

#[test]
fn row_fields_keep_page_js_order() {
    // The widget never depends on key order, but the shape is easier to diff against Page.js's own
    // output when it matches (kind last, as Page.js adds it last).
    let page = parse_browse(&single_column(vec![shelf(
        "S",
        vec![list_row("Song", "abcdefghijk", None)],
    )]));
    let text = serde_json::to_string(&page.sections[0].items[0]).unwrap();
    let keys: Vec<&str> = [
        "title",
        "subtitle",
        "thumb",
        "videoId",
        "setId",
        "playlistId",
        "browseId",
        "params",
        "play",
        "duration",
        "kind",
    ]
    .to_vec();
    let mut last = 0;
    for k in keys {
        let at = text.find(&format!("\"{k}\":")).unwrap();
        assert!(at >= last, "{k} out of order in {text}");
        last = at;
    }
}

#[test]
fn sections_follow_page_order_not_key_order() {
    // Two shelves under sibling keys whose page order is the reverse of their alphabetical order (as
    // a two-column page's `tabs` before `secondaryContents` can be). The walk must follow the page, so
    // this needs serde_json's preserve_order; with the default sorted map it would list "Second"
    // first. The scrubbed fixtures happen not to catch that, which is why this test exists.
    let answer: Value = serde_json::from_str(
        r#"{"contents": {"zeta": {"musicShelfRenderer": {"title": {"runs": [{"text": "First"}]},
              "contents": [{"musicTwoRowItemRenderer": {"title": {"runs": [{"text": "A"}]},
                "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_a"}}}}]}},
            "alpha": {"musicShelfRenderer": {"title": {"runs": [{"text": "Second"}]},
              "contents": [{"musicTwoRowItemRenderer": {"title": {"runs": [{"text": "B"}]},
                "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_b"}}}}]}}}}"#,
    )
    .unwrap();
    let titles: Vec<String> = parse_browse(&answer)
        .sections
        .into_iter()
        .map(|s| s.title)
        .collect();
    assert_eq!(titles, ["First", "Second"]);
}
