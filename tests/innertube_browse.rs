//! The browsing requests (`browse`, `search`, continuations, like and lyrics) against a local
//! wiremock server.
//!
//! The answers served are the scrubbed step-3 fixtures (`fixtures/browse`, see
//! `BROWSE_FIXTURES.md`), so each test runs the whole path: request out, capped read, JSON,
//! `browse::parse_*`. What the parsers make of each fixture is checked against `Page.js` in
//! `browse_parse.rs`; here the result must be what the parser gives for the same answer. Nothing
//! here talks to YouTube: the server's base URL is injected through `Innertube::new`, the one
//! place the https allowlist is bypassed (ruling R7).

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use url::Url;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};
use ytmfast::auth::{Cookie, MemoryStore, Session};
use ytmfast::browse::{LikeStatus, parse_browse, parse_lyrics, parse_more, parse_search};
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, MoreKind, NextRequest, clients};

const HOME: &str = include_str!("fixtures/browse/browse_home.json");
const HOME_CONT: &str = include_str!("fixtures/browse/browse_home_cont.json");
const ALBUM: &str = include_str!("fixtures/browse/browse_album.json");
const SEARCH_MIXED: &str = include_str!("fixtures/browse/search_mixed.json");
const SEARCH_SONGS: &str = include_str!("fixtures/browse/search_songs.json");
const SEARCH_SONGS_CONT: &str = include_str!("fixtures/browse/search_songs_cont.json");
const NEXT_FOR_LYRICS: &str = include_str!("fixtures/browse/next_song_for_lyrics.json");
const LYRICS: &str = include_str!("fixtures/browse/browse_lyrics.json");

/// The lyrics page id the `next_song_for_lyrics` fixture's Lyrics tab points at.
const LYRICS_ID: &str = "MPLYtfake000803";

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
/// cookie is what the wiremock host gets in its `Cookie` header.
fn session() -> Session {
    Session {
        cookies: vec![
            cookie(".youtube.com", "SAPISID", "fake-sapisid", true),
            cookie("127.0.0.1", "SAPISID", "fake-sapisid", false),
        ],
    }
}

struct Rig {
    server: MockServer,
    api: Innertube,
}

async fn rig() -> Rig {
    let server = MockServer::start().await;
    let store = Arc::new(MemoryStore::new());
    let base = Url::parse(&server.uri()).unwrap();
    let api = Innertube::new(Arc::new(Mutex::new(session())), store, base);
    Rig { server, api }
}

fn endpoint(name: &str, body: ResponseTemplate) -> Mock {
    Mock::given(method("POST"))
        .and(path(format!("/youtubei/v1/{name}")))
        .and(query_param("prettyPrint", "false"))
        .respond_with(body)
}

fn json_answer(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body.as_bytes().to_vec(), "application/json")
}

fn value(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

fn header<'a>(req: &'a wiremock::Request, name: &str) -> &'a str {
    req.headers
        .get(name)
        .unwrap_or_else(|| panic!("missing header {name}"))
        .to_str()
        .unwrap()
}

/// The request went out as the music web client, signed for the music origin, and its body
/// carries that client's context. Returns the body.
fn web_remix(req: &wiremock::Request, endpoint: &str) -> Value {
    let music = clients::WEB_REMIX;
    assert_eq!(req.url.path(), format!("/youtubei/v1/{endpoint}"));
    assert_eq!(req.url.query(), Some("prettyPrint=false"));
    assert_eq!(header(req, "x-youtube-client-name"), "67");
    assert_eq!(header(req, "x-youtube-client-version"), music.version);
    assert_eq!(header(req, "origin"), "https://music.youtube.com");
    assert_eq!(header(req, "x-origin"), "https://music.youtube.com");
    assert_eq!(header(req, "x-goog-authuser"), "0");
    assert_eq!(header(req, "user-agent"), music.user_agent);
    assert_eq!(header(req, "content-type"), "application/json");
    assert_eq!(header(req, "cookie"), "SAPISID=fake-sapisid");
    let signed = header(req, "authorization")
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
    body
}

/// The body's own keys, `context` left out, sorted.
fn keys(body: &Value) -> Vec<&str> {
    let mut k: Vec<&str> = body
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "context")
        .collect();
    k.sort_unstable();
    k
}

async fn requests(rig: &Rig) -> Vec<wiremock::Request> {
    rig.server.received_requests().await.unwrap()
}

#[tokio::test]
async fn browse_request_shape() {
    let rig = rig().await;
    endpoint("browse", json_answer(HOME))
        .mount(&rig.server)
        .await;

    let page = rig.api.browse("FEmusic_home", None).await.unwrap();
    // The answer went through the parser whole.
    assert_eq!(page, parse_browse(&value(HOME)));
    assert!(!page.sections.is_empty());

    // With params (an artist's "more" link, say), and a params of "" counts as none: that is
    // how a row without params comes back from a client.
    rig.api.browse("MPREb_fake000001", Some("")).await.unwrap();
    rig.api
        .browse("UCfake000001", Some("fake+params/x=="))
        .await
        .unwrap();

    let reqs = requests(&rig).await;
    assert_eq!(reqs.len(), 3);
    let body = web_remix(&reqs[0], "browse");
    assert_eq!(keys(&body), ["browseId"]);
    assert_eq!(body["browseId"], "FEmusic_home");
    let body = web_remix(&reqs[1], "browse");
    assert_eq!(keys(&body), ["browseId"]);
    let body = web_remix(&reqs[2], "browse");
    assert_eq!(keys(&body), ["browseId", "params"]);
    assert_eq!(body["browseId"], "UCfake000001");
    assert_eq!(body["params"], "fake+params/x==");
}

#[tokio::test]
async fn browse_album_header_art_reaches_rows() {
    // End to end on a second page kind: the album rule (rows take the header's art) runs.
    let rig = rig().await;
    endpoint("browse", json_answer(ALBUM))
        .mount(&rig.server)
        .await;
    let page = rig.api.browse("MPREb_fake000001", None).await.unwrap();
    assert_eq!(page, parse_browse(&value(ALBUM)));
}

#[tokio::test]
async fn search_request_shape() {
    let rig = rig().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/search"))
        .and(wiremock::matchers::body_partial_json(
            json!({"params": "faketoken"}),
        ))
        .respond_with(json_answer(SEARCH_SONGS))
        .with_priority(1)
        .mount(&rig.server)
        .await;
    endpoint("search", json_answer(SEARCH_MIXED))
        .with_priority(2)
        .mount(&rig.server)
        .await;

    // Mixed results: no params, 30 rows a section.
    let mixed = rig.api.search("some song", None).await.unwrap();
    assert_eq!(mixed, parse_search(&value(SEARCH_MIXED), false));
    assert!(!mixed.sections.is_empty());
    // A filter chip's params: the filtered parse (300 a section, paged).
    let songs = rig
        .api
        .search("some song", Some("faketoken"))
        .await
        .unwrap();
    assert_eq!(songs, parse_search(&value(SEARCH_SONGS), true));

    let reqs = requests(&rig).await;
    let body = web_remix(&reqs[0], "search");
    assert_eq!(keys(&body), ["query"]);
    assert_eq!(body["query"], "some song");
    let body = web_remix(&reqs[1], "search");
    assert_eq!(keys(&body), ["params", "query"]);
    assert_eq!(body["params"], "faketoken");
}

#[tokio::test]
async fn continuation_request_shape() {
    let rig = rig().await;
    endpoint("browse", json_answer(HOME_CONT))
        .mount(&rig.server)
        .await;
    endpoint("search", json_answer(SEARCH_SONGS_CONT))
        .mount(&rig.server)
        .await;

    let home = rig
        .api
        .more(MoreKind::Browse, "fake+token/one%3D")
        .await
        .unwrap();
    assert_eq!(home, parse_more(&value(HOME_CONT)));
    assert!(!home.sections.is_empty() || !home.items.is_empty());
    let songs = rig.api.more(MoreKind::Search, "faketoken").await.unwrap();
    assert_eq!(songs, parse_more(&value(SEARCH_SONGS_CONT)));
    assert!(!songs.items.is_empty());

    let reqs = requests(&rig).await;
    // The token alone, on the endpoint the list came from.
    let body = web_remix(&reqs[0], "browse");
    assert_eq!(keys(&body), ["continuation"]);
    assert_eq!(body["continuation"], "fake+token/one%3D");
    let body = web_remix(&reqs[1], "search");
    assert_eq!(keys(&body), ["continuation"]);
    assert_eq!(body["continuation"], "faketoken");

    // On the socket the kind is "browse" or "search".
    assert_eq!(
        serde_json::from_value::<MoreKind>(json!("search")).unwrap(),
        MoreKind::Search
    );
    assert_eq!(serde_json::to_value(MoreKind::Browse).unwrap(), "browse");
}

#[tokio::test]
async fn like_endpoints() {
    let rig = rig().await;
    for name in ["like/like", "like/dislike", "like/removelike"] {
        endpoint(name, json_answer("{}")).mount(&rig.server).await;
    }
    rig.api.like("fakeV000001", LikeStatus::Like).await.unwrap();
    rig.api
        .like("fakeV000002", LikeStatus::Dislike)
        .await
        .unwrap();
    rig.api
        .like("fakeV000003", LikeStatus::Indifferent)
        .await
        .unwrap();

    let reqs = requests(&rig).await;
    for (req, (name, id)) in reqs.iter().zip([
        ("like/like", "fakeV000001"),
        ("like/dislike", "fakeV000002"),
        ("like/removelike", "fakeV000003"),
    ]) {
        let body = web_remix(req, name);
        assert_eq!(keys(&body), ["target"]);
        assert_eq!(body["target"], json!({"videoId": id}));
    }

    // A refused like is a signed-out session, whether YouTube says 401 or 403.
    for status in [401, 403] {
        let rig = self::rig().await;
        endpoint("like/like", ResponseTemplate::new(status))
            .mount(&rig.server)
            .await;
        assert_eq!(
            rig.api.like("fakeV000001", LikeStatus::Like).await,
            Err(Error::SignedOut),
            "{status}"
        );
    }
}

#[tokio::test]
async fn lyrics_two_requests() {
    let rig = rig().await;
    endpoint("next", json_answer(NEXT_FOR_LYRICS))
        .expect(1)
        .mount(&rig.server)
        .await;
    endpoint("browse", json_answer(LYRICS))
        .expect(1)
        .mount(&rig.server)
        .await;

    let got = rig.api.lyrics("fakeV000797").await.unwrap();
    assert_eq!(got, parse_lyrics(&value(LYRICS)));
    assert!(got.is_some());

    let reqs = requests(&rig).await;
    assert_eq!(reqs.len(), 2);
    // First the song's `next`, the same body the queue sends for one song...
    let body = web_remix(&reqs[0], "next");
    assert_eq!(keys(&body), ["isAudioOnly", "videoId"]);
    assert_eq!(body["videoId"], "fakeV000797");
    assert_eq!(body["isAudioOnly"], true);
    // ...then the lyrics page its Lyrics tab names.
    let body = web_remix(&reqs[1], "browse");
    assert_eq!(keys(&body), ["browseId"]);
    assert_eq!(body["browseId"], LYRICS_ID);

    // A song with no Lyrics tab: no second request, and no lyrics.
    let none = self::rig().await;
    endpoint("next", json_answer("{}"))
        .expect(1)
        .mount(&none.server)
        .await;
    assert_eq!(none.api.lyrics("fakeV000797").await, Ok(None));
    assert_eq!(requests(&none).await.len(), 1);
}

#[tokio::test]
async fn query_length_capped() {
    let rig = rig().await;
    endpoint("search", json_answer(SEARCH_MIXED))
        .mount(&rig.server)
        .await;

    // Trimmed before it goes out.
    rig.api.search("  \t some song \n ", None).await.unwrap();
    // 200 characters is the most, counted as characters: 200 "é" are 400 bytes.
    let longest = "é".repeat(200);
    rig.api.search(&longest, None).await.unwrap();
    let reqs = requests(&rig).await;
    assert_eq!(web_remix(&reqs[0], "search")["query"], "some song");
    assert_eq!(web_remix(&reqs[1], "search")["query"], longest.as_str());

    // Refused before anything is sent.
    let too_long = "é".repeat(201);
    for bad in [
        "",
        "   ",
        "\n\t",
        too_long.as_str(),
        "a\u{7}b",
        "line\nbreak",
    ] {
        let err = rig.api.search(bad, None).await.unwrap_err();
        assert_eq!(err.code(), "bad_request", "{bad:?}: {err}");
    }
    assert_eq!(requests(&rig).await.len(), 2);
}

#[tokio::test]
async fn bad_ids_and_tokens_are_refused_before_sending() {
    let rig = rig().await;
    let long = "a".repeat(4097);
    let checks = [
        rig.api.browse("", None).await.map(drop),
        rig.api.browse("a", None).await.map(drop),
        rig.api.browse("FE/../x", None).await.map(drop),
        rig.api
            .browse("FEmusic_home", Some("bad params"))
            .await
            .map(drop),
        rig.api.search("ok", Some("\"}")).await.map(drop),
        rig.api.more(MoreKind::Browse, "").await.map(drop),
        rig.api.more(MoreKind::Search, &long).await.map(drop),
        rig.api.like("short", LikeStatus::Like).await,
        rig.api.like("fakeV00000/", LikeStatus::Like).await,
        rig.api.lyrics("fakeV0000011").await.map(drop),
    ];
    for (i, r) in checks.into_iter().enumerate() {
        assert_eq!(r.unwrap_err().code(), "bad_request", "check {i}");
    }
    assert!(requests(&rig).await.is_empty());
}

#[tokio::test]
async fn answer_size_capped() {
    let big = vec![b' '; 33 << 20];
    for which in ["browse", "search", "more", "like", "lyrics"] {
        let rig = rig().await;
        let answer = ResponseTemplate::new(200).set_body_raw(big.clone(), "application/json");
        let name = match which {
            "more" => "browse",
            "like" => "like/like",
            "lyrics" => "next",
            other => other,
        };
        endpoint(name, answer).mount(&rig.server).await;
        let err = match which {
            "browse" => rig.api.browse("FEmusic_home", None).await.map(drop),
            "search" => rig.api.search("q", None).await.map(drop),
            "more" => rig.api.more(MoreKind::Browse, "faketoken").await.map(drop),
            "like" => rig.api.like("fakeV000001", LikeStatus::Like).await,
            _ => rig.api.lyrics("fakeV000001").await.map(drop),
        }
        .unwrap_err();
        assert_eq!(err.code(), "network", "{which}: {err}");
        assert!(err.to_string().contains("too large"), "{which}: {err}");
    }
}

#[tokio::test]
async fn errors_carry_no_ids_or_tokens() {
    // Every input carries a marker; no error, in either of its printed forms, may hold one.
    const MARK: &str = "SECRETmark";
    fn clean(r: Result<(), Error>, what: &str) {
        let e = r.unwrap_err();
        let shown = format!("{e} {e:?}");
        assert!(!shown.contains(MARK), "{what}: {shown}");
        assert!(!shown.contains("127.0.0.1"), "{what}: {shown}");
    }

    // Refused inputs.
    let rig = rig().await;
    let bad_id = format!("{MARK}/x");
    let bad_token = format!("{MARK} token");
    let bad_query = format!("{MARK}\u{1b}[2J");
    clean(rig.api.browse(&bad_id, None).await.map(drop), "browse id");
    clean(
        rig.api
            .browse("FEmusic_home", Some(&bad_token))
            .await
            .map(drop),
        "params",
    );
    clean(rig.api.search(&bad_query, None).await.map(drop), "query");
    clean(
        rig.api.search(&MARK.repeat(30), None).await.map(drop),
        "long query",
    );
    clean(
        rig.api.more(MoreKind::Search, &bad_token).await.map(drop),
        "token",
    );
    clean(rig.api.like(&bad_id, LikeStatus::Like).await, "like id");
    clean(rig.api.lyrics(&bad_id).await.map(drop), "lyrics id");

    // Good inputs that the server fails: an error status, an answer that isn't JSON (and
    // quotes the input back), a refusal.
    let id = format!("VL{MARK}01");
    let video = "SECRETmark0"; // 11 characters: a valid video id holding the marker.
    // `like` doesn't read its answer's body, so the not-JSON answer is a success for it.
    for (answer, like_fails) in [
        (
            ResponseTemplate::new(500).set_body_string(format!("{{\"error\": \"{MARK}\"}}")),
            true,
        ),
        (json_answer(&format!("{{\"echo\": {MARK}")), false),
        (ResponseTemplate::new(403), true),
    ] {
        let rig = self::rig().await;
        for name in ["browse", "search", "next", "like/like"] {
            endpoint(name, answer.clone()).mount(&rig.server).await;
        }
        clean(rig.api.browse(&id, Some(MARK)).await.map(drop), "browse");
        clean(rig.api.search(MARK, Some(MARK)).await.map(drop), "search");
        clean(rig.api.more(MoreKind::Browse, MARK).await.map(drop), "more");
        let like = rig.api.like(video, LikeStatus::Like).await;
        if like_fails {
            clean(like, "like");
        } else {
            assert_eq!(like, Ok(()));
        }
        clean(rig.api.lyrics(video).await.map(drop), "lyrics");
    }

    // A connection that fails outright (reqwest's own text names the URL).
    let store = Arc::new(MemoryStore::new());
    let dead = Innertube::new(
        Arc::new(Mutex::new(session())),
        store,
        Url::parse("http://127.0.0.1:1").unwrap(),
    );
    clean(dead.browse(&id, Some(MARK)).await.map(drop), "dead browse");
    clean(
        dead.more(MoreKind::Search, MARK).await.map(drop),
        "dead more",
    );
}

#[tokio::test]
async fn usable_concurrently() {
    // The engine browses while the queue source runs `next` on the same `Innertube`.
    fn shareable<T: Send + Sync>() {}
    shareable::<Innertube>();
    fn send<T: Send>(_: &T) {}

    let rig = rig().await;
    endpoint("browse", json_answer(HOME))
        .mount(&rig.server)
        .await;
    endpoint("next", json_answer(NEXT_FOR_LYRICS))
        .mount(&rig.server)
        .await;
    endpoint("search", json_answer(SEARCH_MIXED))
        .mount(&rig.server)
        .await;
    let api = Arc::new(rig.api);
    let browse = api.browse("FEmusic_home", None);
    send(&browse);
    let search = api.search("q", None);
    send(&search);
    let lyrics_api = api.clone();
    let lyrics = tokio::spawn(async move { lyrics_api.lyrics("fakeV000797").await });
    let next = api.next(NextRequest {
        video_id: Some("fakeV000797".into()),
        ..NextRequest::default()
    });
    let (b, s, n) = tokio::join!(browse, search, next);
    b.unwrap();
    s.unwrap();
    n.unwrap();
    // The lyrics browse gets the home fixture here: no lyrics shelf, so none.
    assert_eq!(lyrics.await.unwrap(), Ok(None));
}
