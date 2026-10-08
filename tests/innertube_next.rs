//! The InnerTube `next` request (YouTube Music's queue) against a local wiremock server.
//!
//! The `next_*.json` fixtures are real answers, scrubbed (see `fixtures/NEXT_FIXTURES.md`); the
//! small answers built in this file are hand-made. Nothing here talks to YouTube. As in
//! `innertube_player.rs`, the server's base URL is injected through `Innertube::new`, the one
//! place the https allowlist is bypassed (ruling R7).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::auth::{Cookie, MemoryStore, Session, SessionStore};
use ytmfast::browse::LikeStatus;
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, NextPage, NextRequest, clean_artist, clients};

const ALBUM: &str = include_str!("fixtures/next_album.json");
const RADIO: &str = include_str!("fixtures/next_radio.json");
const RADIO_MORE: &str = include_str!("fixtures/next_radio_continuation.json");
const LIKED: &str = include_str!("fixtures/next_liked.json");

/// The one (scrubbed) image link every fixture thumbnail carries.
const FIXTURE_THUMB: &str = "https://lh3.googleusercontent.com/fake=w544-h544-l90-rj";

fn cookie(domain: &str, name: &str, value: &str, secure: bool) -> Cookie {
    Cookie {
        domain: domain.into(),
        name: name.into(),
        value: value.into(),
        path: "/".into(),
        secure,
        expires_utc: None,
    }
}

/// A signed-in test session: the `.youtube.com` SAPISID signs the request, the `127.0.0.1`
/// cookies are what the wiremock host gets in its `Cookie` header.
fn session() -> Session {
    Session {
        cookies: vec![
            cookie(".youtube.com", "SAPISID", "fake-sapisid", true),
            cookie("127.0.0.1", "SAPISID", "fake-sapisid", false),
            cookie("127.0.0.1", "ROTATE", "old", false),
        ],
        account: None,
    }
}

struct Rig {
    server: MockServer,
    store: Arc<MemoryStore>,
    session: Arc<Mutex<Session>>,
    api: Innertube,
}

async fn rig() -> Rig {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryStore::new());
    let session = Arc::new(Mutex::new(session()));
    let base = Url::parse(&server.uri()).unwrap();
    let api = Innertube::new(session.clone(), store.clone(), base);
    Rig {
        server,
        store,
        session,
        api,
    }
}

fn next_mock(body: ResponseTemplate) -> Mock {
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/next"))
        .and(query_param("prettyPrint", "false"))
        .respond_with(body)
}

fn json_answer(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn header<'a>(req: &'a wiremock::Request, name: &str) -> &'a str {
    req.headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

fn playlist(id: &str) -> NextRequest {
    NextRequest {
        playlist_id: Some(id.into()),
        ..NextRequest::default()
    }
}

/// `answer` served once, then `req` asked of it.
async fn next_from(answer: &str, req: NextRequest) -> Result<NextPage, Error> {
    let rig = rig().await;
    next_mock(json_answer(answer)).mount(&rig.server).await;
    rig.api.next(req).await
}

/// A queue answer (first page shape) holding `items` as its panel contents.
fn queue_answer(items: Value) -> String {
    json!({"contents": {"singleColumnMusicWatchNextResultsRenderer": {"tabbedRenderer": {
        "watchNextTabbedResultsRenderer": {"tabs": [{"tabRenderer": {"content": {
            "musicQueueRenderer": {"content": {"playlistPanelRenderer": {
                "playlistId": "RDfakeQ",
                "contents": items
            }}}
        }}}]}
    }}}})
    .to_string()
}

/// A `playlistPanelVideoRenderer` with the given byline runs.
fn song(video_id: &str, byline: Value) -> Value {
    json!({"playlistPanelVideoRenderer": {
        "videoId": video_id,
        "title": {"runs": [{"text": "A Song"}]},
        "longBylineText": {"runs": byline},
        "lengthText": {"runs": [{"text": "3:05"}]},
        "navigationEndpoint": {"watchEndpoint": {"videoId": video_id, "playlistId": "RDfakeQ"}}
    }})
}

fn browse(text: &str, page_type: &str) -> Value {
    json!({"text": text, "navigationEndpoint": {"browseEndpoint": {
        "browseId": "fakeBrowse",
        "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {
            "pageType": page_type
        }}
    }}})
}

fn plain(text: &str) -> Value {
    json!({"text": text})
}

fn ids(page: &NextPage) -> Vec<&str> {
    page.items.iter().map(|s| s.video_id.as_str()).collect()
}

#[tokio::test]
async fn parses_album_queue() {
    let page = next_from(ALBUM, playlist("OLAK5uy_fakeP0004"))
        .await
        .unwrap();

    // Four songs in album order; the automix preview at the end is not a song.
    assert_eq!(
        ids(&page),
        ["fakeV000003", "fakeV000006", "fakeV000008", "fakeV000010"]
    );
    let titles: Vec<&str> = page.items.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, ["Text 2", "Text 9", "Text 12", "Text 15"]);
    let lengths: Vec<u32> = page.items.iter().map(|s| s.length_seconds).collect();
    assert_eq!(lengths, [6 * 60 + 3, 4 * 60 + 12, 4 * 60 + 38, 4 * 60 + 27]);
    for s in &page.items {
        assert_eq!(s.artists, ["Text 3"], "{}", s.video_id);
        assert_eq!(s.album.as_deref(), Some("Text 4"), "{}", s.video_id);
        // Ruling P15: the album link's browse id, so a widget can open the album.
        assert_eq!(s.album_id, "MPREb_fakeB0002", "{}", s.video_id);
        assert_eq!(s.thumbnail.as_deref(), Some(FIXTURE_THUMB));
        assert_eq!(s.playlist_id.as_deref(), Some("OLAK5uy_fakeP0004"));
    }
    assert_eq!(page.playlist_id.as_deref(), Some("OLAK5uy_fakeP0004"));
    // An album queue is finite: no continuation.
    assert_eq!(page.continuation, None);
}

#[tokio::test]
async fn parses_radio_with_continuation() {
    let page = next_from(
        RADIO,
        NextRequest {
            video_id: Some("fakeV000017".into()),
            playlist_id: Some("RDAMVMfakeV000017".into()),
            ..NextRequest::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.items.len(), 12);
    assert_eq!(page.playlist_id.as_deref(), Some("RDAMVMfakeP0018"));
    assert_eq!(page.continuation.as_deref(), Some("faketoken"));
    let first = &page.items[0];
    assert_eq!(first.video_id, "fakeV000017");
    assert_eq!(first.title, "Text 22");
    assert_eq!(first.artists, ["Text 23"]);
    assert_eq!(first.album.as_deref(), Some("Text 22"));
    assert_eq!(first.album_id, "MPREb_fakeB0016");
    assert_eq!(first.length_seconds, 5 * 60 + 18);
    // A plain (unwrapped) item parses the same way.
    let third = &page.items[2];
    assert_eq!(third.video_id, "fakeV000025");
    assert_eq!(third.artists, ["Text 33"]);
    assert_eq!(third.album.as_deref(), Some("Text 34"));
    assert_eq!(third.album_id, "MPREb_fakeB0024");
    assert_eq!(third.length_seconds, 3 * 60 + 50);
    assert_eq!(page.items[11].video_id, "fakeV000057");

    // The next page sits under `continuationContents`.
    let more = next_from(
        RADIO_MORE,
        NextRequest {
            continuation: Some("faketoken".into()),
            ..NextRequest::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(more.items.len(), 12);
    assert_eq!(more.items[0].video_id, "fakeV000060");
    assert_eq!(more.items[0].artists, ["Text 49"]);
    assert_eq!(more.items[0].length_seconds, 2 * 60 + 44);
    assert_eq!(more.items[11].video_id, "fakeV000093");
    assert_eq!(more.playlist_id.as_deref(), Some("RDAMVMfakeP0018"));
    assert_eq!(more.continuation.as_deref(), Some("faketoken"));
}

/// Every `counterpartRenderer` video id in `answer` (the music-video twins of wrapped songs).
fn counterpart_ids(answer: &str) -> Vec<String> {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Object(o) => {
                if let Some(c) = o.get("counterpartRenderer")
                    && let Some(id) = c["playlistPanelVideoRenderer"]["videoId"].as_str()
                {
                    out.push(id.to_string());
                }
                o.values().for_each(|v| walk(v, out));
            }
            Value::Array(a) => a.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(&serde_json::from_str(answer).unwrap(), &mut out);
    out
}

#[tokio::test]
async fn parses_wrapper_items() {
    // The radio answer mixes wrapped and plain items; a wrapper's song is its primary
    // renderer, never the counterpart (the music video of the same song).
    let page = next_from(RADIO, playlist("RDAMVMfakeV000017"))
        .await
        .unwrap();
    assert_eq!(
        ids(&page),
        [
            "fakeV000017",
            "fakeV000021",
            "fakeV000025",
            "fakeV000028",
            "fakeV000032",
            "fakeV000036",
            "fakeV000040",
            "fakeV000043",
            "fakeV000046",
            "fakeV000050",
            "fakeV000054",
            "fakeV000057"
        ]
    );
    let twins = counterpart_ids(RADIO);
    assert!(!twins.is_empty(), "the fixture has wrapped items");
    for twin in &twins {
        assert!(
            !ids(&page).contains(&twin.as_str()),
            "{twin} is a counterpart"
        );
    }
}

#[tokio::test]
async fn parses_liked_byline_variants() {
    let page = next_from(LIKED, playlist("LM")).await.unwrap();
    assert_eq!(page.items.len(), 12);
    assert_eq!(page.playlist_id.as_deref(), Some("LM"));
    let by_id = |id: &str| page.items.iter().find(|s| s.video_id == id).unwrap();

    // "Text 138 & Text 139 • album • year": the first artist has no link, and is still one.
    let two = by_id("fakeV000106");
    assert_eq!(two.artists, ["Text 138", "Text 139"]);
    assert_eq!(two.album.as_deref(), Some("Text 140"));
    assert_eq!(two.album_id, "MPREb_fakeB0105");
    // Over an hour is still m:ss on YouTube Music.
    assert_eq!(two.length_seconds, 13 * 60 + 42);

    // Two linked artists.
    assert_eq!(by_id("fakeV000121").artists, ["Text 160", "Text 161"]);

    // A user upload: the channel is the artist, and there is no album.
    let upload = by_id("fakeV000113");
    assert_eq!(upload.artists, ["Text 149"]);
    assert_eq!(upload.album, None);
    // No album link: "", never null (the channel link is not an album).
    assert_eq!(upload.album_id, "");
}

/// Ruling P15: the album id is shape-checked like every browse id. A malformed one (spaces,
/// too long, not a string) or a link without one leaves `""`, and the song is still queued
/// with its album name; only a link to an album page counts.
#[tokio::test]
async fn album_id_is_shape_checked() {
    let album_run = |browse_id: Value| {
        json!({"text": "An Album", "navigationEndpoint": {"browseEndpoint": {
            "browseId": browse_id,
            "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {
                "pageType": "MUSIC_PAGE_TYPE_ALBUM"
            }}
        }}})
    };
    let byline = |album: Value| {
        json!([
            plain("Artist"),
            plain(" • "),
            album,
            plain(" • "),
            plain("2024")
        ])
    };
    let cases = [
        (album_run(json!("MPREb_good-1")), "MPREb_good-1"),
        (album_run(json!("MPREb bad")), ""),
        (album_run(json!("MPREb_\"}")), ""),
        (album_run(json!("M".repeat(129))), ""),
        (album_run(json!(42)), ""),
        (album_run(Value::Null), ""),
        (
            json!({"text": "An Album", "navigationEndpoint": {"browseEndpoint": {
                "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {
                    "pageType": "MUSIC_PAGE_TYPE_ALBUM"
                }}
            }}}),
            "",
        ),
        // A link to some other page is not an album, whatever its id.
        (browse("An Album", "MUSIC_PAGE_TYPE_PLAYLIST"), ""),
    ];
    let items: Vec<Value> = cases
        .iter()
        .enumerate()
        .map(|(i, (run, _))| song(&format!("fakeV{i:06}"), byline(run.clone())))
        .collect();
    let page = next_from(&queue_answer(json!(items)), playlist("RDfakeQ"))
        .await
        .unwrap();
    assert_eq!(page.items.len(), cases.len(), "every song is kept");
    for (song, (run, want)) in page.items.iter().zip(&cases) {
        assert_eq!(song.album_id, *want, "{run}");
        assert_eq!(song.artists, ["Artist"], "{run}");
    }
    assert_eq!(page.items[0].album.as_deref(), Some("An Album"));
    assert_eq!(
        page.items[1].album.as_deref(),
        Some("An Album"),
        "the name is kept"
    );
}

/// Task 6: a queue asked for with a song carries that song's Lyrics tab (the lyrics page's
/// browse id), as it carries its like status: lyrics for the song then need only that browse.
#[tokio::test]
async fn a_song_s_queue_carries_its_lyrics_tab() {
    let radio = NextRequest {
        video_id: Some("fakeV000001".into()),
        playlist_id: Some("RDAMVMfakeV000001".into()),
        ..NextRequest::default()
    };
    let page = next_from(RADIO, radio).await.unwrap();
    assert_eq!(page.lyrics_tab.as_deref(), Some("MPLYt_fakeB0058"));
    // A playlist names no song: its tabs are not used.
    assert_eq!(
        next_from(RADIO, playlist("LM")).await.unwrap().lyrics_tab,
        None
    );
}

#[tokio::test]
async fn a_song_s_queue_carries_its_like_status() {
    // A queue asked for with a song: the answer's like button is that song's (ruling P1).
    let mut answer: Value = serde_json::from_str(&queue_answer(json!([song(
        "fakeV000001",
        json!([plain("Artist")])
    )])))
    .unwrap();
    let button = |target: &str, status: &str| {
        json!({"playerOverlayRenderer": {"actions": [{"likeButtonRenderer": {
            "target": {"videoId": target}, "likeStatus": status
        }}]}})
    };
    answer["playerOverlays"] = button("fakeV000001", "LIKE");
    let radio = |id: &str| NextRequest {
        video_id: Some(id.into()),
        playlist_id: Some(format!("RDAMVM{id}")),
        ..NextRequest::default()
    };
    let text = answer.to_string();
    let page = next_from(&text, radio("fakeV000001")).await.unwrap();
    assert_eq!(page.like, Some(LikeStatus::Like));
    // A playlist or a continuation names no song: whatever its button says is not used.
    assert_eq!(next_from(&text, playlist("LM")).await.unwrap().like, None);
    // A button for another song says nothing about the one asked for.
    assert_eq!(
        next_from(&text, radio("fakeV000002")).await.unwrap().like,
        None
    );
    // No button: unknown.
    answer["playerOverlays"] = json!({});
    let page = next_from(&answer.to_string(), radio("fakeV000001"))
        .await
        .unwrap();
    assert_eq!(page.like, None);
    answer["playerOverlays"] = button("fakeV000001", "DISLIKE");
    let page = next_from(&answer.to_string(), radio("fakeV000001"))
        .await
        .unwrap();
    assert_eq!(page.like, Some(LikeStatus::Dislike));
}

#[tokio::test]
async fn continuation_request_shape() {
    let rig = rig().await;
    next_mock(json_answer(RADIO_MORE))
        .expect(1)
        .mount(&rig.server)
        .await;

    rig.api
        .next(NextRequest {
            continuation: Some("fakecontinuation".into()),
            ..NextRequest::default()
        })
        .await
        .unwrap();

    let reqs = rig.server.received_requests().await.unwrap();
    let req = &reqs[0];
    let music = clients::WEB_REMIX;
    assert_eq!(req.url.path(), "/youtubei/v1/next");
    assert_eq!(req.url.query(), Some("prettyPrint=false"));
    assert_eq!(header(req, "x-youtube-client-name"), "67");
    assert_eq!(header(req, "x-youtube-client-version"), music.version);
    assert_eq!(header(req, "origin"), "https://music.youtube.com");
    assert_eq!(header(req, "x-origin"), "https://music.youtube.com");
    assert_eq!(header(req, "x-goog-authuser"), "0");
    assert_eq!(header(req, "user-agent"), music.user_agent);
    assert_eq!(header(req, "content-type"), "application/json");
    assert_eq!(header(req, "cookie"), "SAPISID=fake-sapisid; ROTATE=old");

    // The SAPISIDHASH is signed for the music origin, not www.youtube.com.
    let auth = header(req, "authorization");
    let signed = auth
        .strip_prefix("SAPISIDHASH ")
        .expect("authorization scheme");
    let (ts, hash) = signed.split_once('_').unwrap();
    let want = Sha1::digest(format!("{ts} fake-sapisid https://music.youtube.com").as_bytes());
    assert_eq!(hash, format!("{want:x}"));

    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(
        body["context"]["client"],
        json!({"clientName": "WEB_REMIX", "clientVersion": music.version, "hl": "en"})
    );
    assert_eq!(body["isAudioOnly"], true);
    assert_eq!(body["continuation"], "fakecontinuation");
    // Only what was asked for goes out.
    for absent in ["videoId", "playlistId", "index", "params"] {
        assert!(body.get(absent).is_none(), "{absent} sent: {body}");
    }
}

#[tokio::test]
async fn first_page_request_shape() {
    let rig = rig().await;
    next_mock(json_answer(RADIO)).mount(&rig.server).await;
    rig.api
        .next(NextRequest {
            video_id: Some("fakeV000017".into()),
            playlist_id: Some("RDAMVMfakeV000017".into()),
            index: Some(3),
            params: Some("fakeparams".into()),
            continuation: None,
        })
        .await
        .unwrap();
    let reqs = rig.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["videoId"], "fakeV000017");
    assert_eq!(body["playlistId"], "RDAMVMfakeV000017");
    assert_eq!(body["index"], 3);
    assert_eq!(body["params"], "fakeparams");
    assert!(body.get("continuation").is_none(), "{body}");

    // An album is asked for by playlist id alone (a video id with it gives one song plus
    // an automix preview; see NEXT_FIXTURES.md).
    let album = self::rig().await;
    next_mock(json_answer(ALBUM)).mount(&album.server).await;
    album.api.next(playlist("OLAK5uy_fakeP0004")).await.unwrap();
    let reqs = album.server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    assert_eq!(body["playlistId"], "OLAK5uy_fakeP0004");
    assert!(body.get("videoId").is_none(), "{body}");
}

#[tokio::test]
async fn thumbnail_must_pass_allowlist() {
    let mut item = song("fakeT000001", json!([plain("Artist")]));
    item["playlistPanelVideoRenderer"]["thumbnail"] = json!({"thumbnails": [
        {"url": "https://i.ytimg.com/vi/fakeT000001/small.jpg", "width": 60},
        {"url": "//i.ytimg.com/vi/fakeT000001/big.jpg", "width": 544},
        // The widest, but on no allowed host.
        {"url": "https://evil.example/huge.jpg", "width": 4000},
        {"url": "http://i.ytimg.com/vi/fakeT000001/plain-http.jpg", "width": 3000}
    ]});
    let mut bad = song("fakeT000002", json!([plain("Artist")]));
    bad["playlistPanelVideoRenderer"]["thumbnail"] =
        json!({"thumbnails": [{"url": "https://evil.example/only.jpg", "width": 500}]});
    let page = next_from(&queue_answer(json!([item, bad])), playlist("RDfakeQ"))
        .await
        .unwrap();
    // The widest allowed one, made https.
    assert_eq!(
        page.items[0].thumbnail.as_deref(),
        Some("https://i.ytimg.com/vi/fakeT000001/big.jpg")
    );
    assert_eq!(page.items[1].thumbnail, None);
}

#[tokio::test]
async fn topic_suffix_stripped() {
    assert_eq!(clean_artist("Some Band - Topic"), "Some Band");
    assert_eq!(clean_artist("Some Band"), "Some Band");
    // Only a trailing suffix, and only the exact one.
    assert_eq!(clean_artist("Topic - Topical"), "Topic - Topical");
    assert_eq!(clean_artist("A - Topic - Topic"), "A - Topic");
    assert_eq!(clean_artist("Band-Topic"), "Band-Topic");

    let item = song(
        "fakeT000003",
        json!([
            browse("Some Band - Topic", "MUSIC_PAGE_TYPE_USER_CHANNEL"),
            plain(" • "),
            plain("1.2K views")
        ]),
    );
    let page = next_from(&queue_answer(json!([item])), playlist("RDfakeQ"))
        .await
        .unwrap();
    assert_eq!(page.items[0].artists, ["Some Band"]);
}

#[tokio::test]
async fn byline_without_links() {
    // No run has a link: the artists are the text before the first " • ".
    let item = song(
        "fakeT000004",
        json!([
            plain("One"),
            plain(", "),
            plain("Two"),
            plain(" • "),
            plain("2024")
        ]),
    );
    let page = next_from(&queue_answer(json!([item])), playlist("RDfakeQ"))
        .await
        .unwrap();
    assert_eq!(page.items[0].artists, ["One", "Two"]);
    assert_eq!(page.items[0].album, None);
}

#[tokio::test]
async fn odd_items_skipped_unavailable_kept() {
    let mut unavailable = song("fakeU000001", json!([plain("Artist")]));
    unavailable["playlistPanelVideoRenderer"]
        .as_object_mut()
        .unwrap()
        .remove("lengthText");
    let mut no_id = song("unused", json!([plain("Artist")]));
    let r = no_id["playlistPanelVideoRenderer"].as_object_mut().unwrap();
    r.remove("videoId");
    r.remove("navigationEndpoint");
    // An item whose own id is missing but whose endpoint has one is still a song.
    let mut endpoint_only = song("fakeE000001", json!([plain("Artist")]));
    endpoint_only["playlistPanelVideoRenderer"]
        .as_object_mut()
        .unwrap()
        .remove("videoId");
    let mut odd_length = song("fakeL000001", json!([plain("Artist")]));
    odd_length["playlistPanelVideoRenderer"]["lengthText"] = json!({"runs": [{"text": "1:2:3:4"}]});
    let mut hours = song("fakeH000001", json!([plain("Artist")]));
    hours["playlistPanelVideoRenderer"]["lengthText"] = json!({"runs": [{"text": "1:02:03"}]});
    // A malformed item (the title is a number) is dropped, not the whole page.
    let mut malformed = song("fakeM000001", json!([plain("Artist")]));
    malformed["playlistPanelVideoRenderer"]["title"] = json!(5);
    let automix = json!({"automixPreviewVideoRenderer": {"content": {}}});
    // Not an 11-character id: it would change the yt-dlp URL it ends up in, so it is skipped.
    let bad_id = song("bad&list=xx", json!([plain("Artist")]));

    let page = next_from(
        &queue_answer(json!([
            unavailable,
            no_id,
            endpoint_only,
            odd_length,
            hours,
            malformed,
            automix,
            bad_id
        ])),
        playlist("RDfakeQ"),
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&page),
        ["fakeU000001", "fakeE000001", "fakeL000001", "fakeH000001"]
    );
    assert_eq!(page.items[0].length_seconds, 0);
    assert_eq!(page.items[2].length_seconds, 0);
    assert_eq!(page.items[3].length_seconds, 3600 + 2 * 60 + 3);
}

#[tokio::test]
async fn answer_without_a_queue_is_an_error() {
    let err = next_from("{\"responseContext\": {}}", playlist("RDfakeQ"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "unavailable", "{err}");
}

#[tokio::test]
async fn garbage_answer_is_an_error_without_its_text() {
    let err = next_from("{\"contents\": SECRETGARBAGE", playlist("RDfakeQ"))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "internal");
    assert!(!err.to_string().contains("SECRETGARBAGE"), "{err}");
}

#[tokio::test]
async fn unauthorized_is_signed_out() {
    let rig = rig().await;
    next_mock(ResponseTemplate::new(401))
        .mount(&rig.server)
        .await;
    assert_eq!(rig.api.next(playlist("LM")).await, Err(Error::SignedOut));
}

#[tokio::test]
async fn set_cookie_from_next_is_kept() {
    // `next` shares the session handling of `player`: a rotation is applied and saved.
    let rig = rig().await;
    next_mock(json_answer(LIKED).append_header("set-cookie", "ROTATE=new; Path=/"))
        .mount(&rig.server)
        .await;
    rig.api.next(playlist("LM")).await.unwrap();
    let live = rig.session.lock().unwrap().clone();
    assert!(
        live.cookies
            .iter()
            .any(|c| c.name == "ROTATE" && c.value == "new")
    );
    for _ in 0..500 {
        if let Ok(saved) = rig.store.load().await {
            assert!(
                saved
                    .cookies
                    .iter()
                    .any(|c| c.name == "ROTATE" && c.value == "new")
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the rotation was never saved");
}

#[test]
fn api_host_is_used() {
    // In production each client goes to its own `api_host`: `next` to music.youtube.com,
    // `player` to www.youtube.com.
    let store: Arc<dyn SessionStore> = Arc::new(MemoryStore::new());
    let api = Innertube::production(Arc::default(), store.clone());
    let next = api.endpoint_url(&clients::WEB_REMIX, "next");
    assert_eq!(
        next.as_str(),
        "https://music.youtube.com/youtubei/v1/next?prettyPrint=false"
    );
    assert_eq!(next.host_str(), Some(clients::WEB_REMIX.api_host));
    let player = api.endpoint_url(&clients::TV, "player");
    assert_eq!(
        player.as_str(),
        "https://www.youtube.com/youtubei/v1/player?prettyPrint=false"
    );
    assert_eq!(player.host_str(), Some(clients::TV.api_host));
    for c in clients::ALL {
        assert!(ytmfast::net::allowed_host(&api.endpoint_url(c, "next")));
    }

    // An injected test base takes every client to it.
    let base = Url::parse("http://127.0.0.1:9").unwrap();
    let test = Innertube::new(Arc::default(), store, base);
    assert_eq!(
        test.endpoint_url(&clients::WEB_REMIX, "next").as_str(),
        "http://127.0.0.1:9/youtubei/v1/next?prettyPrint=false"
    );
}

#[tokio::test]
async fn next_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let rig = rig().await;
    let fut = rig.api.next(playlist("LM"));
    assert_send(&fut);
}
