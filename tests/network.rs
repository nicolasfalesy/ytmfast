//! Live checks against YouTube with the real session. Not run in CI or by `cargo test`: they
//! need the session in the login keyring, the network, and (for the yt-dlp ones) yt-dlp.
//!
//! Run by hand, with any song id that should play as Premium Opus:
//! `YTMFAST_TEST_VIDEO=<id> cargo test --release --test network -- --ignored --nocapture --test-threads=1`
//! The id comes from the environment so none lives in the repo.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use url::Url;
use ytmfast::auth::{KeyringStore, Session, SessionStore};
use ytmfast::error::Error;
use ytmfast::innertube::{API_BASE, Innertube, clients};
use ytmfast::solver::{Answers, ChallengeKind, ChallengeSolver, Solver};
use ytmfast::streams::ytdlp::{YtDlp, YtDlpCommand};
use ytmfast::streams::{Resolver, Stream, Streams};
use ytmfast::{net, paths};

fn video() -> String {
    std::env::var("YTMFAST_TEST_VIDEO").expect("set YTMFAST_TEST_VIDEO to a song id")
}

/// A yt-dlp that refuses, so a passing test proves the own-code path did the work.
struct NoYtDlp;

#[async_trait]
impl YtDlp for NoYtDlp {
    async fn info_json(&self, _: &str, _: &Session) -> Result<Vec<u8>, Error> {
        Err(Error::StreamFailed("yt-dlp is off for this test".into()))
    }
}

/// A solver that always fails, so the fallback has to do the work.
struct NoSolver;

#[async_trait]
impl ChallengeSolver for NoSolver {
    fn has_player(&self, _: &str) -> bool {
        true
    }
    async fn solve_batch(
        &self,
        _: &str,
        _: Option<String>,
        _: Vec<(ChallengeKind, Vec<String>)>,
    ) -> Result<Vec<Answers>, Error> {
        Err(Error::StreamFailed(
            "the solver is off for this test".into(),
        ))
    }
}

async fn streams(solver: Arc<dyn ChallengeSolver>, ytdlp: Arc<dyn YtDlp>) -> Streams {
    let store = Arc::new(KeyringStore::new());
    let session = Arc::new(Mutex::new(store.load().await.expect("a stored session")));
    let api = Arc::new(Innertube::new(
        session.clone(),
        store,
        Url::parse(API_BASE).unwrap(),
    ));
    Streams::new(api, session, solver, ytdlp, paths::cache_dir().unwrap())
}

fn real_solver() -> Arc<dyn ChallengeSolver> {
    Arc::new(Solver::new(paths::cache_dir().unwrap()))
}

fn real_ytdlp() -> Arc<dyn YtDlp> {
    Arc::new(YtDlpCommand::new(paths::runtime_dir().unwrap()))
}

/// The first KiB of the link comes back as a 206: the link is signed and its `n` is right.
async fn check_link(s: &Stream) {
    let http = net::client(clients::TV.user_agent);
    let resp = http
        .get(&s.url)
        .header("Range", "bytes=0-1023")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);
}

#[tokio::test]
#[ignore = "live: needs the session, the network and YTMFAST_TEST_VIDEO"]
async fn resolves_premium_opus() {
    let id = video();
    let streams = streams(real_solver(), Arc::new(NoYtDlp)).await;
    let start = Instant::now();
    let first = streams.resolve(&id).await.unwrap();
    let first_ms = start.elapsed().as_millis();
    assert_eq!(first.itag, 774);
    check_link(&first).await;
    // Warm: player script, preprocessed player and JS context are all ready.
    let start = Instant::now();
    let warm = streams.resolve_fresh(&id).await.unwrap();
    let warm_ms = start.elapsed().as_millis();
    assert_eq!(warm.itag, 774);
    check_link(&warm).await;
    eprintln!("resolve: first {first_ms} ms, warm {warm_ms} ms (target under 1000 ms warm)");
}

#[tokio::test]
#[ignore = "live: needs the session, the network, yt-dlp and YTMFAST_TEST_VIDEO"]
async fn matches_ytdlp() {
    let id = video();
    let own = streams(real_solver(), Arc::new(NoYtDlp))
        .await
        .resolve_fresh(&id)
        .await
        .unwrap();
    let store = KeyringStore::new();
    let session = store.load().await.unwrap();
    let json = real_ytdlp().info_json(&id, &session).await.unwrap();
    let info: serde_json::Value = serde_json::from_slice(&json).unwrap();
    let itag: u32 = info["format_id"].as_str().unwrap().parse().unwrap();
    assert_eq!(own.itag, itag);
    assert_eq!(own.content_length, info["filesize"].as_u64());
}

#[tokio::test]
#[ignore = "live: needs the session, the network, yt-dlp and YTMFAST_TEST_VIDEO"]
async fn ytdlp_fallback_resolves() {
    let id = video();
    let streams = streams(Arc::new(NoSolver), real_ytdlp()).await;
    let start = Instant::now();
    let s = streams.resolve_fresh(&id).await.unwrap();
    eprintln!(
        "resolve through yt-dlp: {} ms, itag {}",
        start.elapsed().as_millis(),
        s.itag
    );
    assert_eq!(s.loudness_db, None);
    check_link(&s).await;
}
