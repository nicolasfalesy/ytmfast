//! The MPRIS name must not stay lost. The bus drops a connection that sends it an invalid
//! message (a NUL in a string is one), and a session bus can also restart under the engine:
//! either way the media keys and widgets would find no player until the engine was restarted
//! by hand, and the engine said nothing.
//!
//! Each test runs its own `dbus-daemon` with no service folders, and every connection is made
//! by address, so the user's own session bus is never reached.

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;
use tokio::sync::{broadcast, mpsc, oneshot};
use ytmfast::control::Hub;
use ytmfast::engine::{EngineCmd, EngineEvent, PlayState, QueueView, Status};
use ytmfast::mpris::{self, Bus};
use ytmfast::queue::Repeat;
use ytmfast::streams::TrackMeta;
use zbus::Connection;

const NAME: &str = "org.mpris.MediaPlayer2.ytmfast";
const WAIT: Duration = Duration::from_secs(20);

/// A throwaway session bus that can be stopped and started again at the same address.
struct PrivateBus {
    dir: TempDir,
    address: String,
    daemon: Option<Child>,
}

impl PrivateBus {
    fn start() -> PrivateBus {
        let dir = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", dir.path().join("bus").display());
        let mut bus = PrivateBus {
            dir,
            address,
            daemon: None,
        };
        bus.up();
        bus
    }

    fn up(&mut self) {
        let socket = self.dir.path().join("bus");
        let _ = std::fs::remove_file(&socket);
        let config = self.dir.path().join("bus.conf");
        // No <servicedir>: nothing can be activated on this bus.
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
            .env("HOME", self.dir.path())
            .env("XDG_RUNTIME_DIR", self.dir.path())
            .env("DBUS_SESSION_BUS_ADDRESS", &self.address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("dbus-daemon (the dbus package) must be installed to run these tests");
        self.daemon = Some(daemon);
        let start = Instant::now();
        while !socket.exists() {
            assert!(start.elapsed() < WAIT, "the private bus never came up");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn down(&mut self) {
        if let Some(mut d) = self.daemon.take() {
            let _ = d.kill();
            let _ = d.wait();
        }
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        self.down();
    }
}

fn status(title: &str) -> Status {
    Status {
        state: PlayState::Playing,
        video_id: Some("dQw4w9WgXcQ".into()),
        meta: Some(TrackMeta {
            title: title.into(),
            artist: "Artist".into(),
            length_seconds: 60,
            thumbnail: None,
        }),
        album: Some("Album".into()),
        album_id: String::new(),
        queue_id: None,
        position: 0.0,
        volume: 1.0,
        muted: false,
        shuffle: false,
        repeat: Repeat::Off,
        liked: None,
        sleep: None,
    }
}

/// A fake engine side: answers `Status` and `QueueGet`, ignores the rest.
fn fake_engine() -> (mpsc::Sender<EngineCmd>, broadcast::Sender<EngineEvent>) {
    let (cmds, mut rx) = mpsc::channel(32);
    let (events, _) = broadcast::channel(64);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                EngineCmd::Status(reply) => {
                    let _ = reply.send(status("Song"));
                }
                EngineCmd::QueueGet(reply) => {
                    let _ = reply.send(QueueView {
                        items: Vec::new().into(),
                        current_id: None,
                        shuffle: false,
                        repeat: Repeat::Off,
                        rev: 0,
                    });
                }
                _ => {}
            }
        }
    });
    (cmds, events)
}

async fn client(address: &str) -> Connection {
    zbus::connection::Builder::address(address)
        .unwrap()
        .build()
        .await
        .unwrap()
}

async fn name_has_owner(address: &str) -> bool {
    let Ok(Ok(c)) = tokio::time::timeout(
        Duration::from_secs(2),
        zbus::connection::Builder::address(address).unwrap().build(),
    )
    .await
    else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(&c).await else {
        return false;
    };
    dbus.name_has_owner(NAME.try_into().unwrap())
        .await
        .unwrap_or(false)
}

async fn eventually_owned(address: &str, what: &str) {
    let start = Instant::now();
    while !name_has_owner(address).await {
        assert!(start.elapsed() < WAIT, "{what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn a_song_with_control_characters_does_not_cost_the_mpris_name() {
    let bus = PrivateBus::start();
    let (cmds, events) = fake_engine();
    let mpris = mpris::serve(
        &Bus::Address(bus.address.clone()),
        cmds,
        events.subscribe(),
        Hub::default(),
    )
    .await
    .unwrap();
    let _watcher = client(&bus.address).await;
    assert!(name_has_owner(&bus.address).await);

    // The state event a NUL in the title would come in (every source cleans its text, so this
    // is the last line of defence: the metadata is cleaned where it goes onto the bus).
    let _ = events.send(EngineEvent::State(status("x\0y\u{1b}[0m")));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        name_has_owner(&bus.address).await,
        "the bus dropped the connection and the name went with it"
    );
    drop(mpris);
}

#[tokio::test]
async fn the_name_comes_back_after_the_bus_restarts() {
    let mut bus = PrivateBus::start();
    let (cmds, events) = fake_engine();
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(mpris::run(
        Bus::Address(bus.address.clone()),
        cmds,
        events.subscribe(),
        Hub::default(),
        stop_rx,
    ));
    eventually_owned(&bus.address, "the name was never taken").await;

    // The bus goes away under the engine, and comes back.
    bus.down();
    tokio::time::sleep(Duration::from_millis(300)).await;
    bus.up();
    eventually_owned(
        &bus.address,
        "the name never came back after the bus restarted",
    )
    .await;

    // And a stop still ends it, giving the name up.
    let _ = stop_tx.send(());
    tokio::time::timeout(WAIT, task).await.unwrap().unwrap();
}
