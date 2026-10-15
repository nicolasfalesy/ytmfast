//! How many clients the control socket serves, and for how long a silent one stays.
//!
//! Without a cap and an idle limit, a script that opens connections and says nothing uses a
//! descriptor and some memory per connection until the daemon can no longer accept (the
//! limit is 1,024), and the bar widget's own connection fails with it.
//!
//! The socket is in a temp folder, with a fake engine side that answers `status`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;
use ytmfast::control::{self, Exit, Options};
use ytmfast::engine::{EngineCmd, EngineEvent, PlayState, Status};
use ytmfast::queue::Repeat;

const WAIT: Duration = Duration::from_secs(5);

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

struct Server {
    path: PathBuf,
    _serve: JoinHandle<Exit>,
    _dir: TempDir,
}

fn server(max_clients: usize, client_idle: Duration) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let (std, _bound) = control::bind_socket(&path).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let (cmds, mut rx) = mpsc::channel(32);
    let (events, _) = broadcast::channel::<EngineEvent>(64);
    tokio::spawn(async move {
        while let Some(cmd) = rx.recv().await {
            if let EngineCmd::Status(reply) = cmd {
                let _ = reply.send(stopped());
            }
        }
    });
    let options = Options {
        idle: None,
        max_clients,
        client_idle,
        ..Options::default()
    };
    Server {
        path,
        _serve: tokio::spawn(control::serve(listener, cmds, events, options)),
        _dir: dir,
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

impl Client {
    async fn ask(&mut self, line: &str) -> Value {
        self.write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let reply = tokio::time::timeout(WAIT, self.lines.next_line())
            .await
            .expect("no reply in time")
            .unwrap()
            .expect("the daemon closed the connection");
        serde_json::from_str(&reply).unwrap()
    }

    /// The next line, or `None` once the daemon closed the connection.
    async fn next(&mut self, within: Duration) -> Option<Value> {
        let line = tokio::time::timeout(within, self.lines.next_line())
            .await
            .ok()?
            .ok()??;
        serde_json::from_str(&line).ok()
    }

    /// True when the daemon closes this connection within `within`.
    async fn closed_within(&mut self, within: Duration) -> bool {
        let start = std::time::Instant::now();
        while start.elapsed() < within {
            match tokio::time::timeout(within - start.elapsed(), self.lines.next_line()).await {
                Ok(Ok(None)) | Ok(Err(_)) => return true,
                Ok(Ok(Some(_))) => continue,
                Err(_) => return false,
            }
        }
        false
    }
}

const STATUS: &str = r#"{"id":1,"cmd":"status"}"#;

#[tokio::test]
async fn clients_over_the_cap_are_told_why_and_closed() {
    let s = server(3, Duration::from_secs(600));
    let mut held = Vec::new();
    for _ in 0..3 {
        let mut c = connect(&s.path).await;
        assert_eq!(c.ask(STATUS).await["ok"], true);
        held.push(c);
    }
    // The fourth is answered with a clear error line, then closed.
    let mut extra = connect(&s.path).await;
    let line = extra
        .next(WAIT)
        .await
        .expect("a refused client gets a line saying why");
    assert_eq!(line["ok"], false);
    assert_eq!(line["error"]["code"], "unavailable");
    assert!(
        line["error"]["message"]
            .as_str()
            .unwrap()
            .contains("too many"),
        "{line}"
    );
    assert!(extra.closed_within(WAIT).await, "and then closed");
    // The ones inside are served as before.
    assert_eq!(held[0].ask(STATUS).await["ok"], true);

    // A place freed is a place for the next.
    drop(held.pop());
    let mut again = None;
    for _ in 0..50 {
        let mut c = connect(&s.path).await;
        c.write
            .write_all(format!("{STATUS}\n").as_bytes())
            .await
            .unwrap();
        if c.next(WAIT).await.is_some_and(|l| l["ok"] == true) {
            again = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(again.is_some(), "a freed place was never taken again");
}

#[tokio::test]
async fn a_connection_that_says_nothing_is_closed_after_the_idle_time() {
    let s = server(64, Duration::from_millis(400));
    // Says nothing.
    let mut silent = connect(&s.path).await;
    // Asks once, then only listens (the bar widget's way): a client, kept.
    let mut asked = connect(&s.path).await;
    assert_eq!(asked.ask(STATUS).await["ok"], true);
    // Watches, then says nothing: a listener, kept.
    let mut watching = connect(&s.path).await;
    assert_eq!(
        watching
            .ask(r#"{"id":1,"cmd":"watch","args":{"queue":false}}"#)
            .await["ok"],
        true
    );
    assert!(
        silent.closed_within(Duration::from_secs(2)).await,
        "a connection that never sent anything was kept"
    );
    // Well past two idle times: the other two are both still served.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(asked.ask(STATUS).await["ok"], true);
    assert_eq!(watching.ask(STATUS).await["ok"], true);
}

/// Child side of `running_out_of_descriptors_is_logged_once_not_every_100_ms`: the daemon's
/// socket with few descriptors to give (the parent set the limit), for a few seconds. Does
/// nothing unless the parent asks.
#[tokio::test]
async fn child_serves_with_few_descriptors() {
    let Some(path) = std::env::var_os("YTMFAST_CHILD_SOCKET") else {
        return;
    };
    let (std, _bound) = control::bind_socket(Path::new(&path)).unwrap();
    let listener = UnixListener::from_std(std).unwrap();
    let (cmds, mut rx) = mpsc::channel(32);
    let (events, _) = broadcast::channel::<EngineEvent>(64);
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let options = Options {
        idle: None,
        max_clients: 10_000,
        ..Options::default()
    };
    let serve = tokio::spawn(control::serve(listener, cmds, events, options));
    tokio::time::sleep(Duration::from_secs(5)).await;
    serve.abort();
}

#[tokio::test]
async fn running_out_of_descriptors_is_logged_once_not_every_100_ms() {
    if std::env::var_os("YTMFAST_CHILD_SOCKET").is_some() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(control::SOCKET_NAME);
    let copy = dir.path().join("omarchy-crash-ytmfast-ctl");
    std::fs::copy(std::env::current_exe().unwrap(), &copy).unwrap();
    // 20 descriptors: the test harness and the listener use a dozen, the rest run out fast.
    let mut child = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("ulimit -n 20; exec \"$0\" --exact child_serves_with_few_descriptors --nocapture")
        .arg(&copy)
        .env("YTMFAST_CHILD_SOCKET", &path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let start = std::time::Instant::now();
    while !path.exists() {
        assert!(start.elapsed() < WAIT, "the child never bound its socket");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Many clients: more than the child has descriptors for. They stay connected.
    let mut held = Vec::new();
    for _ in 0..60 {
        if let Ok(c) = UnixStream::connect(&path).await {
            held.push(c);
        }
    }
    // About 30 accept attempts at 100 ms apiece in the 3 s that follow.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = child.kill();
    let mut stderr = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
    let _ = child.wait();
    drop(held);
    let failures = stderr
        .lines()
        .filter(|l| l.contains("accept failed"))
        .count();
    assert!(
        failures >= 1,
        "the child never ran out of descriptors, so this proved nothing:\n{stderr}"
    );
    assert_eq!(
        failures, 1,
        "one line for the first failure, not one per attempt:\n{stderr}"
    );
}
