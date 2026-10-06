//! MPRIS end to end, on a private `dbus-daemon` per test: a fake engine side answers
//! `Status` and records every other command, and a plain zbus client plays the part of
//! playerctl or a desktop media widget.
//!
//! The bus config lists no service folders, so nothing can be auto-started on it, and every
//! connection here is made by address, so the user's own session bus is never reached.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::StreamExt;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use ytmfast::audio::player::AudioPlayer;
use ytmfast::audio::sink::NullSink;
use ytmfast::control::{self, Exit, Hub, Options};
use ytmfast::engine::{Engine, EngineCmd, EngineEvent, PlayState, QueueSource, QueueView, Status};
use ytmfast::error::Error;
use ytmfast::innertube::{NextPage, NextRequest, SongItem};
use ytmfast::mpris::{self, Bus, Mpris};
use ytmfast::queue::{QueueItem, Repeat};
use ytmfast::streams::{Resolver, Stream, TrackMeta};
use zbus::zvariant::{ObjectPath, OwnedValue};
use zbus::{Connection, Proxy};

const NAME: &str = "org.mpris.MediaPlayer2.ytmfast";
const PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT: &str = "org.mpris.MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
/// Real-time cap on any one wait, so a broken server fails the test instead of hanging it.
const WAIT: Duration = Duration::from_secs(10);

/// Kills a child process when the test ends, pass or panic.
struct Reap(Child);

impl Drop for Reap {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A throwaway session bus. Dropping it stops the daemon and removes its folder.
struct PrivateBus {
    address: String,
    _daemon: Reap,
    _dir: TempDir,
}

fn private_bus() -> PrivateBus {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("bus");
    let address = format!("unix:path={}", socket.display());
    let config = dir.path().join("bus.conf");
    // No <servicedir> and no <standard_session_servicedirs/>: nothing can be activated.
    std::fs::write(
        &config,
        format!(
            r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>"#,
            socket.display()
        ),
    )
    .unwrap();
    let daemon = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .arg("--nofork")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", dir.path())
        .env("XDG_RUNTIME_DIR", dir.path())
        .env("DBUS_SESSION_BUS_ADDRESS", &address)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("dbus-daemon (the dbus package) must be installed to run these tests");
    let daemon = Reap(daemon);
    let start = Instant::now();
    while !socket.exists() {
        assert!(start.elapsed() < WAIT, "the private bus never came up");
        std::thread::sleep(Duration::from_millis(10));
    }
    PrivateBus {
        address,
        _daemon: daemon,
        _dir: dir,
    }
}

/// The engine side, faked: answers `Status` from `status` and `QueueGet` from `queue`,
/// echoes a seek's landing as a `Position` event (as the engine does), and passes every
/// other command to the test.
struct FakeEngine {
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Sender<EngineEvent>,
    status: Arc<Mutex<Status>>,
    queue: Arc<Mutex<QueueView>>,
    seen: mpsc::UnboundedReceiver<EngineCmd>,
}

fn empty_queue() -> QueueView {
    QueueView {
        items: Vec::new().into(),
        current_id: None,
        shuffle: false,
        repeat: Repeat::Off,
    }
}

/// A queue of `n` songs (queue ids 1 to n) with `current` (a queue id) current.
fn queue_of(n: u64, current: Option<u64>, repeat: Repeat) -> QueueView {
    QueueView {
        items: (1..=n)
            .map(|id| QueueItem {
                id,
                song: SongItem {
                    video_id: format!("song{id:07}"),
                    ..SongItem::default()
                },
            })
            .collect(),
        current_id: current,
        shuffle: false,
        repeat,
    }
}

fn queue_event(q: &QueueView) -> EngineEvent {
    EngineEvent::Queue {
        items: q.items.clone(),
        current_id: q.current_id,
        shuffle: q.shuffle,
        repeat: q.repeat,
    }
}

fn fake_engine(initial: Status) -> FakeEngine {
    let (cmds, mut cmd_rx) = mpsc::channel(32);
    // The engine's own event capacity.
    let (events, _) = broadcast::channel(64);
    let status = Arc::new(Mutex::new(initial));
    let queue = Arc::new(Mutex::new(empty_queue()));
    let (seen_tx, seen) = mpsc::unbounded_channel();
    let (st, q, ev) = (status.clone(), queue.clone(), events.clone());
    tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                EngineCmd::Status(reply) => {
                    let _ = reply.send(st.lock().unwrap().clone());
                }
                EngineCmd::QueueGet(reply) => {
                    let _ = reply.send(q.lock().unwrap().clone());
                }
                EngineCmd::Seek(seconds) => {
                    st.lock().unwrap().position = seconds;
                    let _ = ev.send(EngineEvent::Position {
                        seconds,
                        seeked: true,
                    });
                    let _ = seen_tx.send(EngineCmd::Seek(seconds));
                }
                other => {
                    let _ = seen_tx.send(other);
                }
            }
        }
    });
    FakeEngine {
        cmds,
        events,
        status,
        queue,
        seen,
    }
}

impl FakeEngine {
    /// The next command the engine got (other than `Status`).
    async fn next(&mut self) -> EngineCmd {
        tokio::time::timeout(WAIT, self.seen.recv())
            .await
            .expect("no command in time")
            .expect("engine side closed")
    }

    /// True when no command came within a short while.
    async fn none_within(&mut self, d: Duration) -> bool {
        tokio::time::timeout(d, self.seen.recv()).await.is_err()
    }
}

fn stopped() -> Status {
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

fn meta() -> TrackMeta {
    TrackMeta {
        title: "Never Gonna Give You Up".into(),
        artist: "Rick Astley".into(),
        length_seconds: 213,
        thumbnail: Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg".into()),
    }
}

fn playing(id: &str, position: f64) -> Status {
    Status {
        state: PlayState::Playing,
        video_id: Some(id.into()),
        meta: Some(meta()),
        album: None,
        album_id: String::new(),
        queue_id: None,
        position,
        volume: 1.0,
        muted: false,
        shuffle: false,
        repeat: Repeat::Off,
        liked: None,
    }
}

/// The server, a client connection and proxies for both interfaces.
struct Rig {
    engine: FakeEngine,
    hub: Hub,
    mpris: Mpris,
    client: Connection,
    player: Proxy<'static>,
    root: Proxy<'static>,
    _bus: PrivateBus,
}

async fn rig(initial: Status) -> Rig {
    let bus = private_bus();
    let engine = fake_engine(initial);
    let hub = Hub::default();
    let mpris = mpris::serve(
        &Bus::Address(bus.address.clone()),
        engine.cmds.clone(),
        engine.events.subscribe(),
        hub.clone(),
    )
    .await
    .expect("MPRIS on the private bus");
    let client = client(&bus.address).await;
    let player = proxy(&client, PLAYER).await;
    let root = proxy(&client, ROOT).await;
    Rig {
        engine,
        hub,
        mpris,
        client,
        player,
        root,
        _bus: bus,
    }
}

async fn client(address: &str) -> Connection {
    zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap()
}

/// An uncached proxy: every property read goes to the server, as playerctl's does.
async fn proxy(conn: &Connection, interface: &'static str) -> Proxy<'static> {
    zbus::proxy::Builder::new(conn)
        .destination(NAME)
        .unwrap()
        .path(PATH)
        .unwrap()
        .interface(interface)
        .unwrap()
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .unwrap()
}

async fn call(p: &Proxy<'_>, method: &str) {
    tokio::time::timeout(WAIT, p.call_method(method, &()))
        .await
        .expect("no reply in time")
        .unwrap();
}

async fn metadata(p: &Proxy<'_>) -> HashMap<String, OwnedValue> {
    p.get_property("Metadata").await.unwrap()
}

fn track_id(m: &HashMap<String, OwnedValue>) -> String {
    let path: ObjectPath = m["mpris:trackid"].downcast_ref().unwrap();
    path.to_string()
}

/// Polls `check` until it holds (properties follow events through another task).
async fn eventually<F, Fut>(mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = Instant::now();
    while !check().await {
        assert!(start.elapsed() < WAIT, "condition never held");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn name_has_owner(conn: &Connection) -> bool {
    zbus::fdo::DBusProxy::new(conn)
        .await
        .unwrap()
        .name_has_owner(NAME.try_into().unwrap())
        .await
        .unwrap()
}

#[tokio::test]
async fn playpause_sends_toggle() {
    let mut r = rig(stopped()).await;
    call(&r.player, "PlayPause").await;
    assert!(matches!(r.engine.next().await, EngineCmd::Toggle));
    call(&r.player, "Play").await;
    assert!(matches!(
        r.engine.next().await,
        EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            params: None,
            start_seconds: 0.0
        }
    ));
    call(&r.player, "Pause").await;
    assert!(matches!(r.engine.next().await, EngineCmd::Pause));
}

#[tokio::test]
async fn seek_sends_seek_relative() {
    let mut r = rig(playing("dQw4w9WgXcQ", 5.0)).await;
    let mut seeked = r.player.receive_signal("Seeked").await.unwrap();
    r.player
        .call_method("Seek", &(10_000_000i64,))
        .await
        .unwrap();
    match r.engine.next().await {
        EngineCmd::Seek(s) => assert_eq!(s, 15.0),
        other => panic!("expected Seek(15.0), got {other:?}"),
    }
    // Seeked carries where the engine landed, in microseconds.
    let signal = tokio::time::timeout(WAIT, seeked.next())
        .await
        .expect("no Seeked in time")
        .unwrap();
    let at: i64 = signal.body().deserialize().unwrap();
    assert_eq!(at, 15_000_000);
    let position: i64 = r.player.get_property("Position").await.unwrap();
    assert_eq!(position, 15_000_000);

    // Back past the start: clamped to 0, never a negative seek.
    r.player
        .call_method("Seek", &(-60_000_000i64,))
        .await
        .unwrap();
    match r.engine.next().await {
        EngineCmd::Seek(s) => assert_eq!(s, 0.0),
        other => panic!("expected Seek(0.0), got {other:?}"),
    }
}

#[tokio::test]
async fn set_position_seeks_only_for_the_current_track() {
    let mut r = rig(playing("dQw4w9WgXcQ", 5.0)).await;
    let current = ObjectPath::try_from("/org/ytmfast/track/dQw4w9WgXcQ").unwrap();
    r.player
        .call_method("SetPosition", &(&current, 30_000_000i64))
        .await
        .unwrap();
    match r.engine.next().await {
        EngineCmd::Seek(s) => assert_eq!(s, 30.0),
        other => panic!("expected Seek(30.0), got {other:?}"),
    }
    // Another track's id, a negative position, or one past the end: ignored (the spec).
    let other = ObjectPath::try_from("/org/ytmfast/track/other").unwrap();
    for (track, at) in [
        (&other, 30_000_000i64),
        (&current, -1),
        (&current, 214_000_000),
        // Exactly the end too: the engine would take a seek there as Next.
        (&current, 213_000_000),
    ] {
        r.player
            .call_method("SetPosition", &(track, at))
            .await
            .unwrap();
    }
    assert!(r.engine.none_within(Duration::from_millis(200)).await);
}

#[tokio::test]
async fn metadata_follows_state_event() {
    let r = rig(stopped()).await;
    let m = metadata(&r.player).await;
    assert_eq!(track_id(&m), "/org/mpris/MediaPlayer2/TrackList/NoTrack");
    assert!(!m.contains_key("xesam:title"));

    let props = zbus::fdo::PropertiesProxy::builder(&r.client)
        .destination(NAME)
        .unwrap()
        .path(PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = props.receive_properties_changed().await.unwrap();

    // YouTube ids hold `-` (and `_`); an object path may not.
    let mut status = playing("ab-cd_EF12", 0.0);
    status.state = PlayState::Buffering;
    *r.engine.status.lock().unwrap() = status.clone();
    r.engine.events.send(EngineEvent::State(status)).unwrap();

    let change = tokio::time::timeout(WAIT, changes.next())
        .await
        .expect("no PropertiesChanged in time")
        .unwrap();
    let args = change.args().unwrap();
    assert_eq!(args.interface_name().as_str(), PLAYER);
    assert!(args.changed_properties().contains_key("Metadata"));
    assert!(args.changed_properties().contains_key("PlaybackStatus"));

    let m = metadata(&r.player).await;
    assert_eq!(track_id(&m), "/org/ytmfast/track/ab_cd_EF12");
    let title: String = m["xesam:title"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(title, "Never Gonna Give You Up");
    let artist: Vec<String> = m["xesam:artist"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(artist, vec!["Rick Astley".to_string()]);
    let length: i64 = m["mpris:length"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(length, 213_000_000);
    let art: String = m["mpris:artUrl"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(art, "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg");

    // Buffering shows as Playing: MPRIS has no buffering state.
    let s: String = r.player.get_property("PlaybackStatus").await.unwrap();
    assert_eq!(s, "Playing");

    for (state, want) in [
        (PlayState::Paused, "Paused"),
        (PlayState::Stopped, "Stopped"),
        (PlayState::Playing, "Playing"),
    ] {
        let mut st = playing("ab-cd_EF12", 0.0);
        st.state = state;
        r.engine.events.send(EngineEvent::State(st)).unwrap();
        let player = &r.player;
        eventually(|| async move {
            player
                .get_property::<String>("PlaybackStatus")
                .await
                .unwrap()
                == want
        })
        .await;
    }
}

#[tokio::test]
async fn position_is_read_on_demand_without_change_signals() {
    let r = rig(playing("dQw4w9WgXcQ", 1.0)).await;
    let props = zbus::fdo::PropertiesProxy::builder(&r.client)
        .destination(NAME)
        .unwrap()
        .path(PATH)
        .unwrap()
        .build()
        .await
        .unwrap();
    let mut changes = props.receive_properties_changed().await.unwrap();
    r.engine
        .events
        .send(EngineEvent::Position {
            seconds: 42.5,
            seeked: false,
        })
        .unwrap();
    let player = &r.player;
    eventually(
        || async move { player.get_property::<i64>("Position").await.unwrap() == 42_500_000 },
    )
    .await;
    // The spec: Position never comes as PropertiesChanged.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), changes.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn volume_set_sends_volume() {
    let mut r = rig(stopped()).await;
    r.player.set_property("Volume", 0.25f64).await.unwrap();
    match r.engine.next().await {
        EngineCmd::Volume(v) => assert_eq!(v, 0.25),
        other => panic!("expected Volume(0.25), got {other:?}"),
    }
    // Out of range is clamped to the engine's 0..=1.
    r.player.set_property("Volume", 1.5f64).await.unwrap();
    assert!(matches!(r.engine.next().await, EngineCmd::Volume(1.0)));
    r.player.set_property("Volume", -0.5f64).await.unwrap();
    assert!(matches!(r.engine.next().await, EngineCmd::Volume(0.0)));

    // The property follows the engine's state.
    let mut st = stopped();
    st.volume = 0.5;
    r.engine.events.send(EngineEvent::State(st)).unwrap();
    let player = &r.player;
    eventually(|| async move { player.get_property::<f64>("Volume").await.unwrap() == 0.5 }).await;
}

#[tokio::test]
async fn identity_and_capabilities() {
    let r = rig(stopped()).await;
    let identity: String = r.root.get_property("Identity").await.unwrap();
    assert_eq!(identity, "YouTube Music");
    for (prop, want) in [
        ("CanQuit", true),
        ("CanRaise", false),
        ("HasTrackList", false),
    ] {
        assert_eq!(
            r.root.get_property::<bool>(prop).await.unwrap(),
            want,
            "{prop}"
        );
    }
    // No .desktop file exists, so no DesktopEntry.
    assert!(r.root.get_property::<String>("DesktopEntry").await.is_err());
    for (prop, want) in [
        ("CanPlay", true),
        ("CanPause", true),
        ("CanSeek", true),
        ("CanControl", true),
        // An empty queue: nowhere to go.
        ("CanGoNext", false),
        ("CanGoPrevious", false),
        ("Shuffle", false),
    ] {
        assert_eq!(
            r.player.get_property::<bool>(prop).await.unwrap(),
            want,
            "{prop}"
        );
    }
    let s: String = r.player.get_property("PlaybackStatus").await.unwrap();
    assert_eq!(s, "Stopped");
    let l: String = r.player.get_property("LoopStatus").await.unwrap();
    assert_eq!(l, "None");
    assert_eq!(r.player.get_property::<f64>("Rate").await.unwrap(), 1.0);
}

#[tokio::test]
async fn next_and_previous_send_the_commands() {
    let mut r = rig(playing("dQw4w9WgXcQ", 1.0)).await;
    call(&r.player, "Next").await;
    assert!(matches!(r.engine.next().await, EngineCmd::Next));
    call(&r.player, "Previous").await;
    assert!(matches!(r.engine.next().await, EngineCmd::Previous));
    tokio::time::timeout(WAIT, r.hub.touched())
        .await
        .expect("Next and Previous restart the idle clock");
}

async fn properties(client: &Connection) -> zbus::fdo::PropertiesProxy<'static> {
    zbus::fdo::PropertiesProxy::builder(client)
        .destination(NAME)
        .unwrap()
        .path(PATH)
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn can_go(p: &Proxy<'_>) -> (bool, bool) {
    (
        p.get_property("CanGoNext").await.unwrap(),
        p.get_property("CanGoPrevious").await.unwrap(),
    )
}

/// The value a PropertiesChanged signal carried for `prop`, waiting for the first signal
/// that names it.
async fn changed<T>(changes: &mut zbus::fdo::PropertiesChangedStream, prop: &str) -> T
where
    T: TryFrom<OwnedValue>,
    <T as TryFrom<OwnedValue>>::Error: std::fmt::Debug,
{
    loop {
        let signal = tokio::time::timeout(WAIT, changes.next())
            .await
            .unwrap_or_else(|_| panic!("no PropertiesChanged for {prop} in time"))
            .unwrap();
        let args = signal.args().unwrap();
        if let Some(v) = args.changed_properties().get(prop) {
            let owned = OwnedValue::try_from(v.try_clone().unwrap()).unwrap();
            return T::try_from(owned).unwrap();
        }
    }
}

#[tokio::test]
async fn can_go_next_and_previous_follow_the_queue() {
    let r = rig(playing("song0000001", 1.0)).await;
    let props = properties(&r.client).await;
    let mut changes = props.receive_properties_changed().await.unwrap();
    let player = &r.player;
    let send = |q: QueueView| {
        *r.engine.queue.lock().unwrap() = q.clone();
        r.engine.events.send(queue_event(&q)).unwrap();
    };
    // The first of three: only forward.
    send(queue_of(3, Some(1), Repeat::Off));
    assert!(changed::<bool>(&mut changes, "CanGoNext").await);
    eventually(|| async move { can_go(player).await == (true, false) }).await;
    // The middle: both ways.
    send(queue_of(3, Some(2), Repeat::Off));
    assert!(changed::<bool>(&mut changes, "CanGoPrevious").await);
    eventually(|| async move { can_go(player).await == (true, true) }).await;
    // The last: only back.
    send(queue_of(3, Some(3), Repeat::Off));
    assert!(!changed::<bool>(&mut changes, "CanGoNext").await);
    eventually(|| async move { can_go(player).await == (false, true) }).await;
    // Repeat all wraps both ways, from either end.
    send(queue_of(3, Some(3), Repeat::All));
    eventually(|| async move { can_go(player).await == (true, true) }).await;
    send(queue_of(3, Some(1), Repeat::All));
    eventually(|| async move { can_go(player).await == (true, true) }).await;
    // Repeat one: a skip still moves on (it only repeats when a song ends by itself).
    send(queue_of(3, Some(3), Repeat::One));
    eventually(|| async move { can_go(player).await == (false, true) }).await;
    // Songs but none current: Next starts the first one.
    send(queue_of(2, None, Repeat::Off));
    eventually(|| async move { can_go(player).await == (true, false) }).await;
    send(empty_queue());
    eventually(|| async move { can_go(player).await == (false, false) }).await;
}

#[tokio::test]
async fn shuffle_and_loop_status_map_to_the_engine() {
    let mut r = rig(stopped()).await;
    r.player.set_property("Shuffle", true).await.unwrap();
    assert!(matches!(r.engine.next().await, EngineCmd::Shuffle(true)));
    assert!(r.player.get_property::<bool>("Shuffle").await.unwrap());
    r.player.set_property("Shuffle", false).await.unwrap();
    assert!(matches!(r.engine.next().await, EngineCmd::Shuffle(false)));

    for (loop_status, want) in [
        ("Playlist", Repeat::All),
        ("Track", Repeat::One),
        ("None", Repeat::Off),
    ] {
        r.player
            .set_property("LoopStatus", loop_status)
            .await
            .unwrap();
        match r.engine.next().await {
            EngineCmd::Repeat(got) => assert_eq!(got, want, "{loop_status}"),
            other => panic!("{loop_status}: {other:?}"),
        }
        let now: String = r.player.get_property("LoopStatus").await.unwrap();
        assert_eq!(now, loop_status);
    }
    // Not one of the spec's three: refused, and nothing reaches the engine.
    assert!(
        r.player
            .set_property("LoopStatus", "Forever")
            .await
            .is_err()
    );
    assert!(r.engine.none_within(Duration::from_millis(200)).await);
    tokio::time::timeout(WAIT, r.hub.touched())
        .await
        .expect("setting Shuffle or LoopStatus restarts the idle clock");

    // Both follow the engine (a socket client changed them), with PropertiesChanged.
    let props = properties(&r.client).await;
    let mut changes = props.receive_properties_changed().await.unwrap();
    let mut st = stopped();
    st.shuffle = true;
    st.repeat = Repeat::All;
    r.engine.events.send(EngineEvent::State(st)).unwrap();
    assert!(changed::<bool>(&mut changes, "Shuffle").await);
    let l: String = r.player.get_property("LoopStatus").await.unwrap();
    assert_eq!(l, "Playlist");
    let mut st = stopped();
    st.shuffle = true;
    st.repeat = Repeat::One;
    r.engine.events.send(EngineEvent::State(st)).unwrap();
    assert_eq!(changed::<String>(&mut changes, "LoopStatus").await, "Track");
}

/// A seek from anywhere (here the socket) moves the position under the desktop's widgets,
/// so MPRIS announces it with `Seeked`; ticks never do.
#[tokio::test]
async fn socket_seek_emits_seeked() {
    let r = rig(playing("dQw4w9WgXcQ", 1.0)).await;
    let mut seeked = r.player.receive_signal("Seeked").await.unwrap();
    // The socket on the same fake engine.
    let dir = tempfile::tempdir().unwrap();
    let (socket, path) = listener(dir.path());
    let serve = tokio::spawn(control::serve(
        socket,
        r.engine.cmds.clone(),
        r.engine.events.clone(),
        Options {
            power_supply_root: dir.path().to_path_buf(),
            ..Options::default()
        },
    ));
    // A tick first: no Seeked for it.
    r.engine
        .events
        .send(EngineEvent::Position {
            seconds: 2.0,
            seeked: false,
        })
        .unwrap();
    let stream = UnixStream::connect(&path).await.unwrap();
    let (read, mut write) = stream.into_split();
    write
        .write_all(b"{\"id\":1,\"cmd\":\"seek\",\"args\":{\"seconds\":42.5}}\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(read).lines();
    let reply = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(reply.contains("\"ok\":true"), "{reply}");
    let signal = tokio::time::timeout(WAIT, seeked.next())
        .await
        .expect("no Seeked in time")
        .unwrap();
    let at: i64 = signal.body().deserialize().unwrap();
    assert_eq!(at, 42_500_000);
    assert_eq!(
        r.player.get_property::<i64>("Position").await.unwrap(),
        42_500_000
    );
    // Exactly one Seeked for the one seek.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), seeked.next())
            .await
            .is_err()
    );
    serve.abort();
}

/// The MPRIS seek methods announce their landing once, not twice (now that every seeked
/// position is announced).
#[tokio::test]
async fn mpris_seek_emits_one_seeked() {
    let mut r = rig(playing("dQw4w9WgXcQ", 5.0)).await;
    let mut seeked = r.player.receive_signal("Seeked").await.unwrap();
    r.player
        .call_method("Seek", &(1_000_000i64,))
        .await
        .unwrap();
    r.engine.next().await;
    tokio::time::timeout(WAIT, seeked.next())
        .await
        .expect("no Seeked in time")
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), seeked.next())
            .await
            .is_err()
    );
}

/// The album and the art come from the song's queue item, on the real engine: a song added
/// over the socket with its details and jumped to shows them at once (its link never
/// resolves here).
#[tokio::test]
async fn album_and_art_come_from_the_queue_item() {
    let bus = private_bus();
    let dir = tempfile::tempdir().unwrap();
    let (task, path) = run_daemon(dir.path(), Bus::Address(bus.address.clone()));
    let conn = client(&bus.address).await;
    eventually(|| name_has_owner(&conn)).await;
    let player = proxy(&conn, PLAYER).await;

    let stream = UnixStream::connect(&path).await.unwrap();
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let add = serde_json::json!({"id": 1, "cmd": "queue.add", "args": {"songs": [
        {"videoId": "dQw4w9WgXcQ", "title": "Song", "artists": ["A", "B"],
         "album": "The Album", "thumbnail": "https://lh3.googleusercontent.com/art=w544-h544",
         "lengthSeconds": 200}]}});
    write
        .write_all(format!("{add}\n").as_bytes())
        .await
        .unwrap();
    // The queue event carries the new song's id.
    let qid = loop {
        let line = tokio::time::timeout(WAIT, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["event"] == "queue" {
            break v["items"][0]["queueId"].as_u64().unwrap();
        }
    };
    write
        .write_all(
            format!("{{\"id\":2,\"cmd\":\"queue.jump\",\"args\":{{\"queueId\":{qid}}}}}\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let p = &player;
    eventually(|| async move { metadata(p).await.contains_key("xesam:album") }).await;
    let m = metadata(&player).await;
    let album: String = m["xesam:album"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(album, "The Album");
    let art: String = m["mpris:artUrl"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(art, "https://lh3.googleusercontent.com/art=w544-h544");
    let title: String = m["xesam:title"].try_clone().unwrap().try_into().unwrap();
    assert_eq!(title, "Song");
    assert_eq!(track_id(&m), "/org/ytmfast/track/dQw4w9WgXcQ");
    // One song: nowhere to go either way.
    assert_eq!(can_go(&player).await, (false, false));

    let root = proxy(&conn, ROOT).await;
    call(&root, "Quit").await;
    assert_eq!(
        tokio::time::timeout(WAIT, task).await.unwrap().unwrap(),
        Exit::Quit
    );
}

#[tokio::test]
async fn commands_count_as_activity() {
    let mut r = rig(stopped()).await;
    call(&r.player, "Pause").await;
    r.engine.next().await;
    tokio::time::timeout(WAIT, r.hub.touched())
        .await
        .expect("an MPRIS command restarts the idle clock");
    r.player.set_property("Volume", 0.5f64).await.unwrap();
    r.engine.next().await;
    tokio::time::timeout(WAIT, r.hub.touched())
        .await
        .expect("setting the volume restarts the idle clock");
    // Reading properties is not activity: desktop widgets poll Position all the time, and
    // that must not keep an idle engine alive.
    let _: i64 = r.player.get_property("Position").await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(200), r.hub.touched())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn quit_goes_through_the_hub() {
    let mut r = rig(stopped()).await;
    call(&r.root, "Quit").await;
    tokio::time::timeout(WAIT, r.hub.quit_requested())
        .await
        .expect("Quit asks the hub to stop the daemon");
    // The hub, not MPRIS, stops the engine (the socket's quit path).
    assert!(r.engine.none_within(Duration::from_millis(100)).await);
}

#[tokio::test]
async fn lagged_events_requery_status() {
    let r = rig(stopped()).await;
    // Over the 64-event buffer in one go, before the MPRIS task gets a turn: it lags.
    for i in 0..100 {
        r.engine
            .events
            .send(EngineEvent::State(playing(&format!("old{i}"), 0.0)))
            .unwrap();
    }
    // The truth, which no event carries: only a fresh Status shows it.
    *r.engine.status.lock().unwrap() = playing("fresh", 9.0);
    let player = &r.player;
    eventually(|| async move { track_id(&metadata(player).await) == "/org/ytmfast/track/fresh" })
        .await;
    assert_eq!(
        r.player.get_property::<i64>("Position").await.unwrap(),
        9_000_000
    );
}

#[tokio::test]
async fn second_server_is_refused_and_stop_releases_the_name() {
    let r = rig(stopped()).await;
    assert!(name_has_owner(&r.client).await);
    let second = mpris::serve(
        &Bus::Address(r._bus.address.clone()),
        r.engine.cmds.clone(),
        r.engine.events.subscribe(),
        Hub::default(),
    )
    .await;
    assert!(second.is_err(), "the name is taken");
    r.mpris.stop().await;
    assert!(!name_has_owner(&r.client).await);
}

#[tokio::test]
async fn unreachable_bus_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = fake_engine(stopped());
    let res = mpris::serve(
        &Bus::Address(format!("unix:path={}/no-bus", dir.path().display())),
        engine.cmds.clone(),
        engine.events.subscribe(),
        Hub::default(),
    )
    .await;
    assert!(res.is_err());
}

// ---- The daemon: `control::run` starts MPRIS next to the socket. ----

/// Never answers: a play stays `buffering`.
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
    async fn song_next(&self, _: &str) -> Result<ytmfast::innertube::SongNext, Error> {
        std::future::pending().await
    }
    async fn like(&self, _: &str, _: ytmfast::browse::LikeStatus) -> Result<(), Error> {
        std::future::pending().await
    }
}

fn listener(dir: &Path) -> (UnixListener, std::path::PathBuf) {
    let path = dir.join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    (UnixListener::from_std(std).unwrap(), path)
}

fn run_daemon(dir: &Path, bus: Bus) -> (tokio::task::JoinHandle<Exit>, std::path::PathBuf) {
    let (listener, path) = listener(dir);
    let player = AudioPlayer::spawn(Box::new(NullSink::new()));
    let (engine, cmds, events) = Engine::new(Arc::new(Hang), Arc::new(Hang), player);
    let options = Options {
        mpris: Some(bus),
        // An empty folder: no mains, so "battery"; the limit is minutes away either way.
        power_supply_root: dir.to_path_buf(),
        ..Options::default()
    };
    let task = tokio::spawn(control::run(
        listener,
        engine,
        cmds,
        events,
        options,
        std::future::pending(),
    ));
    (task, path)
}

#[tokio::test]
async fn daemon_serves_mpris_and_releases_the_name_on_quit() {
    let bus = private_bus();
    let dir = tempfile::tempdir().unwrap();
    let (task, _path) = run_daemon(dir.path(), Bus::Address(bus.address.clone()));
    let conn = client(&bus.address).await;
    eventually(|| name_has_owner(&conn)).await;

    // MPRIS drives the real engine: a volume set shows in the engine's state.
    let player = proxy(&conn, PLAYER).await;
    player.set_property("Volume", 0.5f64).await.unwrap();
    let p = &player;
    eventually(|| async move { p.get_property::<f64>("Volume").await.unwrap() == 0.5 }).await;

    let root = proxy(&conn, ROOT).await;
    call(&root, "Quit").await;
    let exit = tokio::time::timeout(WAIT, task)
        .await
        .expect("the daemon quits")
        .unwrap();
    assert_eq!(exit, Exit::Quit);
    // Released by the time `run` returns, so before the process exits.
    assert!(!name_has_owner(&conn).await);
}

#[tokio::test]
async fn daemon_keeps_the_socket_without_a_bus() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Bus::Address(format!("unix:path={}/no-bus", dir.path().display()));
    let (task, path) = run_daemon(dir.path(), bus);
    let stream = UnixStream::connect(&path).await.unwrap();
    let (read, mut write) = stream.into_split();
    write
        .write_all(b"{\"id\":1,\"cmd\":\"status\"}\n")
        .await
        .unwrap();
    let mut lines = BufReader::new(read).lines();
    let line = tokio::time::timeout(WAIT, lines.next_line())
        .await
        .expect("a reply in time")
        .unwrap()
        .unwrap();
    assert!(line.contains("\"ok\":true"), "{line}");
    write
        .write_all(b"{\"id\":2,\"cmd\":\"quit\"}\n")
        .await
        .unwrap();
    let exit = tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
    assert_eq!(exit, Exit::Quit);
}
