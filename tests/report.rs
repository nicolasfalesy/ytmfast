//! Play reports (history) against a local wiremock server.
//!
//! The server stands in for YouTube: the music web client's `player` answer (with its
//! play-history links pointing back at the server), the history pings themselves, and, for
//! the engine tests, the song's audio. Its http base URL is injected through
//! `Innertube::new` and `Engine::download_from_test_base`, the only ways past the https
//! allowlist (ruling R7). Nothing here talks to YouTube; every value is made up.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};
use tokio::sync::broadcast;
use url::Url;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};
use ytmfast::audio::player::AudioPlayer;
use ytmfast::audio::sink::NullSink;
use ytmfast::auth::{Cookie, MemoryStore, Session};
use ytmfast::engine::{Engine, EngineCmd, EngineEvent, PlayState, QueueSource};
use ytmfast::error::Error;
use ytmfast::innertube::{Innertube, NextPage, NextRequest, SongItem, Tracking, clients};
use ytmfast::report::{self, PlayReport, ReportApi, Reporter};
use ytmfast::streams::{Resolver, Stream, TrackMeta};

const VIDEO: &str = "testvideo01";
const OTHER: &str = "testvideo02";
/// The signature timestamp the fake player script would give.
const STS: u32 = 20725;
/// The visitor id in the fake `player` answer.
const VISITOR: &str = "CgtGYWtlVmlzaXRvchIEGgAgOQ%3D%3D";
const CPN: &str = "AbCdEfGhIjKl-_09";
const OPUS: &str = "audio/webm; codecs=\"opus\"";

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

/// The `.youtube.com` SAPISID signs the requests; the `127.0.0.1` one is what the wiremock
/// host gets in its `Cookie` header.
fn session() -> Session {
    Session {
        cookies: vec![
            cookie(".youtube.com", "SAPISID", "fake-sapisid", true),
            cookie("127.0.0.1", "SAPISID", "fake-sapisid", false),
        ],
        account: None,
    }
}

/// The real request code (`Innertube::play_tracking`, `Innertube::ping`) against the test
/// server, with a fixed signature timestamp in place of the player script's.
struct TestApi {
    api: Innertube,
}

#[async_trait]
impl ReportApi for TestApi {
    async fn tracking(&self, video_id: &str) -> Result<Tracking, Error> {
        self.api.play_tracking(video_id, STS).await
    }
    async fn ping(&self, url: Url, visitor_data: Option<String>) -> Result<(), Error> {
        self.api.ping(&url, visitor_data.as_deref()).await
    }
}

/// The music web client's `player` answer for the asked song, with its history links on
/// `playback` and `watchtime` (origins, or full links for an off-allowlist test).
struct PlayerAnswer {
    playback: String,
    watchtime: String,
    delay: Duration,
}

impl Respond for PlayerAnswer {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&req.body).unwrap();
        let id = body["videoId"].as_str().unwrap_or("").to_string();
        let answer = json!({
            "responseContext": {"visitorData": VISITOR},
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": id, "lengthSeconds": "2"},
            "playbackTracking": {
                "videostatsPlaybackUrl": {"baseUrl":
                    format!("{}/api/stats/playback?cl=1&docid={id}&ei=fake", self.playback)},
                "videostatsWatchtimeUrl": {"baseUrl":
                    format!("{}/api/stats/watchtime?cl=1&docid={id}&ei=fake", self.watchtime)},
            }
        });
        ResponseTemplate::new(200)
            .set_body_json(answer)
            .set_delay(self.delay)
    }
}

async fn mount_player(server: &MockServer, answer: PlayerAnswer) {
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .respond_with(answer)
        .mount(server)
        .await;
}

/// Mounts the history links answering `status` (204 like YouTube, or a failure).
async fn mount_stats(server: &MockServer, status: u16) {
    Mock::given(method("GET"))
        .and(path_regex("^/api/stats/"))
        .respond_with(ResponseTemplate::new(status))
        .mount(server)
        .await;
}

struct Rig {
    server: MockServer,
    reporter: Reporter,
}

/// A server with the `player` answer (history links back on itself) and 204 pings.
async fn rig() -> Rig {
    rig_with(Duration::ZERO, 204).await
}

async fn rig_with(delay: Duration, status: u16) -> Rig {
    let server = MockServer::start().await;
    mount_player(
        &server,
        PlayerAnswer {
            playback: server.uri(),
            watchtime: server.uri(),
            delay,
        },
    )
    .await;
    mount_stats(&server, status).await;
    let reporter = reporter_for(&server);
    Rig { server, reporter }
}

fn reporter_for(server: &MockServer) -> Reporter {
    let base = Url::parse(&server.uri()).unwrap();
    let api = Innertube::new(
        Arc::new(Mutex::new(session())),
        Arc::new(MemoryStore::new()),
        base,
    );
    Reporter::new(Arc::new(TestApi { api }))
}

/// One history ping as the server saw it.
#[derive(Debug, Clone)]
struct Ping {
    kind: String,
    query: Vec<(String, String)>,
    headers: HashMap<String, String>,
}

impl Ping {
    fn get(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn num(&self, key: &str) -> f64 {
        self.get(key).unwrap().parse().unwrap()
    }
    /// (st, et, cmt) of a watch-time ping.
    fn range(&self) -> (f64, f64, f64) {
        (self.num("st"), self.num("et"), self.num("cmt"))
    }
}

async fn pings(server: &MockServer) -> Vec<Ping> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().starts_with("/api/stats/"))
        .map(|r| Ping {
            kind: r.url.path().trim_start_matches("/api/stats/").to_string(),
            query: r.url.query_pairs().into_owned().collect(),
            headers: r
                .headers
                .iter()
                .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
                .collect(),
        })
        .collect()
}

async fn player_requests(server: &MockServer) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path() == "/youtubei/v1/player")
        .collect()
}

/// Waits up to 5 s for the server to have seen pings that satisfy `done`.
async fn wait_for(server: &MockServer, what: &str, done: impl Fn(&[Ping]) -> bool) -> Vec<Ping> {
    let t = std::time::Instant::now();
    loop {
        let p = pings(server).await;
        if done(&p) {
            return p;
        }
        assert!(t.elapsed() < Duration::from_secs(5), "{what}: {p:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn of_kind<'a>(p: &'a [Ping], kind: &str) -> Vec<&'a Ping> {
    p.iter().filter(|p| p.kind == kind).collect()
}

fn finals(p: &[Ping]) -> usize {
    p.iter().filter(|p| p.get("final") == Some("1")).count()
}

/// Ticks once per second of play, from `from` to `to` (both included).
fn tick_through(r: &PlayReport, from: u32, to: u32) {
    for s in from..=to {
        r.tick(f64::from(s));
    }
}

// ---- the report on its own ---------------------------------------------------------------

#[tokio::test]
async fn playback_ping_matches_recipe() {
    let r = rig().await;
    let report = r.reporter.start(VIDEO, CPN.into(), 0.0, 2.0);
    let p = wait_for(&r.server, "the playback ping", |p| {
        !of_kind(p, "playback").is_empty()
    })
    .await;

    // The music web client's `player` request, with the player script's timestamp.
    let players = player_requests(&r.server).await;
    assert_eq!(players.len(), 1);
    let req = &players[0];
    let body: Value = serde_json::from_slice(&req.body).unwrap();
    assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
    assert_eq!(body["videoId"], VIDEO);
    assert_eq!(
        body["playbackContext"]["contentPlaybackContext"]["signatureTimestamp"],
        STS
    );
    assert_eq!(req.headers.get("x-youtube-client-name").unwrap(), "67");
    assert_eq!(
        req.headers.get("origin").unwrap(),
        "https://music.youtube.com"
    );

    // The ping: the base link plus exactly ver, c and cpn (the spike's variant 2).
    let ping = of_kind(&p, "playback")[0];
    let query: Vec<(&str, &str)> = ping
        .query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        query,
        vec![
            ("cl", "1"),
            ("docid", VIDEO),
            ("ei", "fake"),
            ("ver", "2"),
            ("c", "WEB_REMIX"),
            ("cpn", CPN),
        ]
    );
    let h = &ping.headers;
    assert_eq!(h["user-agent"], clients::WEB_REMIX.user_agent);
    assert_eq!(h["origin"], "https://music.youtube.com");
    assert_eq!(h["x-origin"], "https://music.youtube.com");
    assert_eq!(h["referer"], "https://music.youtube.com/");
    assert_eq!(h["x-goog-authuser"], "0");
    assert_eq!(h["x-goog-visitor-id"], VISITOR);
    assert!(h["cookie"].contains("SAPISID=fake-sapisid"), "{h:?}");
    // SAPISIDHASH signed for the music origin.
    let signed = h["authorization"]
        .strip_prefix("SAPISIDHASH ")
        .expect("authorization scheme");
    let (ts, hash) = signed.split_once('_').unwrap();
    let hash = hash.split_whitespace().next().unwrap();
    let want = Sha1::digest(format!("{ts} fake-sapisid https://music.youtube.com").as_bytes());
    assert_eq!(hash, format!("{want:x}"));
    report.end(0.5);
}

#[tokio::test]
async fn watchtime_cadence_10_20_30_then_40() {
    let r = rig().await;
    let report = r.reporter.start(VIDEO, CPN.into(), 0.0, 200.0);
    tick_through(&report, 1, 150);
    report.end(150.5);
    let p = wait_for(&r.server, "the final ping", |p| finals(p) == 1).await;

    let watch = of_kind(&p, "watchtime");
    let ranges: Vec<(f64, f64, f64)> = watch.iter().map(|w| w.range()).collect();
    assert_eq!(
        ranges,
        vec![
            (0.0, 10.0, 10.0),
            (10.0, 20.0, 20.0),
            (20.0, 30.0, 30.0),
            (30.0, 70.0, 70.0),
            (70.0, 110.0, 110.0),
            (110.0, 150.0, 150.0),
            (150.0, 150.5, 150.5),
        ]
    );
    for w in &watch {
        assert_eq!(w.get("ver"), Some("2"));
        assert_eq!(w.get("c"), Some("WEB_REMIX"));
        assert_eq!(w.get("cpn"), Some(CPN));
        assert_eq!(w.get("len"), Some("200"));
        assert_eq!(w.get("state"), Some("playing"));
        assert_eq!(w.get("docid"), Some(VIDEO));
        assert_eq!(w.headers["x-goog-visitor-id"], VISITOR);
        assert_eq!(w.headers["referer"], "https://music.youtube.com/");
    }
    // `final=1` only on the last one.
    assert_eq!(watch.last().unwrap().get("final"), Some("1"));
    assert_eq!(finals(&p), 1);
    assert_eq!(of_kind(&p, "playback").len(), 1);
}

#[tokio::test]
async fn pause_and_seek_send_ranges() {
    let r = rig().await;
    let report = r.reporter.start(VIDEO, CPN.into(), 0.0, 200.0);
    tick_through(&report, 1, 5);
    report.pause(5.25);
    // Paused: nothing is played, so nothing counts towards the next 10 s.
    report.resume(5.25);
    report.tick(6.25);
    report.tick(7.25);
    report.seek(7.5, 60.0);
    report.tick(61.0);
    report.tick(62.0);
    report.end(63.5);
    let p = wait_for(&r.server, "the final ping", |p| finals(p) == 1).await;

    let watch = of_kind(&p, "watchtime");
    let got: Vec<_> = watch.iter().map(|w| (w.range(), w.get("state"))).collect();
    assert_eq!(
        got,
        vec![
            ((0.0, 5.25, 5.25), Some("paused")),
            ((5.25, 7.5, 7.5), Some("playing")),
            ((60.0, 63.5, 63.5), Some("playing")),
        ]
    );
    assert_eq!(finals(&p), 1);
    assert_eq!(watch.last().unwrap().get("final"), Some("1"));
    // No cadence ping: the ticks never saw 10 s of play (the seek's jump adds none).
    assert_eq!(watch.len(), 3);
}

#[tokio::test]
async fn ping_urls_pass_allowlist() {
    // The history links point at another host: a second local server stands in for it (any
    // host but the test base is off the allowlist here), and an https host off the list.
    let server = MockServer::start().await;
    let elsewhere = MockServer::start().await;
    mount_stats(&elsewhere, 204).await;
    mount_player(
        &server,
        PlayerAnswer {
            playback: elsewhere.uri(),
            watchtime: "https://stats.example".into(),
            delay: Duration::ZERO,
        },
    )
    .await;
    mount_stats(&server, 204).await;
    let reporter = reporter_for(&server);
    let report = reporter.start(VIDEO, CPN.into(), 0.0, 200.0);
    tick_through(&report, 1, 12);
    report.end(12.5);
    // The player answer was fetched...
    let t = std::time::Instant::now();
    while player_requests(&server).await.is_empty() {
        assert!(t.elapsed() < Duration::from_secs(5), "no player request");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    // ...but no ping went anywhere.
    assert!(pings(&elsewhere).await.is_empty());
    assert!(pings(&server).await.is_empty());

    // And the production client refuses an off-allowlist or plain-http link before sending.
    let prod = Innertube::production(
        Arc::new(Mutex::new(session())),
        Arc::new(MemoryStore::new()),
    );
    for bad in [
        "https://stats.example/api/stats/playback?docid=x",
        "https://youtube.com.example/api/stats/playback",
        "http://s.youtube.com/api/stats/playback",
        &format!("{}/api/stats/playback", elsewhere.uri()),
    ] {
        let url = Url::parse(bad).unwrap();
        let e = prod.ping(&url, Some(VISITOR)).await.unwrap_err();
        assert_eq!(e.code(), "internal", "{bad}");
        assert!(!e.to_string().contains("://"), "R6: {e}");
    }
    assert!(pings(&elsewhere).await.is_empty());
}

#[tokio::test]
async fn a_song_stopped_before_its_report_went_gets_no_ping() {
    // The `player` answer is slow; the song is gone before it comes.
    let r = rig_with(Duration::from_millis(400), 204).await;
    let report = r.reporter.start(VIDEO, CPN.into(), 0.0, 200.0);
    report.end(0.3);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(pings(&r.server).await.is_empty());
}

#[test]
fn cpn_is_16_url_safe_chars_and_new_each_time() {
    let a = report::cpn();
    let b = report::cpn();
    for c in [&a, &b] {
        assert_eq!(c.len(), 16);
        assert!(
            c.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "{c}"
        );
    }
    assert_ne!(a, b);
}

// ---- through the engine ----------------------------------------------------------------

/// Links to the test server's `/audio/{id}`, two-second songs; `fail` ids are unplayable.
struct FakeResolver {
    base: Url,
    fail: Vec<String>,
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn resolve(&self, id: &str) -> Result<Stream, Error> {
        if self.fail.iter().any(|f| f == id) {
            return Err(Error::Unavailable("Video unavailable".into()));
        }
        Ok(Stream {
            video_id: id.into(),
            url: format!("{}audio/{id}", self.base),
            itag: 251,
            mime: OPUS.into(),
            content_length: None,
            expires_unix: u64::MAX,
            loudness_db: None,
            meta: TrackMeta {
                title: format!("Song {id}"),
                artist: "Artist".into(),
                length_seconds: 2,
                thumbnail: None,
            },
            tracking: Tracking::default(),
        })
    }
    async fn resolve_fresh(&self, id: &str) -> Result<Stream, Error> {
        self.resolve(id).await
    }
}

/// The playlist `PLtest` holds VIDEO then OTHER; anything else has no queue.
struct FakeSource;

fn song(id: &str) -> SongItem {
    SongItem {
        video_id: id.into(),
        title: format!("Title {id}"),
        artists: vec!["Artist".into()],
        length_seconds: 2,
        ..SongItem::default()
    }
}

#[async_trait]
impl QueueSource for FakeSource {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        if req.playlist_id.as_deref() == Some("PLtest") {
            return Ok(NextPage {
                items: vec![song(VIDEO), song(OTHER)],
                ..NextPage::default()
            });
        }
        Err(Error::Unavailable("YouTube sent no queue".into()))
    }
    /// Unknown: the reports don't depend on it.
    async fn song_next(&self, _: &str) -> Result<ytmfast::innertube::SongNext, Error> {
        Ok(ytmfast::innertube::SongNext::default())
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        Ok(())
    }
}

struct EngineRig {
    server: MockServer,
    cmds: tokio::sync::mpsc::Sender<EngineCmd>,
    events: broadcast::Receiver<EngineEvent>,
}

/// A real-time engine (null output) whose songs come from the test server, with reports on.
async fn engine_rig(stats_status: u16, fail: &[&str]) -> EngineRig {
    let server = MockServer::start().await;
    mount_player(
        &server,
        PlayerAnswer {
            playback: server.uri(),
            watchtime: server.uri(),
            delay: Duration::ZERO,
        },
    )
    .await;
    mount_stats(&server, stats_status).await;
    let audio = std::fs::read(format!(
        "{}/tests/fixtures/sine440_48k.webm",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    Mock::given(method("GET"))
        .and(path_regex("^/audio/"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(audio))
        .mount(&server)
        .await;
    let base = Url::parse(&format!("{}/", server.uri())).unwrap();
    let resolver = Arc::new(FakeResolver {
        base: base.clone(),
        fail: fail.iter().map(|s| s.to_string()).collect(),
    });
    let player = AudioPlayer::spawn(Box::new(NullSink::realtime()));
    let (mut engine, cmds, events) = Engine::new(resolver, Arc::new(FakeSource), player);
    engine.download_from_test_base(base);
    engine.report_with(reporter_for(&server));
    let events = events.subscribe();
    tokio::spawn(engine.run());
    EngineRig {
        server,
        cmds,
        events,
    }
}

impl EngineRig {
    async fn play(&self, video_id: Option<&str>, playlist_id: Option<&str>) {
        self.cmds
            .send(EngineCmd::Play {
                video_id: video_id.map(String::from),
                playlist_id: playlist_id.map(String::from),
                index: None,
                params: None,
                start_seconds: 0.0,
            })
            .await
            .unwrap();
    }

    /// Every event until the engine stops (the queue ran out), failing after 10 s.
    async fn until_stopped(&mut self) -> Vec<EngineEvent> {
        let mut seen = Vec::new();
        let mut playing = false;
        loop {
            let e = tokio::time::timeout(Duration::from_secs(10), self.events.recv())
                .await
                .expect("the engine stops within 10 s")
                .expect("the event channel is open");
            let state = match &e {
                EngineEvent::State(s) => Some(s.state),
                _ => None,
            };
            seen.push(e);
            match state {
                Some(PlayState::Playing) => playing = true,
                // A stop after playing, or a stop with nothing playable (an error came first).
                Some(PlayState::Stopped) if playing || seen.iter().any(is_error) => {
                    return seen;
                }
                _ => {}
            }
        }
    }
}

fn is_error(e: &EngineEvent) -> bool {
    matches!(e, EngineEvent::Error { .. })
}

#[tokio::test]
async fn playback_ping_once_at_start() {
    let mut r = engine_rig(204, &[]).await;
    r.play(Some(VIDEO), None).await;
    let events = r.until_stopped().await;
    assert!(!events.iter().any(is_error), "{events:?}");
    // The end ping follows the stop.
    let p = wait_for(&r.server, "the end ping", |p| finals(p) == 1).await;
    let playback = of_kind(&p, "playback");
    assert_eq!(playback.len(), 1, "{p:#?}");
    assert_eq!(playback[0].get("docid"), Some(VIDEO));
    let cpn = playback[0].get("cpn").unwrap();
    assert_eq!(cpn.len(), 16);
    // The end ping is the same play's, at the song's end.
    let end = of_kind(&p, "watchtime");
    let end = end.last().unwrap();
    assert_eq!(end.get("cpn"), Some(cpn));
    assert_eq!(end.get("len"), Some("2"));
    assert!(end.num("et") > 1.5, "{end:?}");
    // One extra `player` request for the song, for its history links.
    assert_eq!(player_requests(&r.server).await.len(), 1);
}

#[tokio::test]
async fn failed_ping_never_stops_playback() {
    // Every history link fails: both songs still play to the end, with no error event.
    let mut r = engine_rig(500, &[]).await;
    r.play(None, Some("PLtest")).await;
    let events = r.until_stopped().await;
    assert!(!events.iter().any(is_error), "{events:?}");
    let played: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::State(s) if s.state == PlayState::Playing => s.video_id.as_deref(),
            _ => None,
        })
        .collect();
    assert!(
        played.contains(&VIDEO) && played.contains(&OTHER),
        "{played:?}"
    );
    // The pings were tried (and failed), for both songs.
    let p = wait_for(&r.server, "both playback pings", |p| {
        of_kind(p, "playback").len() == 2
    })
    .await;
    assert_eq!(of_kind(&p, "playback").len(), 2);
}

#[tokio::test]
async fn no_ping_for_a_song_that_never_started() {
    // The song can't be played: it is never heard, so it is never reported.
    let mut r = engine_rig(204, &[VIDEO]).await;
    r.play(Some(VIDEO), None).await;
    let events = r.until_stopped().await;
    assert!(events.iter().any(is_error));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(pings(&r.server).await.is_empty());
    assert!(player_requests(&r.server).await.is_empty());
}

#[tokio::test]
async fn advanced_song_gets_its_own_ping() {
    let mut r = engine_rig(204, &[]).await;
    r.play(None, Some("PLtest")).await;
    let events = r.until_stopped().await;
    assert!(!events.iter().any(is_error), "{events:?}");
    // The second song came by the gapless handover (`Advanced`), never through a load.
    let buffering_other = events.iter().any(|e| {
        matches!(e, EngineEvent::State(s)
            if s.state == PlayState::Buffering && s.video_id.as_deref() == Some(OTHER))
    });
    assert!(!buffering_other, "OTHER was loaded, not handed over");

    let p = wait_for(&r.server, "two plays, each ended", |p| {
        of_kind(p, "playback").len() == 2 && finals(p) == 2
    })
    .await;
    let playback = of_kind(&p, "playback");
    let docs: Vec<&str> = playback.iter().map(|p| p.get("docid").unwrap()).collect();
    assert_eq!(docs, vec![VIDEO, OTHER]);
    let (a, b) = (
        playback[0].get("cpn").unwrap(),
        playback[1].get("cpn").unwrap(),
    );
    assert_ne!(a, b, "each play has its own cpn");
    // The first song's end ping went at its end, under its own cpn.
    let first_end = p
        .iter()
        .find(|p| p.get("final") == Some("1") && p.get("docid") == Some(VIDEO))
        .unwrap();
    assert_eq!(first_end.get("cpn"), Some(a));
    assert!(first_end.num("et") > 1.5, "{first_end:?}");
}
