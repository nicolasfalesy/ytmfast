//! MPRIS: the desktop's media keys, `playerctl` and media widgets drive the engine over D-Bus
//! as `org.mpris.MediaPlayer2.ytmfast`.
//!
//! Shape:
//! - Method calls and property sets become `EngineCmd`s on the engine's channel, and count
//!   as activity for the daemon's idle clock through the control hub (ruling R21). `Quit`
//!   takes the socket's quit path, through the hub too.
//! - One follower task keeps a copy of the engine's `Status` from its events and announces
//!   what changed in one `PropertiesChanged` signal. Property reads answer from that copy, so
//!   a widget polling `Position` never reaches the engine.
//!
//! Built on zbus directly (it is already in the tree through the keyring client) rather than
//! the `mpris-server` crate: that crate only connects through the process-wide session-bus
//! variable, and the tests need each server on its own private bus.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::broadcast::error::{RecvError, TryRecvError};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use zbus::fdo::RequestNameFlags;
use zbus::object_server::SignalEmitter;
use zbus::zvariant::{ObjectPath, Value};
use zbus::{Connection, fdo, interface};

use crate::control::Hub;
use crate::engine::{EngineCmd, EngineEvent, PlayState, Status};
use crate::error::Error;

/// The well-known name (Global Constraints).
pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.ytmfast";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const IDENTITY: &str = "YouTube Music";
/// The spec's id for "no track": a trackid must always be a valid object path.
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
const TRACK_PREFIX: &str = "/org/ytmfast/track/";

/// How long a quitting daemon waits for the bus to confirm the name is released. A stuck
/// bus must not hold up the exit; closing the connection releases the name anyway.
const RELEASE_WAIT: Duration = Duration::from_secs(2);

/// Which bus to serve on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bus {
    /// The user's session bus (the daemon).
    Session,
    /// A bus by address (tests use a private one).
    Address(String),
}

/// A running MPRIS server. It holds the bus name until `stop` (or until dropped, when the
/// closing connection gives it up).
pub struct Mpris {
    connection: Connection,
    follower: JoinHandle<()>,
}

impl Mpris {
    /// Gives up the bus name and closes the connection. Desktop widgets see the player go
    /// at once, rather than when the process's socket closes.
    pub async fn stop(self) {
        self.follower.abort();
        let _ = tokio::time::timeout(RELEASE_WAIT, self.connection.release_name(BUS_NAME)).await;
    }
}

impl Drop for Mpris {
    fn drop(&mut self) {
        self.follower.abort();
    }
}

/// Starts MPRIS on `bus`: takes the engine's current status, registers the two interfaces
/// and the well-known name, and starts following `events`. Fails when the bus can't be
/// reached or the name is taken (another ytmfast is running).
pub async fn serve(
    bus: &Bus,
    cmds: mpsc::Sender<EngineCmd>,
    mut events: broadcast::Receiver<EngineEvent>,
    hub: Hub,
) -> Result<Mpris, Error> {
    let status = fresh_status(&mut events, &cmds)
        .await
        .ok_or_else(|| Error::Internal("the engine stopped".into()))?;
    let state = Arc::new(Mutex::new(status));
    let player = Player {
        state: state.clone(),
        cmds: cmds.clone(),
        hub: hub.clone(),
        probe: Mutex::new(events.resubscribe()),
    };
    let builder = match bus {
        Bus::Session => zbus::connection::Builder::session(),
        Bus::Address(a) => zbus::connection::Builder::address(a.as_str()),
    };
    // The interfaces are registered before the name is requested, so no call that comes
    // with the name can find them missing.
    let connection = builder
        .and_then(|b| b.serve_at(OBJECT_PATH, Root { hub }))
        .and_then(|b| b.serve_at(OBJECT_PATH, player))
        .map_err(bus_error)?
        .build()
        .await
        .map_err(bus_error)?;
    // Asked for here rather than with `Builder::name`: zbus 5.19's builder requests without
    // DoNotQueue (despite its docs), so a second daemon would sit in the queue and take
    // over the name when the first one quits. With DoNotQueue a taken name is an error.
    connection
        .request_name_with_flags(BUS_NAME, RequestNameFlags::DoNotQueue.into())
        .await
        .map_err(bus_error)?;
    let follower = tokio::spawn(follow(connection.clone(), state, cmds, events));
    Ok(Mpris {
        connection,
        follower,
    })
}

/// The daemon's MPRIS: serves until `stop` fires, then gives the name up. Without a usable
/// session bus it logs one line and ends; the socket works on without it.
pub async fn run(
    bus: Bus,
    cmds: mpsc::Sender<EngineCmd>,
    events: broadcast::Receiver<EngineEvent>,
    hub: Hub,
    mut stop: oneshot::Receiver<()>,
) {
    let mpris = tokio::select! {
        served = serve(&bus, cmds, events, hub) => match served {
            Ok(m) => m,
            Err(e) => {
                eprintln!("ytmfast: MPRIS is off ({e})");
                return;
            }
        },
        // Stopped while still connecting: dropping the half-made connection is enough.
        _ = &mut stop => return,
    };
    let _ = stop.await;
    mpris.stop().await;
}

/// zbus errors carry D-Bus names and reasons, never session values or links.
fn bus_error(e: zbus::Error) -> Error {
    match e {
        zbus::Error::NameTaken => Error::Internal(format!("{BUS_NAME} is taken")),
        e => Error::Internal(format!("D-Bus: {e}")),
    }
}

/// The engine's status, with every event already waiting dropped first: those are older
/// than the answer, and applied after it they would roll the copy back.
async fn fresh_status(
    events: &mut broadcast::Receiver<EngineEvent>,
    cmds: &mpsc::Sender<EngineCmd>,
) -> Option<Status> {
    while !matches!(
        events.try_recv(),
        Err(TryRecvError::Empty | TryRecvError::Closed)
    ) {}
    query_status(cmds).await
}

async fn query_status(cmds: &mpsc::Sender<EngineCmd>) -> Option<Status> {
    let (tx, rx) = oneshot::channel();
    cmds.send(EngineCmd::Status(tx)).await.ok()?;
    rx.await.ok()
}

/// Keeps the status copy in step with the engine, and announces changes.
async fn follow(
    connection: Connection,
    state: Arc<Mutex<Status>>,
    cmds: mpsc::Sender<EngineCmd>,
    mut events: broadcast::Receiver<EngineEvent>,
) {
    loop {
        let status = match events.recv().await {
            Ok(EngineEvent::State(s)) => s,
            // Read on demand only: the spec says Position never comes as PropertiesChanged
            // (clients count from the last read or `Seeked`).
            Ok(EngineEvent::Position { seconds, .. }) => {
                lock(&state).position = seconds;
                continue;
            }
            // The queue's MPRIS side (CanGoNext and the rest) arrives with Task 8.
            Ok(EngineEvent::Error { .. } | EngineEvent::Queue { .. }) => continue,
            // Missed some events: ask for the whole state again (Task 8 carry).
            Err(RecvError::Lagged(_)) => match fresh_status(&mut events, &cmds).await {
                Some(s) => s,
                None => return,
            },
            Err(RecvError::Closed) => return,
        };
        let changed = {
            let mut current = lock(&state);
            let changed = changes(&current, &status);
            *current = status;
            changed
        };
        if changed.is_empty() {
            continue;
        }
        // A failed emit (the bus went away) only costs this one announcement.
        if let Ok(emitter) = SignalEmitter::new(&connection, OBJECT_PATH) {
            let _ = fdo::Properties::properties_changed(
                &emitter,
                PLAYER_INTERFACE.try_into().expect("a valid interface name"),
                changed,
                Cow::Borrowed(&[]),
            )
            .await;
        }
    }
}

/// The player properties that differ between two statuses, with their new values. All in
/// one map, so a new song is one signal rather than one per property.
fn changes(old: &Status, new: &Status) -> HashMap<&'static str, Value<'static>> {
    let mut changed = HashMap::new();
    if playback_status(old.state) != playback_status(new.state) {
        changed.insert("PlaybackStatus", Value::from(playback_status(new.state)));
    }
    if old.video_id != new.video_id || old.meta != new.meta {
        changed.insert("Metadata", Value::from(metadata(new)));
    }
    if old.volume != new.volume {
        changed.insert("Volume", Value::from(f64::from(new.volume)));
    }
    changed
}

fn lock(state: &Mutex<Status>) -> std::sync::MutexGuard<'_, Status> {
    // A panic while holding it can only have left a whole Status behind (every write is one
    // assignment), so a poisoned lock is still safe to use.
    state.lock().unwrap_or_else(|p| p.into_inner())
}

/// MPRIS has no buffering state: a song about to play shows as Playing, the way the
/// play button should look while it loads.
pub fn playback_status(state: PlayState) -> &'static str {
    match state {
        PlayState::Playing | PlayState::Buffering => "Playing",
        PlayState::Paused => "Paused",
        PlayState::Stopped => "Stopped",
    }
}

/// The trackid for a video: object path elements allow only `[A-Za-z0-9_]`, and YouTube ids
/// also use `-`, so anything else becomes `_`.
pub fn track_id(video_id: Option<&str>) -> String {
    match video_id {
        None => NO_TRACK.to_string(),
        Some(id) => {
            let mut path = String::with_capacity(TRACK_PREFIX.len() + id.len().max(1));
            path.push_str(TRACK_PREFIX);
            path.extend(
                id.chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }),
            );
            // An empty element is not a valid path either.
            if id.is_empty() {
                path.push('_');
            }
            path
        }
    }
}

fn micros(seconds: f64) -> i64 {
    // `as` saturates (and turns NaN into 0), so no position can overflow.
    (seconds * 1e6).round() as i64
}

fn metadata(status: &Status) -> HashMap<String, Value<'static>> {
    let mut m = HashMap::new();
    let id = ObjectPath::try_from(track_id(status.video_id.as_deref()))
        .expect("track_id makes valid object paths");
    m.insert("mpris:trackid".to_string(), Value::from(id));
    if let Some(meta) = status.meta.as_ref().filter(|_| status.video_id.is_some()) {
        m.insert("xesam:title".into(), Value::from(meta.title.clone()));
        // A list in the spec: a song can have several artists.
        m.insert(
            "xesam:artist".into(),
            Value::from(vec![meta.artist.clone()]),
        );
        if meta.length_seconds > 0 {
            m.insert(
                "mpris:length".into(),
                Value::from(i64::from(meta.length_seconds) * 1_000_000),
            );
        }
        if let Some(art) = &meta.thumbnail {
            m.insert("mpris:artUrl".into(), Value::from(art.clone()));
        }
    }
    m
}

/// `org.mpris.MediaPlayer2`.
struct Root {
    hub: Hub,
}

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    /// Nothing to raise: ytmfast has no window (CanRaise is false).
    fn raise(&self) {
        self.hub.touch();
    }

    /// The socket's `quit`: the hub stops the daemon, which stops the engine and then this.
    fn quit(&self) {
        self.hub.quit();
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_quit(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn identity(&self) -> String {
        IDENTITY.to_string()
    }

    // No DesktopEntry: there is no .desktop file to point at (the spec makes it optional).

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player`.
struct Player {
    state: Arc<Mutex<Status>>,
    cmds: mpsc::Sender<EngineCmd>,
    hub: Hub,
    /// Never read: only cloned (`resubscribe`) by a seek, to catch where the engine landed.
    probe: Mutex<broadcast::Receiver<EngineEvent>>,
}

impl Player {
    /// Sends a command; counts as activity for the idle clock, like a socket command.
    async fn send(&self, cmd: EngineCmd) -> fdo::Result<()> {
        self.hub.touch();
        self.cmds.send(cmd).await.map_err(|_| engine_gone())
    }

    /// Seeks to `target` seconds, then emits `Seeked` with where the engine really landed
    /// (it clamps to the song). The engine answers commands in order and announces a seek's
    /// landing as a `Position` event while handling it, so once a `Status` asked after the
    /// seek is answered, that event is already in a receiver made before the seek was sent.
    async fn seek_to(&self, target: f64, emitter: &SignalEmitter<'_>) -> fdo::Result<()> {
        let mut probe = self
            .probe
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .resubscribe();
        self.send(EngineCmd::Seek(target)).await?;
        query_status(&self.cmds).await.ok_or_else(engine_gone)?;
        let mut landed = None;
        loop {
            match probe.try_recv() {
                // Only the seek's own event: a tick that slipped in before it is not where
                // the seek landed.
                Ok(EngineEvent::Position {
                    seconds,
                    seeked: true,
                }) => landed = Some(seconds),
                Ok(_) | Err(TryRecvError::Lagged(_)) => {}
                Err(TryRecvError::Empty | TryRecvError::Closed) => break,
            }
        }
        // No landing: the engine ignored the seek (nothing loaded), so nothing moved.
        if let Some(at) = landed {
            lock(&self.state).position = at;
            Self::seeked(emitter, micros(at))
                .await
                .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        }
        Ok(())
    }
}

fn engine_gone() -> fdo::Error {
    fdo::Error::Failed("the engine stopped".into())
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    /// No queue until step 2 (CanGoNext is false): does nothing.
    fn next(&self) {
        self.hub.touch();
    }

    /// No queue until step 2 (CanGoPrevious is false): does nothing.
    fn previous(&self) {
        self.hub.touch();
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(EngineCmd::Pause).await
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(EngineCmd::Toggle).await
    }

    /// The engine has no "stopped but loaded" state yet, so Stop pauses: the song stays
    /// where it is, ready to resume.
    async fn stop(&self) -> fdo::Result<()> {
        self.send(EngineCmd::Pause).await
    }

    /// Resumes what is loaded, or plays the last song again once it ended.
    async fn play(&self) -> fdo::Result<()> {
        self.send(EngineCmd::Play {
            video_id: None,
            playlist_id: None,
            index: None,
            start_seconds: 0.0,
        })
        .await
    }

    /// Relative, in microseconds. Before the start means the start.
    async fn seek(
        &self,
        offset: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let from = lock(&self.state).position;
        let target = (from + offset as f64 / 1e6).max(0.0);
        self.seek_to(target, &emitter).await
    }

    /// Absolute, in microseconds. The spec: ignored unless `track_id` is the current track
    /// and the position is within it.
    async fn set_position(
        &self,
        track_id: ObjectPath<'_>,
        position: i64,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> fdo::Result<()> {
        let (current, length) = {
            let s = lock(&self.state);
            let length = s
                .meta
                .as_ref()
                .map(|m| i64::from(m.length_seconds) * 1_000_000)
                .filter(|l| *l > 0);
            (s.video_id.clone(), length)
        };
        let ours = current.is_some() && track_id.as_str() == self::track_id(current.as_deref());
        if !ours || position < 0 || length.is_some_and(|l| position > l) {
            self.hub.touch();
            return Ok(());
        }
        self.seek_to(position as f64 / 1e6, &emitter).await
    }

    /// No URI schemes are supported (SupportedUriSchemes is empty).
    fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        self.hub.touch();
        Err(fdo::Error::NotSupported("ytmfast opens no URIs".into()))
    }

    #[zbus(signal)]
    async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;

    #[zbus(property)]
    fn playback_status(&self) -> String {
        playback_status(lock(&self.state).state).to_string()
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, Value<'static>> {
        metadata(&lock(&self.state))
    }

    /// Linear 0.0 to 1.0, the same scale as `EngineCmd::Volume`.
    #[zbus(property)]
    fn volume(&self) -> f64 {
        f64::from(lock(&self.state).volume)
    }

    /// Clamped to 0.0..=1.0 as the engine does. The copy is updated at once: zbus announces
    /// the property right after this returns, and it must carry the new value, not the old.
    #[zbus(property)]
    async fn set_volume(&self, volume: f64) -> fdo::Result<()> {
        if volume.is_nan() {
            return Ok(());
        }
        let v = volume.clamp(0.0, 1.0) as f32;
        self.send(EngineCmd::Volume(v)).await?;
        lock(&self.state).volume = v;
        Ok(())
    }

    /// Microseconds, from the last position the engine reported.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        micros(lock(&self.state).position)
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_go_next(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_go_previous(&self) -> bool {
        false
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_seek(&self) -> bool {
        true
    }

    #[zbus(property(emits_changed_signal = "const"))]
    fn can_control(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_ids_are_object_paths() {
        assert_eq!(track_id(None), NO_TRACK);
        assert_eq!(
            track_id(Some("dQw4w9WgXcQ")),
            "/org/ytmfast/track/dQw4w9WgXcQ"
        );
        assert_eq!(track_id(Some("a-b_c")), "/org/ytmfast/track/a_b_c");
        assert_eq!(track_id(Some("")), "/org/ytmfast/track/_");
        for id in ["a-b_c", "", "ünï/code", "x.y z"] {
            assert!(ObjectPath::try_from(track_id(Some(id))).is_ok(), "{id}");
        }
    }

    #[test]
    fn buffering_shows_as_playing() {
        assert_eq!(playback_status(PlayState::Buffering), "Playing");
        assert_eq!(playback_status(PlayState::Playing), "Playing");
        assert_eq!(playback_status(PlayState::Paused), "Paused");
        assert_eq!(playback_status(PlayState::Stopped), "Stopped");
    }

    #[test]
    fn micros_round_and_saturate() {
        assert_eq!(micros(1.5), 1_500_000);
        assert_eq!(micros(f64::NAN), 0);
        assert_eq!(micros(f64::INFINITY), i64::MAX);
    }
}
