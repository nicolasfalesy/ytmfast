//! A resolver that loads the session on first use.
//!
//! The daemon serves its socket and watches for signals at once, and only reads the keyring
//! when a song is first asked for. Reading it at start would hold every reply (and a stop)
//! behind the keyring's unlock prompt, which has no timeout of its own. Until a session is
//! found, every resolve tries the store again, so `ytmfast import-session` takes effect
//! without restarting the engine.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::auth::{Session, SessionStore};
use crate::error::Error;
use crate::streams::{Resolver, Stream};

/// How long one session load may take: long enough to type a keyring password into the
/// unlock prompt, short enough that a play doesn't hang on a prompt nobody sees.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the real resolver once the session is loaded.
pub type Build = Box<dyn Fn(Session) -> Arc<dyn Resolver> + Send + Sync>;

pub struct LazyResolver {
    store: Arc<dyn SessionStore>,
    build: Build,
    load_timeout: Duration,
    /// The real resolver once a session was loaded. A tokio mutex, held across the load, so
    /// songs asked for at the same moment wait for one load instead of each prompting.
    inner: tokio::sync::Mutex<Option<Arc<dyn Resolver>>>,
}

impl LazyResolver {
    pub fn new(store: Arc<dyn SessionStore>, build: Build) -> LazyResolver {
        LazyResolver {
            store,
            build,
            load_timeout: LOAD_TIMEOUT,
            inner: tokio::sync::Mutex::new(None),
        }
    }

    /// A shorter load timeout, for tests.
    pub fn with_load_timeout(mut self, timeout: Duration) -> LazyResolver {
        self.load_timeout = timeout;
        self
    }

    /// The real resolver, loading the session first if there is none yet. A failed load is
    /// not remembered: the next call tries again.
    async fn get(&self) -> Result<Arc<dyn Resolver>, Error> {
        let mut inner = self.inner.lock().await;
        if let Some(r) = &*inner {
            return Ok(r.clone());
        }
        let session = match tokio::time::timeout(self.load_timeout, self.store.load()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                // The code only: the message is fixed text, but the code is all a log needs.
                eprintln!("ytmfast: no usable session ({})", e.code());
                return Err(e);
            }
            Err(_) => {
                eprintln!("ytmfast: the keyring did not answer in time");
                return Err(Error::Internal("keyring locked or unavailable".into()));
            }
        };
        crate::trace::mark("session loaded (keyring)");
        let r = (self.build)(session);
        *inner = Some(r.clone());
        Ok(r)
    }
}

#[async_trait]
impl Resolver for LazyResolver {
    async fn resolve(&self, video_id: &str) -> Result<Stream, Error> {
        self.get().await?.resolve(video_id).await
    }

    async fn resolve_fresh(&self, video_id: &str) -> Result<Stream, Error> {
        self.get().await?.resolve_fresh(video_id).await
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

    fn counting_build(builds: Arc<AtomicUsize>) -> Build {
        Box::new(move |s: Session| {
            builds.fetch_add(1, Ordering::SeqCst);
            Arc::new(Echo(s.cookies[0].value.clone())) as Arc<dyn Resolver>
        })
    }

    #[tokio::test]
    async fn picks_up_a_session_imported_later() {
        let store = Arc::new(MemoryStore::new());
        let builds = Arc::new(AtomicUsize::new(0));
        let lazy = LazyResolver::new(store.clone(), counting_build(builds.clone()));

        // No session yet: signed out, and nothing built.
        assert_eq!(lazy.resolve("testvideo01").await, Err(Error::SignedOut));
        assert_eq!(builds.load(Ordering::SeqCst), 0);

        // The user imports one: the next resolve uses it, without a restart.
        store.save(&session("first")).await.unwrap();
        let s = lazy.resolve("testvideo01").await.unwrap();
        assert_eq!(s.meta.title, "first");
        let s = lazy.resolve_fresh("testvideo01").await.unwrap();
        assert_eq!(s.meta.title, "first");
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
        let lazy = LazyResolver::new(store.clone(), counting_build(builds.clone()))
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

    #[test]
    fn the_load_timeout_is_ten_seconds() {
        assert_eq!(LOAD_TIMEOUT, Duration::from_secs(10));
    }
}
