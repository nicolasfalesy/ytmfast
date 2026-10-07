//! Browsing over the control socket (step 3): `browse`, `search`, `more`, `play {endpoint}`,
//! `playPage` and `lyrics`, end to end on a socket in a temp folder, with a fake engine side and a fake
//! `Browser`. Never the network, never the real account.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast, mpsc};
use tokio::task::JoinHandle;
use ytmfast::audio::player::AudioPlayer;
use ytmfast::audio::sink::NullSink;
use ytmfast::browse::{
    self, Browser, Endpoint, Kind, Lyrics, MorePage, Page, PageHeader, Row, SearchPage, Section,
    WatchEndpoint, WatchPlaylistEndpoint,
};
use ytmfast::control::lyrics::{self, LyricsCache, LyricsTabs};
use ytmfast::control::{self, Exit, Options};
use ytmfast::engine::{
    Engine, EngineCmd, EngineEvent, KnownTab, PlayState, QueueSource, QueueView, Status,
};
use ytmfast::error::Error;
use ytmfast::innertube::{MoreKind, NextPage, NextRequest, SongItem, SongNext};
use ytmfast::lyrics::{Fetched, Found, LyricsWeb, SongFacts};
use ytmfast::queue::Repeat;
use ytmfast::streams::{Resolver, Stream};

const SONG: &str = "dQw4w9WgXcQ";
const WAIT: Duration = Duration::from_secs(10);

/// What the fake browser was asked, in order: `"browse <id> <params>"`, `"search <query>
/// <params>"`, `"more <kind> <token>"`, `"next <videoId>"` (a lyrics' own `next`) and
/// `"lyrics <MPLYt id>"`.
type Calls = Arc<Mutex<Vec<String>>>;

/// Answers from fixed pages. With a gate, every call first waits for a permit, so a test can
/// hold requests in flight.
#[derive(Default)]
struct FakeBrowser {
    pages: HashMap<String, Page>,
    /// What every call answers instead, when set.
    fail: Option<Error>,
    gate: Option<Arc<Semaphore>>,
    calls: Calls,
    /// Lyrics tabs by video id (a song not here has none).
    tabs: HashMap<String, String>,
    /// Lyrics by lyrics page id (a page not here has no text).
    lyrics: HashMap<String, Lyrics>,
}

impl FakeBrowser {
    async fn wait(&self) {
        if let Some(g) = &self.gate {
            g.acquire().await.unwrap().forget();
        }
    }
}

#[async_trait]
impl Browser for FakeBrowser {
    async fn browse(&self, browse_id: &str, params: Option<&str>) -> Result<Page, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("browse {browse_id} {}", params.unwrap_or("-")));
        self.wait().await;
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(self.pages.get(browse_id).cloned().unwrap_or_default())
    }

    async fn search(&self, query: &str, params: Option<&str>) -> Result<SearchPage, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("search {query} {}", params.unwrap_or("-")));
        self.wait().await;
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(SearchPage {
            sections: vec![section("Top result", vec![song_row(SONG)])],
            chips: Vec::new(),
        })
    }

    async fn more(&self, kind: MoreKind, token: &str) -> Result<MorePage, Error> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("more {kind:?} {token}"));
        self.wait().await;
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(MorePage {
            items: vec![song_row(SONG)],
            sections: Vec::new(),
            cont: String::new(),
        })
    }

    async fn song_next(&self, video_id: &str) -> Result<SongNext, Error> {
        self.calls.lock().unwrap().push(format!("next {video_id}"));
        self.wait().await;
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(SongNext {
            like: None,
            lyrics_tab: self.tabs.get(video_id).cloned(),
        })
    }

    async fn lyrics_page(&self, page_id: &str) -> Result<Option<Lyrics>, Error> {
        self.calls.lock().unwrap().push(format!("lyrics {page_id}"));
        self.wait().await;
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        Ok(self.lyrics.get(page_id).cloned())
    }
}

/// What the engine's per-song cache knows of each song's Lyrics tab, faked: the rig's engine
/// side keeps it as the real engine does (`EngineCmd::LyricsTab` and `LearnSong`).
type Known = Arc<Mutex<HashMap<String, KnownTab>>>;

fn song_row(id: &str) -> Row {
    Row {
        title: "Song".into(),
        subtitle: "Artist • Album".into(),
        thumb: "https://lh3.googleusercontent.com/x=w120-h120".into(),
        video_id: id.into(),
        play: Some(Endpoint::Watch(WatchEndpoint {
            video_id: Some(id.into()),
            ..WatchEndpoint::default()
        })),
        duration: "3:33".into(),
        kind: Kind::Song,
        ..Row::default()
    }
}

/// A row that opens something but plays nothing (an artist tile).
fn page_row(browse_id: &str) -> Row {
    Row {
        title: "Artist".into(),
        browse_id: browse_id.into(),
        kind: Kind::Artist,
        ..Row::default()
    }
}

fn section(title: &str, items: Vec<Row>) -> Section {
    Section {
        title: title.into(),
        items,
        cont: String::new(),
        more: None,
    }
}

fn status() -> Status {
    Status {
        state: PlayState::Stopped,
        video_id: None,
        meta: None,
        album: None,
        album_id: String::new(),
        queue_id: None,
        position: 0.0,
        volume: 1.0,
        muted: false,
        shuffle: false,
        repeat: Repeat::Off,
        liked: None,
    }
}

/// A fake engine side (as in `tests/control.rs`): answers `Status` and `QueueGet`, passes
/// every other command to the test.
struct Rig {
    path: PathBuf,
    events: broadcast::Sender<EngineEvent>,
    commands: mpsc::UnboundedReceiver<EngineCmd>,
    calls: Calls,
    known: Known,
    serve: JoinHandle<Exit>,
    _dir: TempDir,
}

/// What the engine side knows of each song for lyrics (`EngineCmd::LyricsSong`).
type Facts = HashMap<String, SongFacts>;

fn rig(browser: FakeBrowser) -> Rig {
    rig_with(browser, Facts::new(), None)
}

fn rig_with(mut browser: FakeBrowser, facts: Facts, web: Option<Arc<dyn LyricsWeb>>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let (cmd_tx, mut cmd_rx) = mpsc::channel(32);
    let (events, _) = broadcast::channel(64);
    let (seen_tx, commands) = mpsc::unbounded_channel();
    let known = Known::default();
    let engine_known = known.clone();
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                EngineCmd::Status(reply) => {
                    let _ = reply.send(status());
                }
                EngineCmd::QueueGet(reply) => {
                    let _ = reply.send(QueueView {
                        items: Vec::new().into(),
                        current_id: None,
                        shuffle: false,
                        repeat: Repeat::Off,
                    });
                }
                // No play ever comes between: a late play always goes in (the real engine's
                // rule is tested in `play_page_never_overrides_a_newer_play`).
                EngineCmd::PlayEpoch(reply) => {
                    let _ = reply.send(7);
                }
                EngineCmd::PlayIfLatest {
                    epoch,
                    play,
                    played,
                } => {
                    assert_eq!(epoch, 7);
                    let _ = played.send(true);
                    let _ = seen_tx.send(*play);
                }
                EngineCmd::LyricsTab { video_id, reply } => {
                    let _ = reply.send(engine_known.lock().unwrap().get(&video_id).cloned());
                }
                EngineCmd::LyricsSong {
                    video_id, reply, ..
                } => {
                    let _ = reply.send(facts.get(&video_id).cloned());
                }
                EngineCmd::LearnSong { video_id, next } => {
                    engine_known.lock().unwrap().insert(
                        video_id,
                        KnownTab {
                            page: next.lyrics_tab,
                            at: tokio::time::Instant::now(),
                        },
                    );
                }
                EngineCmd::Quit => return,
                other => {
                    let _ = seen_tx.send(other);
                }
            }
        }
    });
    let calls = Calls::default();
    browser.calls = calls.clone();
    // On mains power, read from a folder of our own rather than the machine's.
    let power = dir.path().join("power");
    let ac = power.join("AC");
    std::fs::create_dir_all(&ac).unwrap();
    std::fs::write(ac.join("type"), "Mains\n").unwrap();
    std::fs::write(ac.join("online"), "1\n").unwrap();
    let options = Options {
        browser: Some(Arc::new(browser)),
        lyrics_web: web,
        power_supply_root: power,
        ..Options::default()
    };
    let serve = tokio::spawn(control::serve(listener, cmd_tx, events.clone(), options));
    Rig {
        path,
        events,
        commands,
        calls,
        known,
        serve,
        _dir: dir,
    }
}

impl Rig {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// The next command the engine got.
    async fn command(&mut self) -> EngineCmd {
        tokio::time::timeout(WAIT, self.commands.recv())
            .await
            .expect("a command in time")
            .expect("the engine side is running")
    }
}

struct Client {
    lines: Lines<BufReader<OwnedReadHalf>>,
    write: OwnedWriteHalf,
}

async fn connect(path: &Path) -> Client {
    let (read, write) = UnixStream::connect(path).await.unwrap().into_split();
    Client {
        lines: BufReader::new(read).lines(),
        write,
    }
}

/// Keeps a paused test clock still until dropped (as in `tests/control.rs`): tokio advances a
/// paused clock whenever the runtime parks, even when that park is what delivers a socket's
/// readiness, so the idle timer could fire under a request in flight. It never auto-advances
/// while a blocking task runs. Harmless with a real clock.
struct ClockHold(#[allow(dead_code)] std::sync::mpsc::Sender<()>);

fn hold_clock() -> ClockHold {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    tokio::task::spawn_blocking(move || {
        let _ = rx.recv();
    });
    ClockHold(tx)
}

impl Client {
    async fn send(&mut self, v: Value) {
        let _hold = hold_clock();
        let mut line = v.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.unwrap();
    }

    /// The next line, or None at the end of the connection.
    async fn next(&mut self) -> Option<Value> {
        let _hold = hold_clock();
        let line = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("no line in time")
            .ok()??;
        Some(serde_json::from_str(&line).unwrap())
    }

    /// The reply to `id`, skipping events.
    async fn reply(&mut self, id: u64) -> Value {
        loop {
            let v = self.next().await.expect("connection closed");
            if v.get("id") == Some(&json!(id)) {
                return v;
            }
        }
    }

    async fn ask(&mut self, id: u64, cmd: &str, args: Value) -> Value {
        self.send(json!({"id": id, "cmd": cmd, "args": args})).await;
        self.reply(id).await
    }
}

fn code(reply: &Value) -> &str {
    reply["error"]["code"].as_str().unwrap_or("<ok>")
}

fn play_of(cmd: &EngineCmd) -> (Option<&str>, Option<&str>, Option<usize>, Option<&str>, f64) {
    match cmd {
        EngineCmd::Play {
            video_id,
            playlist_id,
            index,
            params,
            start_seconds,
        } => (
            video_id.as_deref(),
            playlist_id.as_deref(),
            *index,
            params.as_deref(),
            *start_seconds,
        ),
        other => panic!("not a play: {other:?}"),
    }
}

/// Review Focus 3: a row's endpoint sent back with extra or odd fields is cleaned (unknown keys
/// dropped) or refused (a malformed id, index or params), never passed through raw.
#[tokio::test]
async fn play_endpoint_is_sanitised() {
    let mut r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    // Unknown keys at both levels, YouTube's extra fields and a start time: dropped.
    let v = c
        .ask(
            1,
            "play",
            json!({"endpoint": {"watchEndpoint": {
                "videoId": SONG, "playlistId": "PLfake", "index": 3, "params": "wAEB+/=",
                "startTimeSeconds": 99, "playerParams": "x y", "loggingContext": {"a": 1},
                "watchEndpointMusicSupportedConfigs": {}},
                "clickTrackingParams": "zzz"}}),
        )
        .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        play_of(&r.command().await),
        (Some(SONG), Some("PLfake"), Some(3), Some("wAEB+/="), 0.0)
    );
    // Empty strings are "none", the way rows carry them.
    let v = c
        .ask(
            2,
            "play",
            json!({"endpoint": {"watchPlaylistEndpoint": {"playlistId": "PLfake", "params": ""}}}),
        )
        .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        play_of(&r.command().await),
        (None, Some("PLfake"), None, None, 0.0)
    );

    // Refused, and nothing reaches the engine.
    let huge = json!(1u64 << 40);
    for (n, endpoint) in [
        json!({"watchEndpoint": {"videoId": SONG, "playlistId": "PLfake", "index": huge}}),
        json!({"watchEndpoint": {"videoId": SONG, "playlistId": "PLfake", "index": -1}}),
        json!({"watchEndpoint": {"videoId": SONG, "playlistId": "PLfake", "index": 1.5}}),
        json!({"watchEndpoint": {"videoId": "../etc/passw", "playlistId": "PLfake"}}),
        json!({"watchEndpoint": {"videoId": SONG, "playlistId": "PL fake"}}),
        json!({"watchEndpoint": {"videoId": SONG, "params": "a\"b"}}),
        json!({"watchEndpoint": {"videoId": 5}}),
        json!({"watchEndpoint": {}}),
        json!({"watchPlaylistEndpoint": {"params": "abc"}}),
        json!({"watchPlaylistEndpoint": {"playlistId": "PLfake", "params": "a b"}}),
        json!({"browseEndpoint": {"browseId": "UCfake"}}),
        json!({}),
        json!("watchEndpoint"),
        json!([{"watchEndpoint": {"videoId": SONG}}]),
        // Both kinds at once: which one the user meant is not clear, so neither is guessed.
        json!({"watchEndpoint": {"videoId": SONG},
               "watchPlaylistEndpoint": {"playlistId": "PLfake"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let id = 10 + n as u64;
        let v = c.ask(id, "play", json!({ "endpoint": endpoint })).await;
        assert_eq!(code(&v), "bad_request", "{endpoint}: {v}");
    }
    // An endpoint can't be mixed with the plain form.
    for (n, extra) in [
        json!({"videoId": SONG}),
        json!({"playlistId": "PLfake"}),
        json!({"index": 1}),
        json!({"startSeconds": 5}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut args = extra.clone();
        args["endpoint"] = json!({"watchEndpoint": {"videoId": SONG}});
        let v = c.ask(40 + n as u64, "play", args).await;
        assert_eq!(code(&v), "bad_request", "{extra}: {v}");
    }
    assert!(
        r.commands.try_recv().is_err(),
        "a refused play reached the engine"
    );
}

#[tokio::test]
async fn play_watch_endpoint_with_playlist_plays_at_song() {
    let mut r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    // A playlist's row: the list, at that song.
    let v = c
        .ask(
            1,
            "play",
            json!({"endpoint": {"watchEndpoint": {"videoId": SONG, "playlistId": "PLfake"}}}),
        )
        .await;
    assert_eq!(v, json!({"id": 1, "ok": true, "data": {}}));
    assert_eq!(
        play_of(&r.command().await),
        (Some(SONG), Some("PLfake"), None, None, 0.0)
    );
    // A song row with no list: the song and its radio, as a play by id (step 2). Its params
    // (YouTube's player flavour for the song) are not a list's.
    let v = c
        .ask(
            2,
            "play",
            json!({"endpoint": {"watchEndpoint": {"videoId": SONG, "params": "wAEB"}}}),
        )
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        play_of(&r.command().await),
        (Some(SONG), None, None, None, 0.0)
    );
    // A list without a song: from its index.
    let v = c
        .ask(
            3,
            "play",
            json!({"endpoint": {"watchEndpoint": {"playlistId": "PLfake", "index": 4}}}),
        )
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        play_of(&r.command().await),
        (None, Some("PLfake"), Some(4), None, 0.0)
    );
}

#[tokio::test]
async fn play_watch_playlist_endpoint_uses_params() {
    let mut r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    // An artist's shuffle button: the list with its params.
    let v = c
        .ask(
            1,
            "play",
            json!({"endpoint": {"watchPlaylistEndpoint":
                {"playlistId": "RDAOfake", "params": "wAEB8gECKAE%3D"}}}),
        )
        .await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        play_of(&r.command().await),
        (None, Some("RDAOfake"), None, Some("wAEB8gECKAE%3D"), 0.0)
    );
}

/// The plain step 2 form still works, with no params.
#[tokio::test]
async fn the_plain_play_form_still_works() {
    let mut r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    let v = c
        .ask(
            1,
            "play",
            json!({"videoId": SONG, "playlistId": "PLfake", "index": 2, "startSeconds": 7.5}),
        )
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        play_of(&r.command().await),
        (Some(SONG), Some("PLfake"), Some(2), None, 7.5)
    );
}

#[tokio::test]
async fn play_page_uses_header_button_then_first_row() {
    let header_play = Endpoint::WatchPlaylist(WatchPlaylistEndpoint {
        playlist_id: "RDAOheader".into(),
        params: Some("wAEB8gECKAE%3D".into()),
    });
    let mut pages = HashMap::new();
    // An artist page: the header's button (shuffle) wins over its rows.
    pages.insert(
        "UCheader".to_string(),
        Page {
            header: PageHeader {
                title: "Artist".into(),
                play: Some(header_play),
                ..PageHeader::default()
            },
            sections: vec![section("Songs", vec![song_row("AAAAAAAAAAA")])],
            cont: String::new(),
        },
    );
    // No button: the first row that plays, past rows that only open something.
    pages.insert(
        "UCrows".to_string(),
        Page {
            sections: vec![
                section("Artists", vec![page_row("UCother1"), page_row("UCother2")]),
                section("Songs", vec![page_row("UCother3"), song_row("BBBBBBBBBBB")]),
            ],
            ..Page::default()
        },
    );
    // Like `Page.js`, only each section's first 5 rows are looked at.
    let mut deep = vec![page_row("UCx"); 5];
    deep.push(song_row("CCCCCCCCCCC"));
    pages.insert(
        "UCdeep".to_string(),
        Page {
            sections: vec![
                section("Deep", deep),
                section("Next", vec![song_row("DDDDDDDDDDD")]),
            ],
            ..Page::default()
        },
    );
    // Nothing playable at all.
    pages.insert(
        "UCnothing".to_string(),
        Page {
            sections: vec![section("Artists", vec![page_row("UCother")])],
            ..Page::default()
        },
    );
    let mut r = rig(FakeBrowser {
        pages,
        ..FakeBrowser::default()
    });
    let mut c = connect(&r.path).await;

    let v = c
        .ask(
            1,
            "playPage",
            json!({"browseId": "UCheader", "params": "ggMIegYIARoCAQI%3D"}),
        )
        .await;
    assert_eq!(v, json!({"id": 1, "ok": true, "data": {}}));
    assert_eq!(
        play_of(&r.command().await),
        (None, Some("RDAOheader"), None, Some("wAEB8gECKAE%3D"), 0.0)
    );

    let v = c.ask(2, "playPage", json!({"browseId": "UCrows"})).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        play_of(&r.command().await),
        (Some("BBBBBBBBBBB"), None, None, None, 0.0)
    );

    let v = c.ask(3, "playPage", json!({"browseId": "UCdeep"})).await;
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(
        play_of(&r.command().await),
        (Some("DDDDDDDDDDD"), None, None, None, 0.0)
    );

    let v = c.ask(4, "playPage", json!({"browseId": "UCnothing"})).await;
    assert_eq!(
        v,
        json!({"id": 4, "ok": false,
               "error": {"code": "bad_request", "message": "Nothing here can be played."}})
    );
    assert!(r.commands.try_recv().is_err());
    assert_eq!(
        r.calls(),
        [
            "browse UCheader ggMIegYIARoCAQI%3D",
            "browse UCrows -",
            "browse UCdeep -",
            "browse UCnothing -"
        ]
    );
}

#[tokio::test]
async fn browse_reply_goes_only_to_the_asker() {
    let mut pages = HashMap::new();
    let page = Page {
        header: PageHeader {
            title: "Album".into(),
            ..PageHeader::default()
        },
        sections: vec![section("", vec![song_row(SONG)])],
        cont: "faketoken".into(),
    };
    pages.insert("MPREfake".to_string(), page.clone());
    let gate = Arc::new(Semaphore::new(0));
    let r = rig(FakeBrowser {
        pages,
        gate: Some(gate.clone()),
        ..FakeBrowser::default()
    });
    let mut events = r.events.subscribe();
    let mut a = connect(&r.path).await;
    let mut b = connect(&r.path).await;

    a.send(json!({"id": 1, "cmd": "browse", "args": {"browseId": "MPREfake"}}))
        .await;
    // While the browse waits on YouTube, the same client is still answered: browsing runs
    // off the client's loop.
    a.send(json!({"id": 2, "cmd": "status"})).await;
    let v = a.next().await.unwrap();
    assert_eq!(v["id"], 2, "{v}");
    gate.add_permits(1);
    let v = a.reply(1).await;
    assert_eq!(v["ok"], true);
    // The exact `Page.js` shape, straight from the page.
    assert_eq!(v["data"], serde_json::to_value(&page).unwrap());
    assert_eq!(
        v["data"]["sections"][0]["items"][0]["play"],
        json!({"watchEndpoint": {"videoId": SONG}})
    );

    // The other client saw none of it: its first line is its own reply.
    b.send(json!({"id": 9, "cmd": "status"})).await;
    let v = b.next().await.unwrap();
    assert_eq!(v["id"], 9, "{v}");
    // And no event went out to anyone.
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn browse_doesnt_touch_the_queue() {
    let mut r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    let v = c
        .ask(1, "browse", json!({"browseId": "FEmusic_home"}))
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        v["data"],
        json!({"header": {"title": "", "subtitle": "", "thumb": "",
        "play": null}, "sections": [], "cont": ""})
    );
    let v = c
        .ask(
            2,
            "search",
            json!({"query": "  some song  ", "params": "EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D"}),
        )
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["sections"][0]["title"], "Top result");
    assert_eq!(v["data"]["chips"], json!([]));
    let v = c
        .ask(
            3,
            "more",
            json!({"kind": "search", "token": "fake+token/=="}),
        )
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(v["data"]["items"][0]["videoId"], SONG);
    assert_eq!(v["data"]["cont"], "");
    let v = c
        .ask(4, "more", json!({"kind": "browse", "token": "faketoken"}))
        .await;
    assert_eq!(v["ok"], true);
    assert_eq!(
        r.calls(),
        [
            "browse FEmusic_home -",
            "search some song EgWKAQIIAWoKEAkQBRAKEAMQBA%3D%3D",
            "more Search fake+token/==",
            "more Browse faketoken"
        ]
    );
    // Nothing reached the engine: no play, no queue change.
    c.send(json!({"id": 5, "cmd": "queue.get"})).await;
    assert_eq!(c.reply(5).await["data"]["items"], json!([]));
    assert!(r.commands.try_recv().is_err());
}

#[tokio::test]
async fn bad_ids_are_bad_request() {
    let r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    let long = "x".repeat(4097);
    let long_query = "q".repeat(201);
    for (n, (cmd, args)) in [
        ("browse", json!({})),
        ("browse", json!({"browseId": ""})),
        ("browse", json!({"browseId": "x"})),
        ("browse", json!({"browseId": "../FEmusic_home"})),
        ("browse", json!({"browseId": "x".repeat(129)})),
        ("browse", json!({"browseId": 5})),
        (
            "browse",
            json!({"browseId": "FEmusic_home", "params": "a b"}),
        ),
        (
            "browse",
            json!({"browseId": "FEmusic_home", "params": long}),
        ),
        ("browse", json!({"browseId": "FEmusic_home", "params": 1})),
        ("playPage", json!({"browseId": "UC/../x"})),
        ("playPage", json!({"browseId": "UCfake", "params": "\"}"})),
        ("search", json!({})),
        ("search", json!({"query": ""})),
        ("search", json!({"query": "   "})),
        ("search", json!({"query": long_query})),
        ("search", json!({"query": "a\nb"})),
        ("search", json!({"query": "a\u{202E}b"})),
        ("search", json!({"query": "a\u{2028}b"})),
        ("search", json!({"query": "a\u{200B}b"})),
        ("search", json!({"query": 5})),
        ("search", json!({"query": "ok", "params": "a:b"})),
        ("more", json!({"token": "faketoken"})),
        ("more", json!({"kind": "next", "token": "faketoken"})),
        ("more", json!({"kind": "browse"})),
        ("more", json!({"kind": "browse", "token": ""})),
        ("more", json!({"kind": "browse", "token": "fake token"})),
        ("more", json!({"kind": "browse", "token": long})),
    ]
    .into_iter()
    .enumerate()
    {
        let id = n as u64 + 1;
        let v = c.ask(id, cmd, args.clone()).await;
        assert_eq!(code(&v), "bad_request", "{cmd} {args}: {v}");
        // The message says what was wrong, never the value.
        let message = v["error"]["message"].as_str().unwrap();
        assert!(
            !message.contains("../") && !message.contains("fake token"),
            "{message}"
        );
    }
    // Refused before anything was asked of YouTube (or the keyring).
    assert!(r.calls().is_empty(), "{:?}", r.calls());
}

/// A refusal from the request itself (Task 3's checks) and a failed request both answer the
/// asker with the error's code and fixed text, and are never broadcast as an error event.
#[tokio::test]
async fn failures_answer_the_asker_and_are_never_broadcast() {
    for (fail, want_code, want_message) in [
        (
            Error::BadRequest("not a browse id".into()),
            "bad_request",
            "bad request: not a browse id",
        ),
        (
            Error::Network("timed out".into()),
            "network",
            "network error: timed out",
        ),
        (Error::SignedOut, "signed_out", "signed out"),
    ] {
        let r = rig(FakeBrowser {
            fail: Some(fail),
            ..FakeBrowser::default()
        });
        let mut events = r.events.subscribe();
        let mut c = connect(&r.path).await;
        let v = c
            .ask(1, "browse", json!({"browseId": "FEmusic_home"}))
            .await;
        assert_eq!(
            v,
            json!({"id": 1, "ok": false, "error": {"code": want_code, "message": want_message}})
        );
        let v = c.ask(2, "playPage", json!({"browseId": "UCfake"})).await;
        assert_eq!(code(&v), want_code);
        assert!(
            events.try_recv().is_err(),
            "a browsing failure was broadcast"
        );
    }
}

#[tokio::test]
async fn at_most_four_browsing_requests_at_once() {
    let gate = Arc::new(Semaphore::new(0));
    let r = rig(FakeBrowser {
        gate: Some(gate.clone()),
        ..FakeBrowser::default()
    });
    let mut c = connect(&r.path).await;
    for id in 1..=4 {
        c.send(json!({"id": id, "cmd": "browse", "args": {"browseId": "FEmusic_home"}}))
            .await;
    }
    // The fifth is refused at once; the rest wait.
    let v = c.ask(5, "search", json!({"query": "x"})).await;
    assert_eq!(
        v,
        json!({"id": 5, "ok": false, "error": {"code": "bad_request", "message": "busy"}})
    );
    // Another client has its own four.
    let mut other = connect(&r.path).await;
    other
        .send(json!({"id": 1, "cmd": "browse", "args": {"browseId": "FEmusic_home"}}))
        .await;
    gate.add_permits(5);
    let v = other.reply(1).await;
    assert_eq!(v["ok"], true);
    let mut got: Vec<u64> = Vec::new();
    for _ in 0..4 {
        let v = c.next().await.unwrap();
        assert_eq!(v["ok"], true, "{v}");
        got.push(v["id"].as_u64().unwrap());
    }
    got.sort();
    assert_eq!(got, [1, 2, 3, 4]);
    // Once answered, there is room again.
    gate.add_permits(1);
    let v = c
        .ask(6, "browse", json!({"browseId": "FEmusic_home"}))
        .await;
    assert_eq!(v["ok"], true);
}

/// A client that sends a browse and closes its sending side still gets the answer, as with
/// every other reply.
#[tokio::test]
async fn a_half_closed_client_still_gets_its_answer() {
    let gate = Arc::new(Semaphore::new(0));
    let r = rig(FakeBrowser {
        gate: Some(gate.clone()),
        ..FakeBrowser::default()
    });
    let mut c = connect(&r.path).await;
    c.send(json!({"id": 1, "cmd": "browse", "args": {"browseId": "FEmusic_home"}}))
        .await;
    c.write.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.add_permits(1);
    let v = c.reply(1).await;
    assert_eq!(v["ok"], true);
    assert!(
        c.next().await.is_none(),
        "the connection closes after the answer"
    );
}

/// The biggest answers the fixtures give, as reply lines: printed for the task report, and
/// held well under the 1 MiB line the widgets read. Plus the worst case the caps allow: a
/// continuation of 1,000 rows (`Page.js`'s cap for one) with real-sized fields.
#[test]
fn replies_fit_a_line() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/browse");
    let mut sizes = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        if path.extension().is_none_or(|e| e != "json")
            || !name.starts_with("browse_") && !name.starts_with("search_")
        {
            continue;
        }
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let line = if name.ends_with("_cont.json") {
            control::protocol::data_reply(1, &browse::parse_more(&raw))
        } else if name.starts_with("search_") {
            let filtered = name != "search_mixed.json";
            control::protocol::data_reply(1, &browse::parse_search(&raw, filtered))
        } else {
            control::protocol::data_reply(1, &browse::parse_browse(&raw))
        };
        sizes.push((line.len(), name));
    }
    sizes.sort();
    for (bytes, name) in &sizes {
        println!("{name}: {bytes} bytes");
    }
    let (biggest, _) = sizes.last().unwrap();
    assert!(*biggest < control::protocol::MAX_LINE / 4, "{biggest}");

    let row = Row {
        title: "A Song Title Of Typical Length (Remastered)".into(),
        subtitle: "First Artist & Second Artist • An Album Name Of Typical Length".into(),
        thumb: format!(
            "https://lh3.googleusercontent.com/{}=w120-h120-l90-rj",
            "x".repeat(110)
        ),
        video_id: SONG.into(),
        set_id: "56B44F6D10557CC6".into(),
        playlist_id: String::new(),
        browse_id: String::new(),
        params: String::new(),
        play: Some(Endpoint::Watch(WatchEndpoint {
            video_id: Some(SONG.into()),
            playlist_id: Some("PL".to_string() + &"x".repeat(32)),
            index: None,
            params: Some("8gECGAE%3D".into()),
        })),
        duration: "4:05".into(),
        kind: Kind::Song,
    };
    let more = MorePage {
        items: vec![row; 1000],
        sections: Vec::new(),
        cont: "x".repeat(400),
    };
    let bytes = control::protocol::data_reply(1, &more).len();
    println!("1,000-row continuation: {bytes} bytes");
    // About 550 KB with these long-ish fields: under the line with room, and the daemon refuses
    // (rather than sends) anything over it.
    assert!(bytes < control::protocol::MAX_LINE * 3 / 4, "{bytes}");
}

/// A browsing request is activity (ruling R21): a user looking through pages with nothing
/// playing keeps the daemon up.
#[tokio::test(start_paused = true)]
async fn browsing_restarts_the_idle_clock() {
    const MIN: Duration = Duration::from_secs(60);
    let r = rig(FakeBrowser::default());
    let mut c = connect(&r.path).await;
    assert_eq!(c.ask(1, "status", json!({})).await["ok"], true);
    tokio::time::sleep(4 * MIN).await;
    assert_eq!(
        c.ask(2, "browse", json!({"browseId": "FEmusic_home"}))
            .await["ok"],
        true
    );
    // 8 minutes after the status, 4 after the browse: still up (the limit on mains is 5).
    tokio::time::sleep(4 * MIN).await;
    assert!(!r.serve.is_finished(), "quit 4 minutes after a browse");
    assert_eq!(r.serve.await.unwrap(), Exit::Idle);
}

/// Never answers: a play stays `buffering`, with its song as the state's `videoId`.
struct Hang;

#[async_trait]
impl Resolver for Hang {
    async fn resolve(&self, _: &str) -> Result<Stream, Error> {
        std::future::pending().await
    }
    async fn resolve_fresh(&self, id: &str) -> Result<Stream, Error> {
        self.resolve(id).await
    }
}

#[async_trait]
impl QueueSource for Hang {
    async fn next(&self, _: NextRequest) -> Result<NextPage, Error> {
        std::future::pending().await
    }
    async fn song_next(&self, _: &str) -> Result<SongNext, Error> {
        std::future::pending().await
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        std::future::pending().await
    }
}

/// The real engine (with `Hang` and a `NullSink`) behind the socket, and a browser whose
/// calls wait for `gate`.
fn real_engine(pages: HashMap<String, Page>, gate: Arc<Semaphore>) -> (PathBuf, TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(Hang), player);
    let options = Options {
        browser: Some(Arc::new(FakeBrowser {
            pages,
            gate: Some(gate),
            ..FakeBrowser::default()
        })),
        ..Options::default()
    };
    tokio::spawn(control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    ));
    (path, dir)
}

/// Ruling P7: a `playPage` whose page is still loading never overrides a play the user made
/// meanwhile (here from another client); alone, it plays.
#[tokio::test]
async fn play_page_never_overrides_a_newer_play() {
    let mut pages = HashMap::new();
    pages.insert(
        "UCfake".to_string(),
        Page {
            sections: vec![section("Songs", vec![song_row("BBBBBBBBBBB")])],
            ..Page::default()
        },
    );
    let gate = Arc::new(Semaphore::new(0));
    let (path, _dir) = real_engine(pages, gate.clone());
    let mut a = connect(&path).await;
    let mut b = connect(&path).await;

    a.send(json!({"id": 1, "cmd": "playPage", "args": {"browseId": "UCfake"}}))
        .await;
    // Its epoch is read before the browse starts: wait for the browse to be under way.
    a.send(json!({"id": 2, "cmd": "status"})).await;
    a.reply(2).await;
    assert_eq!(b.ask(1, "play", json!({"videoId": SONG})).await["ok"], true);
    gate.add_permits(1);
    assert_eq!(
        a.reply(1).await,
        json!({"id": 1, "ok": true, "data": {"superseded": true}})
    );
    let v = a.ask(3, "status", json!({})).await;
    assert_eq!(v["data"]["videoId"], SONG, "{v}");

    // Nothing in between: the page's song plays.
    gate.add_permits(1);
    assert_eq!(
        a.ask(4, "playPage", json!({"browseId": "UCfake"})).await,
        json!({"id": 4, "ok": true, "data": {}})
    );
    let v = a.ask(5, "status", json!({})).await;
    assert_eq!(v["data"]["videoId"], "BBBBBBBBBBB", "{v}");
}

/// A page whose answer would pass the socket's line cap is refused with `unavailable` rather
/// than sent, and the client stays connected.
#[tokio::test]
async fn an_oversized_answer_is_refused_not_sent() {
    let mut big = song_row(SONG);
    big.title = "t".repeat(2000);
    let mut pages = HashMap::new();
    pages.insert(
        "FEbig".to_string(),
        Page {
            sections: vec![section("Big", vec![big; 600])],
            ..Page::default()
        },
    );
    let r = rig(FakeBrowser {
        pages,
        ..FakeBrowser::default()
    });
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "browse", json!({"browseId": "FEbig"})).await;
    assert_eq!(
        v,
        json!({"id": 1, "ok": false, "error": {"code": "unavailable",
               "message": "unavailable: the page is too big to send"}})
    );
    assert_eq!(c.ask(2, "status", json!({})).await["ok"], true);
}

const LYRICS_PAGE: &str = "MPLYtfake000001";

fn words() -> Lyrics {
    Lyrics {
        text: "First line\nSecond line\n\nChorus".into(),
        source: "Source: Musixmatch".into(),
    }
}

/// YouTube Music's plain lyrics as the chain answers them.
fn found(l: Lyrics) -> Found {
    Found {
        source: l.source,
        synced: false,
        words: false,
        lines: ytmfast::lyrics::lrc::plain_lines(&l.text),
    }
}

/// A browser that knows `SONG`'s Lyrics tab and its text.
fn with_lyrics() -> FakeBrowser {
    FakeBrowser {
        tabs: HashMap::from([(SONG.to_string(), LYRICS_PAGE.to_string())]),
        lyrics: HashMap::from([(LYRICS_PAGE.to_string(), words())]),
        ..FakeBrowser::default()
    }
}

/// Lyrics are `Page.js`'s shape: the text as YouTube gives it (newlines kept) and the shelf's
/// footer as the source. A song nobody knew about costs its `next` and the lyrics browse, and
/// the tab that `next` found is handed to the engine's per-song cache.
#[tokio::test]
async fn lyrics_found() {
    let r = rig(with_lyrics());
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(
        v,
        json!({"id": 1, "ok": true, "data": {
            "source": "Source: Musixmatch", "synced": false, "words": false,
            "lines": [{"text": "First line"}, {"text": "Second line"}, {"text": ""},
                      {"text": "Chorus"}],
        }})
    );
    assert_eq!(
        serde_json::to_string(&v["data"]).unwrap(),
        r#"{"source":"Source: Musixmatch","synced":false,"words":false,"lines":[{"text":"First line"},{"text":"Second line"},{"text":""},{"text":"Chorus"}]}"#,
        "the field order is the spec's"
    );
    assert_eq!(
        r.calls(),
        [format!("next {SONG}"), format!("lyrics {LYRICS_PAGE}")]
    );
    let known = r.known.lock().unwrap().get(SONG).cloned().unwrap();
    assert_eq!(known.page.as_deref(), Some(LYRICS_PAGE));
}

/// No Lyrics tab, or a lyrics page with no text: `{none: true}`. A malformed id is the
/// client's mistake (`bad_request`), and nothing is sent.
#[tokio::test]
async fn lyrics_none() {
    let mut browser = with_lyrics();
    // A tab whose page has no text.
    browser
        .tabs
        .insert("BBBBBBBBBBB".into(), "MPLYtfake_empty".into());
    let r = rig(browser);
    let mut c = connect(&r.path).await;
    let none = |id: u64| json!({"id": id, "ok": true, "data": {"none": true}});
    assert_eq!(
        c.ask(1, "lyrics", json!({"videoId": "AAAAAAAAAAA"})).await,
        none(1)
    );
    assert_eq!(
        c.ask(2, "lyrics", json!({"videoId": "BBBBBBBBBBB"})).await,
        none(2)
    );
    assert_eq!(
        r.calls(),
        [
            "next AAAAAAAAAAA",
            "next BBBBBBBBBBB",
            "lyrics MPLYtfake_empty"
        ]
    );
    // "No tab" is known now too.
    let known = r.known.lock().unwrap().get("AAAAAAAAAAA").cloned().unwrap();
    assert_eq!(known.page, None);

    for (id, args) in [
        (3, json!({})),
        (4, json!({"videoId": "../x"})),
        (5, json!({"videoId": 7})),
        (6, json!({"videoId": "AAAAAAAAAA"})),
    ] {
        let v = c.ask(id, "lyrics", args.clone()).await;
        assert_eq!(code(&v), "bad_request", "{args} -> {v}");
    }
    assert_eq!(r.calls().len(), 3, "nothing sent for a bad id");

    // A failure is the asker's, with its code; it is not kept as "none".
    let r = rig(FakeBrowser {
        fail: Some(Error::SignedOut),
        ..with_lyrics()
    });
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(code(&v), "signed_out", "{v}");
    let v = c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(code(&v), "signed_out", "{v}");
    assert_eq!(r.calls().len(), 2, "asked again: a failure is not kept");
}

fn nth_song(i: usize) -> String {
    format!("song{i:07}")
}

/// The last 20 answers ("none" too) are kept in the daemon, for every client: reopening the
/// Lyrics tab costs nothing. The 21st pushes out the one used longest ago; asked again, it is
/// fetched again, and the tab the engine kept saves its `next`.
#[tokio::test]
async fn lyrics_cached_per_video() {
    let mut browser = FakeBrowser::default();
    for i in 0..21 {
        // Every third song has no lyrics: "none" is kept as well.
        if i % 3 != 2 {
            let page = format!("MPLYtfake{i:06}");
            browser.tabs.insert(nth_song(i), page.clone());
            browser.lyrics.insert(
                page,
                Lyrics {
                    text: format!("words {i}"),
                    source: "Source: LyricFind".into(),
                },
            );
        }
    }
    let r = rig(browser);
    let mut a = connect(&r.path).await;
    let mut b = connect(&r.path).await;
    let mut want = Vec::new();
    for i in 0..21 {
        let v = a
            .ask(i as u64, "lyrics", json!({"videoId": nth_song(i)}))
            .await;
        assert_eq!(v["ok"], true, "{v}");
        want.push(v["data"].clone());
    }
    let fetched = r.calls().len();
    assert_eq!(fetched, 21 + 14);

    // Songs 1 to 20 are kept: asked again, from another client, nothing is sent.
    for (i, data) in want.iter().enumerate().skip(1) {
        let v = b
            .ask(100 + i as u64, "lyrics", json!({"videoId": nth_song(i)}))
            .await;
        assert_eq!(&v["data"], data, "song {i}");
    }
    assert_eq!(r.calls().len(), fetched);

    // Song 0 was pushed out: fetched again, with only the lyrics browse (its tab is known).
    let v = b.ask(200, "lyrics", json!({"videoId": nth_song(0)})).await;
    assert_eq!(v["data"], want[0]);
    assert_eq!(r.calls()[fetched..], ["lyrics MPLYtfake000000"]);
    // That pushed out the one used longest ago, song 1; song 20 stays.
    let v = b.ask(201, "lyrics", json!({"videoId": nth_song(20)})).await;
    assert_eq!(v["data"], want[20]);
    assert_eq!(r.calls().len(), fetched + 1);
    let v = b.ask(202, "lyrics", json!({"videoId": nth_song(1)})).await;
    assert_eq!(v["data"], want[1]);
    assert_eq!(r.calls()[fetched + 1..], ["lyrics MPLYtfake000001"]);
}

/// Answers the queue's `next` with one song and that song's Lyrics tab, as the real `next`
/// does when the request names the song. A lyrics `next` of its own is never wanted here.
struct Tabbed;

#[async_trait]
impl QueueSource for Tabbed {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        assert_eq!(req.video_id.as_deref(), Some(SONG));
        Ok(NextPage {
            items: vec![SongItem {
                video_id: SONG.into(),
                title: "Song".into(),
                length_seconds: 200,
                ..SongItem::default()
            }],
            lyrics_tab: Some(LYRICS_PAGE.into()),
            ..NextPage::default()
        })
    }
    async fn song_next(&self, _: &str) -> Result<SongNext, Error> {
        std::future::pending().await
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        std::future::pending().await
    }
}

/// Ruling P6's carry: the like lookup and lyrics share one `next` per song. A song played by id
/// gets its like status from its queue's `next` (ruling P1), and that answer's Lyrics tab is
/// kept with it; lyrics for the song then cost only the lyrics browse.
#[tokio::test]
async fn lyrics_reuses_the_like_lookup_next() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(Tabbed), player);
    let calls = Calls::default();
    let options = Options {
        browser: Some(Arc::new(FakeBrowser {
            calls: calls.clone(),
            ..with_lyrics()
        })),
        ..Options::default()
    };
    tokio::spawn(control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    ));
    let mut c = connect(&path).await;
    assert_eq!(c.ask(1, "play", json!({"videoId": SONG})).await["ok"], true);
    // The queue's answer is in once the queue lists the song.
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        let q = c.ask(2, "queue.get", json!({})).await;
        if q["data"]["items"].as_array().is_some_and(|i| !i.is_empty()) {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no queue: {q}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let v = c.ask(3, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["lines"][0]["text"], "First line", "{v}");
    assert_eq!(*calls.lock().unwrap(), [format!("lyrics {LYRICS_PAGE}")]);
}

/// The engine's cache, faked for the cache-level test below.
#[derive(Default)]
struct FakeTabs(Known);

#[async_trait]
impl LyricsTabs for FakeTabs {
    async fn known(&self, video_id: &str) -> Option<KnownTab> {
        self.0.lock().unwrap().get(video_id).cloned()
    }
    async fn song(&self, _: &str) -> Option<SongFacts> {
        None
    }
    async fn learn(&self, video_id: &str, next: SongNext) {
        self.0.lock().unwrap().insert(
            video_id.into(),
            KnownTab {
                page: next.lyrics_tab,
                at: tokio::time::Instant::now(),
            },
        );
    }
}

/// A kept "none" is asked again after an hour (YouTube may have added lyrics since), and so is
/// the engine's "no Lyrics tab"; found lyrics stay for the daemon's life.
#[tokio::test(start_paused = true)]
async fn none_expires_after_an_hour() {
    const MIN: Duration = Duration::from_secs(60);
    let calls = Calls::default();
    let browser = FakeBrowser {
        calls: calls.clone(),
        ..with_lyrics()
    };
    let cache = LyricsCache::new(Arc::new(FakeTabs::default()), None);
    let none = "AAAAAAAAAAA";
    assert_eq!(cache.get(&browser, none).await, Ok(None));
    assert_eq!(cache.get(&browser, SONG).await, Ok(Some(found(words()))));
    assert_eq!(calls.lock().unwrap().len(), 3);

    tokio::time::sleep(59 * MIN).await;
    assert_eq!(cache.get(&browser, none).await, Ok(None));
    assert_eq!(calls.lock().unwrap().len(), 3, "kept for the hour");

    tokio::time::sleep(2 * MIN).await;
    assert_eq!(cache.get(&browser, none).await, Ok(None));
    assert_eq!(
        calls.lock().unwrap()[3..],
        [format!("next {none}")],
        "asked again, past the engine's own 'no tab' too"
    );

    tokio::time::sleep(5 * 60 * MIN).await;
    assert_eq!(cache.get(&browser, SONG).await, Ok(Some(found(words()))));
    assert_eq!(calls.lock().unwrap().len(), 4, "found lyrics don't expire");
}

/// Lyrics over `lyrics::MAX_TEXT` are cut to it, on a character boundary, before they are kept:
/// the cache holds 20 answers, and an answer is sent as one line, so neither can grow with what
/// YouTube sends. The source line is kept, and the cut text is what a second ask gets.
#[tokio::test]
async fn long_lyrics_are_cut_before_they_are_kept() {
    // A 2-byte character straddles the cap: it goes whole, never split.
    let text = format!(
        "{}é{}",
        "a".repeat(lyrics::MAX_TEXT - 1),
        "b".repeat(100_000)
    );
    let calls = Calls::default();
    let browser = FakeBrowser {
        calls: calls.clone(),
        lyrics: HashMap::from([(
            LYRICS_PAGE.to_string(),
            Lyrics {
                text,
                source: "Source: Musixmatch".into(),
            },
        )]),
        ..with_lyrics()
    };
    let cache = LyricsCache::new(Arc::new(FakeTabs::default()), None);
    let got = cache.get(&browser, SONG).await.unwrap().unwrap();
    assert_eq!(got.lines.len(), 1);
    assert_eq!(got.lines[0].text.len(), lyrics::MAX_TEXT - 1);
    assert!(got.lines[0].text.bytes().all(|b| b == b'a'));
    assert_eq!(got.source, "Source: Musixmatch");
    assert_eq!(cache.get(&browser, SONG).await, Ok(Some(got)));
    assert_eq!(
        calls.lock().unwrap().len(),
        2,
        "the second ask was the cut, kept answer"
    );

    // Text right at the cap is kept whole.
    let browser = FakeBrowser {
        lyrics: HashMap::from([(
            LYRICS_PAGE.to_string(),
            Lyrics {
                text: "a".repeat(lyrics::MAX_TEXT),
                source: String::new(),
            },
        )]),
        ..with_lyrics()
    };
    let cache = LyricsCache::new(Arc::new(FakeTabs::default()), None);
    let got = cache.get(&browser, SONG).await.unwrap().unwrap();
    assert_eq!(got.lines[0].text.len(), lyrics::MAX_TEXT);
    // No "Source: …" line: YouTube Music's own name.
    assert_eq!(got.source, "YouTube Music");
}

/// The lyrics services, faked: answers by link, and every link asked for.
#[derive(Default)]
struct FakeWeb {
    answers: HashMap<String, Fetched>,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl LyricsWeb for FakeWeb {
    async fn get_json(&self, url: &str) -> Fetched {
        self.asked.lock().unwrap().push(url.into());
        self.answers.get(url).cloned().unwrap_or(Fetched::NotFound)
    }
}

const LRC_GET: &str = "https://lrclib.net/api/get?artist_name=Made%20Up%2C%20Other&track_name=Song%20(feat.%20X)&album_name=Album&duration=213";
const KUGOU_SEARCH: &str = "https://krcs.kugou.com/search?ver=1&man=yes&client=mobi&keyword=Made%20Up%20-%20Song&duration=213000&hash=";

fn song_facts() -> Facts {
    Facts::from([(
        SONG.to_string(),
        SongFacts {
            title: "Song (feat. X)".into(),
            artist: "Made Up, Other".into(),
            album: Some("Album".into()),
            length_seconds: 213,
        },
    )])
}

fn timed_web(failing_search: bool) -> Arc<FakeWeb> {
    let mut web = FakeWeb::default();
    web.answers.insert(
        LRC_GET.into(),
        Fetched::Json(json!({"syncedLyrics": "[00:01.50] One\n[00:04.25] Two"})),
    );
    if failing_search {
        web.answers.insert(KUGOU_SEARCH.into(), Fetched::Failed);
    }
    Arc::new(web)
}

/// Spec A1: with the engine knowing the song, LRCLIB's timed lines come back in the new shape,
/// and YouTube Music is never asked (its plain text comes after any timing). The answer is kept:
/// asked again, nothing is sent anywhere.
#[tokio::test]
async fn lyrics_timed_from_lrclib() {
    let web = timed_web(false);
    let r = rig_with(with_lyrics(), song_facts(), Some(web.clone()));
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(
        v["data"],
        json!({"source": "LRCLIB", "synced": true, "words": false,
               "lines": [{"t": 1.5, "text": "One"}, {"t": 4.25, "text": "Two"}]})
    );
    assert!(
        r.calls().is_empty(),
        "YouTube Music not asked: {:?}",
        r.calls()
    );
    let mut asked = web.asked.lock().unwrap().clone();
    asked.sort();
    assert_eq!(asked, [KUGOU_SEARCH, LRC_GET]);
    let v2 = c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v2["data"], v["data"]);
    assert_eq!(web.asked.lock().unwrap().len(), 2, "kept");
}

/// A failed request on the way (KuGou unreachable): what was found is shown, but not kept, so
/// the next ask tries again.
#[tokio::test]
async fn lyrics_after_a_failure_are_shown_not_kept() {
    let web = timed_web(true);
    let r = rig_with(with_lyrics(), song_facts(), Some(web.clone()));
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["source"], "LRCLIB");
    c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(web.asked.lock().unwrap().len(), 4, "asked again");
}

/// A song the engine knows nothing of (not playing, not queued): KuGou and LRCLIB can't be
/// asked without its title and artist, so only YouTube Music is.
#[tokio::test]
async fn lyrics_of_an_unknown_song_ask_youtube_music_only() {
    let web = Arc::new(FakeWeb::default());
    let r = rig_with(with_lyrics(), Facts::new(), Some(web.clone()));
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["source"], "Source: Musixmatch");
    assert!(web.asked.lock().unwrap().is_empty());
    assert_eq!(r.calls().len(), 2);
    // Not kept: once the engine has the song (its details land a moment after a play by id),
    // the next ask may find its timing.
    c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(r.calls().len(), 3, "asked again: {:?}", r.calls());
}

/// Neither KuGou nor LRCLIB timing: YouTube Music's plain lyrics come before LRCLIB's plain
/// text (the widget's order). With YouTube's step failing and nothing else, the error is the
/// reply, as before, and nothing is kept.
#[tokio::test]
async fn lyrics_plain_order_and_youtube_failures() {
    let mut web = FakeWeb::default();
    web.answers.insert(
        LRC_GET.into(),
        Fetched::Json(json!({"plainLyrics": "LRCLIB plain"})),
    );
    let web = Arc::new(web);
    let r = rig_with(with_lyrics(), song_facts(), Some(web.clone()));
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["source"], "Source: Musixmatch", "{v}");

    // YouTube Music failing: LRCLIB's plain text, not kept.
    let r = rig_with(
        FakeBrowser {
            fail: Some(Error::SignedOut),
            ..with_lyrics()
        },
        song_facts(),
        Some(web.clone()),
    );
    let mut c = connect(&r.path).await;
    let v = c.ask(1, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["lines"], json!([{"text": "LRCLIB plain"}]), "{v}");
    c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(r.calls().len(), 2, "YouTube asked again: not kept");
}

/// The queue of a play by id, with the song's details, a moment after the play (as YouTube's
/// `next` answers). Until then the engine knows the song by id alone.
struct LateDetails;

#[async_trait]
impl QueueSource for LateDetails {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        assert_eq!(req.video_id.as_deref(), Some(SONG));
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(NextPage {
            items: vec![SongItem {
                video_id: SONG.into(),
                title: "Song (feat. X)".into(),
                artists: vec!["Made Up".into(), "Other".into()],
                album: Some("Album".into()),
                length_seconds: 213,
                ..SongItem::default()
            }],
            ..NextPage::default()
        })
    }
    async fn song_next(&self, _: &str) -> Result<SongNext, Error> {
        std::future::pending().await
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        std::future::pending().await
    }
}

/// Fix round 1: a widget with its Lyrics tab open asks for the song it just played by id at
/// once, before the engine has the song's details. The engine waits for them (they land 300 ms
/// later here) and the answer is LRCLIB's timed lines, not YouTube Music's plain text.
#[tokio::test]
async fn lyrics_asked_before_the_details_land_wait_for_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(LateDetails), player);
    let calls = Calls::default();
    let web = timed_web(false);
    let options = Options {
        browser: Some(Arc::new(FakeBrowser {
            calls: calls.clone(),
            ..with_lyrics()
        })),
        lyrics_web: Some(web.clone()),
        ..Options::default()
    };
    tokio::spawn(control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    ));
    let mut c = connect(&path).await;
    assert_eq!(c.ask(1, "play", json!({"videoId": SONG})).await["ok"], true);
    let started = tokio::time::Instant::now();
    let v = c.ask(2, "lyrics", json!({"videoId": SONG})).await;
    assert_eq!(v["data"]["source"], "LRCLIB", "{v}");
    assert_eq!(v["data"]["lines"][1]["t"], 4.25);
    assert!(started.elapsed() >= Duration::from_millis(200), "it waited");
    assert!(calls.lock().unwrap().is_empty(), "YouTube Music not asked");
}
