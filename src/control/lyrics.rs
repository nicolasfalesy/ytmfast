//! The socket's `lyrics {videoId}`: YouTube Music's plain lyrics for a song, as `Page.js` gave
//! them (`{text, source}`, or `{none: true}` on the wire).
//!
//! Lyrics take two requests: the song's `next`, whose Lyrics tab names the lyrics page
//! (`MPLYt…`), then a browse of that page. The engine already makes that `next` for the song
//! playing (its queue's, or its like lookup's), so it keeps the tab with the like status, and
//! lyrics ask it first (`LyricsTabs`): lyrics for the current song cost only the browse. A
//! `next` that lyrics had to make goes back to the engine too, so that song needs no like
//! lookup either (ruling P6's carry).
//!
//! The answers themselves are kept here, in the daemon, for every client: the last `KEPT`
//! songs, so reopening the Lyrics tab costs nothing. A kept "none" lasts `NONE_FOR` only
//! (YouTube adds lyrics to songs later); found lyrics are kept for the daemon's life.
//! Failures are never kept: the next ask tries again.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::browse::{Browser, Lyrics};
use crate::engine::{EngineCmd, KnownTab};
use crate::error::Error;
use crate::innertube::SongNext;
use crate::streams::is_video_id;

/// Lyrics answers kept, by song. A widget's Lyrics tab follows the song playing; 20 covers
/// going back and forth through a stretch of the queue. Each is a few KB.
pub const KEPT: usize = 20;

/// How long a "this song has no lyrics" stands, here and in the engine's tab cache, before the
/// song is asked about again.
pub const NONE_FOR: Duration = Duration::from_secs(60 * 60);

/// Where lyrics learn a song's Lyrics tab, and hand back one they found: the engine's
/// per-song cache in the daemon (`EngineTabs`), a fake in tests.
#[async_trait]
pub trait LyricsTabs: Send + Sync {
    /// What is known of the song's tab; `None` when nothing is.
    async fn known(&self, video_id: &str) -> Option<KnownTab>;
    /// A song's own `next`, made for lyrics: kept for the like lookup and later lyrics.
    async fn learn(&self, video_id: &str, next: SongNext);
}

/// The engine's per-song cache, through its command channel (`EngineCmd::LyricsTab`,
/// `EngineCmd::LearnSong`). An engine that is gone knows nothing and learns nothing; lyrics
/// still work, with their own `next`.
pub struct EngineTabs(pub mpsc::Sender<EngineCmd>);

#[async_trait]
impl LyricsTabs for EngineTabs {
    async fn known(&self, video_id: &str) -> Option<KnownTab> {
        let (reply, rx) = oneshot::channel();
        let cmd = EngineCmd::LyricsTab {
            video_id: video_id.into(),
            reply,
        };
        self.0.send(cmd).await.ok()?;
        rx.await.ok().flatten()
    }

    async fn learn(&self, video_id: &str, next: SongNext) {
        let cmd = EngineCmd::LearnSong {
            video_id: video_id.into(),
            next,
        };
        let _ = self.0.send(cmd).await;
    }
}

/// The daemon's lyrics: the kept answers, and the way to the engine's tabs.
pub struct LyricsCache {
    tabs: Arc<dyn LyricsTabs>,
    /// Oldest used first. A std mutex: never held across an await.
    kept: Mutex<VecDeque<Kept>>,
}

struct Kept {
    video_id: String,
    answer: Option<Lyrics>,
    /// When a "none" was learned (for `NONE_FOR`).
    at: Instant,
}

impl LyricsCache {
    pub fn new(tabs: Arc<dyn LyricsTabs>) -> LyricsCache {
        LyricsCache {
            tabs,
            kept: Mutex::new(VecDeque::new()),
        }
    }

    /// The song's lyrics, `None` when it has none. Errors: `BadRequest` for a malformed id
    /// (nothing is sent), else the request's own (`SignedOut`, `Network`, ...), not kept.
    ///
    /// Two clients asking for one song at the same moment both fetch it: rare (the Lyrics tab
    /// is one widget's), and both answers agree.
    pub async fn get(
        &self,
        browser: &dyn Browser,
        video_id: &str,
    ) -> Result<Option<Lyrics>, Error> {
        if !is_video_id(video_id) {
            return Err(Error::BadRequest("not a video id".into()));
        }
        if let Some(answer) = self.kept(video_id) {
            return Ok(answer);
        }
        let (page, at) = match self.tabs.known(video_id).await {
            Some(KnownTab { page: Some(p), at }) => (Some(p), at),
            Some(KnownTab { page: None, at }) if at.elapsed() < NONE_FOR => (None, at),
            // Unknown, or a "no tab" old enough to ask again: the song's own `next`.
            _ => {
                let next = browser.song_next(video_id).await?;
                let page = next.lyrics_tab.clone();
                self.tabs.learn(video_id, next).await;
                (page, Instant::now())
            }
        };
        // A "none" because the engine knew of no tab dates from when it learned that, so it
        // isn't kept here for longer than there.
        let (answer, at) = match page {
            Some(p) => (browser.lyrics_page(&p).await?, Instant::now()),
            None => (None, at),
        };
        self.keep(video_id, answer.clone(), at);
        Ok(answer)
    }

    /// A kept answer, now the newest used; `None` when there is none, or only a stale "none".
    fn kept(&self, video_id: &str) -> Option<Option<Lyrics>> {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        let at = kept.iter().position(|k| k.video_id == video_id)?;
        let entry = kept.remove(at)?;
        if entry.answer.is_none() && entry.at.elapsed() >= NONE_FOR {
            return None;
        }
        let answer = entry.answer.clone();
        kept.push_back(entry);
        Some(answer)
    }

    fn keep(&self, video_id: &str, answer: Option<Lyrics>, at: Instant) {
        let mut kept = self.kept.lock().unwrap_or_else(|e| e.into_inner());
        kept.retain(|k| k.video_id != video_id);
        kept.push_back(Kept {
            video_id: video_id.into(),
            answer,
            at,
        });
        if kept.len() > KEPT {
            kept.pop_front();
        }
    }
}
