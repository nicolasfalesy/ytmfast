//! The resolver and the queue source, over a session loaded on first use.
//!
//! The daemon serves its socket and watches for signals at once, and only reads the keyring
//! when a song is first asked for. Reading it at start would hold every reply (and a stop)
//! behind the keyring's unlock prompt, which has no timeout of its own. Until a session is
//! found, every resolve tries the store again, so `ytmfast import-session` takes effect
//! without restarting the engine.
//!
//! The resolver and the queue source come from one load and share one session: two loads
//! could each open an unlock prompt, and two copies of the session would drift apart as
//! YouTube rotates its cookies.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::auth::{Session, SessionStore};
use crate::engine::QueueSource;
use crate::error::Error;
use crate::innertube::{NextPage, NextRequest, Tracking};
use crate::report::ReportApi;
use crate::streams::{Resolver, Stream};
use url::Url;

/// How long one session load may take: long enough to type a keyring password into the
/// unlock prompt, short enough that a play doesn't hang on a prompt nobody sees.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// What one session load builds: the real resolver, queue source and play-report requests.
#[derive(Clone)]
pub struct Loaded {
    pub resolver: Arc<dyn Resolver>,
    pub queue: Arc<dyn QueueSource>,
    pub reports: Arc<dyn ReportApi>,
}

/// Builds the real resolver and queue source once the session is loaded.
pub type Build = Box<dyn Fn(Session) -> Loaded + Send + Sync>;

pub struct LazySession {
    store: Arc<dyn SessionStore>,
    build: Build,
    load_timeout: Duration,
    /// The real pair once a session was loaded. A tokio mutex, held across the load, so
    /// requests made at the same moment wait for one load instead of each prompting.
    inner: tokio::sync::Mutex<Inner>,
    /// How many loads have failed. Read before waiting for `inner`, so a request can tell a
    /// load that failed while it waited (it takes that load's error) from one that failed
    /// before it came (it tries again).
    failures: AtomicU64,
}

#[derive(Default)]
struct Inner {
    loaded: Option<Loaded>,
    /// The last failed load's error.
    last_error: Option<Error>,
}

impl LazySession {
    pub fn new(store: Arc<dyn SessionStore>, build: Build) -> LazySession {
        LazySession {
            store,
            build,
            load_timeout: LOAD_TIMEOUT,
            inner: tokio::sync::Mutex::new(Inner::default()),
            failures: AtomicU64::new(0),
        }
    }

    /// A shorter load timeout, for tests.
    pub fn with_load_timeout(mut self, timeout: Duration) -> LazySession {
        self.load_timeout = timeout;
        self
    }

    /// The real pair, loading the session first if there is none yet. Requests that waited
    /// behind a load that failed get its error, so one unlock prompt (and its up to 10 s) is
    /// not followed by another for each of them. A request made after a failed load tries
    /// again, so a later play (or an import) still gets through.
    async fn get(&self) -> Result<Loaded, Error> {
        let failures_before = self.failures.load(Ordering::Acquire);
        let mut inner = self.inner.lock().await;
        if let Some(r) = &inner.loaded {
            return Ok(r.clone());
        }
        // Only ever bumped under the lock, so this sees every failure made while we waited.
        if self.failures.load(Ordering::Acquire) != failures_before
            && let Some(e) = &inner.last_error
        {
            return Err(e.clone());
        }
        let loaded = match tokio::time::timeout(self.load_timeout, self.store.load()).await {
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => {
                // The code only: the message is fixed text, but the code is all a log needs.
                eprintln!("ytmfast: no usable session ({})", e.code());
                Err(e)
            }
            Err(_) => {
                eprintln!("ytmfast: the keyring did not answer in time");
                Err(Error::Internal("keyring locked or unavailable".into()))
            }
        };
        let session = match loaded {
            Ok(s) => s,
            Err(e) => {
                inner.last_error = Some(e.clone());
                self.failures.fetch_add(1, Ordering::Release);
                return Err(e);
            }
        };
        crate::trace::mark("session loaded (keyring)");
        let r = (self.build)(session);
        inner.loaded = Some(r.clone());
        inner.last_error = None;
        Ok(r)
    }
}

#[async_trait]
impl Resolver for LazySession {
    async fn resolve(&self, video_id: &str) -> Result<Stream, Error> {
        self.get().await?.resolver.resolve(video_id).await
    }

    async fn resolve_fresh(&self, video_id: &str) -> Result<Stream, Error> {
        self.get().await?.resolver.resolve_fresh(video_id).await
    }
}

#[async_trait]
impl QueueSource for LazySession {
    async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        self.get().await?.queue.next(req).await
    }
}

/// A song is only reported after it was heard, so after a resolve loaded the session: this
/// never prompts for the keyring by itself in practice.
#[async_trait]
impl ReportApi for LazySession {
    async fn tracking(&self, video_id: &str) -> Result<Tracking, Error> {
        self.get().await?.reports.tracking(video_id).await
    }

    async fn ping(&self, url: Url, visitor_data: Option<String>) -> Result<(), Error> {
        self.get().await?.reports.ping(url, visitor_data).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Cookie, MemoryStore};
    use crate::innertube::Tracking;
    use crate::streams::TrackMeta;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every id with a stream whose title is the session's first cookie value, so a
    /// test can tell which session it was built from.
    struct Echo(String);

    #[async_trait]
    impl Resolver for Echo {
        async fn resolve(&self, video_id: &str) -> Result<Stream, Error> {
            Ok(Stream {
                video_id: video_id.into(),
                url: String::new(),
                itag: 774,
                mime: String::new(),
                content_length: None,
                expires_unix: 0,
                loudness_db: None,
                meta: TrackMeta {
                    title: self.0.clone(),
                    ..TrackMeta::default()
                },
                tracking: Tracking::default(),
            })
        }
        async fn resolve_fresh(&self, video_id: &str) -> Result<Stream, Error> {
            self.resolve(video_id).await
        }
    }

    fn session(value: &str) -> Session {
        Session {
            cookies: vec![Cookie {
                domain: ".youtube.com".into(),
                name: "SAPISID".into(),
                value: value.into(),
                path: "/".into(),
                secure: true,
                expires_utc: None,
            }],
        }
    }

    /// Answers every queue request with one song whose title is the session's first cookie
    /// value, like `Echo`.
    struct EchoQueue(String);

    #[async_trait]
    impl QueueSource for EchoQueue {
        async fn next(&self, _: NextRequest) -> Result<NextPage, Error> {
            Ok(NextPage {
                items: vec![crate::innertube::SongItem {
                    title: self.0.clone(),
                    ..Default::default()
                }],
                ..NextPage::default()
            })
        }
    }

    /// Answers every tracking request with the session's first cookie value as the visitor
    /// id, like `Echo`.
    struct EchoReports(String);

    #[async_trait]
    impl ReportApi for EchoReports {
        async fn tracking(&self, _: &str) -> Result<Tracking, Error> {
            Ok(Tracking {
                visitor_data: Some(self.0.clone()),
                ..Tracking::default()
            })
        }
        async fn ping(&self, _: Url, _: Option<String>) -> Result<(), Error> {
            Ok(())
        }
    }

    fn counting_build(builds: Arc<AtomicUsize>) -> Build {
        Box::new(move |s: Session| {
            builds.fetch_add(1, Ordering::SeqCst);
            let value = s.cookies[0].value.clone();
            Loaded {
                resolver: Arc::new(Echo(value.clone())),
                queue: Arc::new(EchoQueue(value.clone())),
                reports: Arc::new(EchoReports(value)),
            }
        })
    }

    #[tokio::test]
    async fn picks_up_a_session_imported_later() {
        let store = Arc::new(MemoryStore::new());
        let builds = Arc::new(AtomicUsize::new(0));
        let lazy = LazySession::new(store.clone(), counting_build(builds.clone()));

        // No session yet: signed out, and nothing built.
        assert_eq!(lazy.resolve("testvideo01").await, Err(Error::SignedOut));
        assert_eq!(
            lazy.next(NextRequest::default()).await,
            Err(Error::SignedOut)
        );
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        // The user imports one: the next resolve uses it, without a restart.
        store.save(&session("first")).await.unwrap();
        let s = lazy.resolve("testvideo01").await.unwrap();
        assert_eq!(s.meta.title, "first");
        let s = lazy.resolve_fresh("testvideo01").await.unwrap();
        assert_eq!(s.meta.title, "first");
        // The queue source comes from the same load, over the same session.
        let page = lazy.next(NextRequest::default()).await.unwrap();
        assert_eq!(page.items[0].title, "first");
        // So do the play reports.
        let t = lazy.tracking("testvideo01").await.unwrap();
        assert_eq!(t.visitor_data.as_deref(), Some("first"));
        // Built once, then kept: the store isn't read on every song.
        assert_eq!(builds.load(Ordering::SeqCst), 1);
    }

    /// A store whose load never answers: a keyring waiting on its unlock prompt.
    #[derive(Default)]
    struct StuckStore {
        loads: Mutex<usize>,
    }

    #[async_trait]
    impl SessionStore for StuckStore {
        async fn load(&self) -> Result<Session, Error> {
            *self.loads.lock().unwrap() += 1;
            std::future::pending().await
        }
        async fn save(&self, _: &Session) -> Result<(), Error> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_stuck_keyring_times_out() {
        let store = Arc::new(StuckStore::default());
        let builds = Arc::new(AtomicUsize::new(0));
        let lazy = LazySession::new(store.clone(), counting_build(builds.clone()))
            .with_load_timeout(Duration::from_millis(50));
        let started = std::time::Instant::now();
        assert_eq!(
            lazy.resolve("testvideo01").await,
            Err(Error::Internal("keyring locked or unavailable".into()))
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        // Tried again on the next song.
        assert!(lazy.resolve("testvideo01").await.is_err());
        assert_eq!(*store.loads.lock().unwrap(), 2);
        assert_eq!(builds.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn waiters_behind_a_failed_load_get_its_error() {
        // Two songs asked for at once while the keyring hangs: one unlock wait (one prompt),
        // and both get its error, instead of the second waiting out a second prompt.
        let store = Arc::new(StuckStore::default());
        let lazy = Arc::new(
            LazySession::new(store.clone(), counting_build(Arc::default()))
                .with_load_timeout(Duration::from_millis(100)),
        );
        let (a, b) = tokio::join!(lazy.resolve("testvideo01"), async {
            // Starts waiting while the first load is under way.
            tokio::time::sleep(Duration::from_millis(20)).await;
            lazy.next(NextRequest::default()).await
        });
        let want = Error::Internal("keyring locked or unavailable".into());
        assert_eq!(a, Err(want.clone()));
        assert_eq!(b, Err(want));
        assert_eq!(*store.loads.lock().unwrap(), 1);
        // A later play tries again.
        assert!(lazy.resolve("testvideo01").await.is_err());
        assert_eq!(*store.loads.lock().unwrap(), 2);
    }

    #[test]
    fn the_load_timeout_is_ten_seconds() {
        assert_eq!(LOAD_TIMEOUT, Duration::from_secs(10));
    }
}
