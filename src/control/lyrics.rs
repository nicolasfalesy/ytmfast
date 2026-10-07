//! The socket's `lyrics {videoId}`: the song's lyrics through the whole chain (`crate::lyrics`:
//! KuGou's word timing, LRCLIB's timed lines, YouTube Music's plain lyrics, LRCLIB's plain
//! text), as `{source, synced, words, lines}`, or `{none: true}` on the wire.
//!
//! The lyrics services are told the song's title, artists, album and length, which the engine
//! knows for the song playing and every queued one (`LyricsTabs::song`). YouTube Music's step
//! is asked last, and only when neither service has timing for the song.
//!
//! YouTube Music's lyrics take two requests: the song's `next`, whose Lyrics tab names the lyrics page
//! (`MPLYt…`), then a browse of that page. The engine already makes that `next` for the song
//! playing (its queue's, or its like lookup's), so it keeps the tab with the like status, and
//! lyrics ask it first (`LyricsTabs`): lyrics for the current song cost only the browse. A
//! `next` that lyrics had to make goes back to the engine too, so that song needs no like
//! lookup either (ruling P6's carry).
//!
//! The answers themselves are kept here, in the daemon, for every client: the last `KEPT`
//! songs, so reopening the Lyrics tab costs nothing. A kept "none" lasts `NONE_FOR` only
//! (YouTube and LRCLIB add lyrics to songs later); found lyrics are kept for the daemon's life.
//! Failures are never kept, nor anything found while one request failed: the next ask tries
//! again.

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
use crate::lyrics::{self, Found, LyricsWeb, SongFacts};
use crate::streams::is_video_id;

/// Lyrics answers kept, by song. A widget's Lyrics tab follows the song playing; 20 covers
/// going back and forth through a stretch of the queue. Each is a few KB (word timing: tens of
/// KB), and none passes `lyrics::MAX_ANSWER_BYTES`.
pub const KEPT: usize = 20;

/// The most of YouTube Music's lyrics text used: 256 KiB, cut before it becomes lines. Real
/// lyrics are a few KB.
pub const MAX_TEXT: usize = 256 * 1024;

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
    /// What the lyrics services are told of the song; `None` when the engine doesn't have it.
    async fn song(&self, video_id: &str) -> Option<SongFacts>;
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

    async fn song(&self, video_id: &str) -> Option<SongFacts> {
        let (reply, rx) = oneshot::channel();
        let cmd = EngineCmd::LyricsSong {
            video_id: video_id.into(),
            reply,
        };
        self.0.send(cmd).await.ok()?;
        rx.await.ok().flatten()
    }
}

/// The daemon's lyrics: the kept answers, the way to the engine's tabs and song details, and
/// the lyrics services.
pub struct LyricsCache {
    tabs: Arc<dyn LyricsTabs>,
    /// KuGou and LRCLIB. `None` (tests that don't fake them, and the default options) asks
    /// YouTube Music only: nothing here ever reaches the network unless the daemon passed it.
    web: Option<Arc<dyn LyricsWeb>>,
    /// Oldest used first. A std mutex: never held across an await.
    kept: Mutex<VecDeque<Kept>>,
}

struct Kept {
    video_id: String,
    answer: Option<Found>,
    /// When a "none" was learned (for `NONE_FOR`).
    at: Instant,
}

impl LyricsCache {
    pub fn new(tabs: Arc<dyn LyricsTabs>, web: Option<Arc<dyn LyricsWeb>>) -> LyricsCache {
        LyricsCache {
            tabs,
            web,
            kept: Mutex::new(VecDeque::new()),
        }
    }

    /// The song's lyrics, `None` when it has none. Errors: `BadRequest` for a malformed id
    /// (nothing is sent); YouTube Music's own error (`SignedOut`, `Network`, ...) when its step
    /// failed and nothing else was found. Neither is kept, nor an answer found while any
    /// request failed (the widget's rule: shown, not kept), nor one for a song the engine had
    /// no details of.
    ///
    /// Two clients asking for one song at the same moment both fetch it: rare (the Lyrics tab
    /// is one widget's), and both answers agree.
    pub async fn get(&self, browser: &dyn Browser, video_id: &str) -> Result<Option<Found>, Error> {
        if !is_video_id(video_id) {
            return Err(Error::BadRequest("not a video id".into()));
        }
        if let Some(answer) = self.kept(video_id) {
            return Ok(answer);
        }
        // Without the services, the song's details are not even looked up.
        let (web, song): (&dyn LyricsWeb, _) = match &self.web {
            Some(web) => (web.as_ref(), self.tabs.song(video_id).await),
            None => (&NoWeb, None),
        };
        // When YouTube Music's step ran and found nothing, its "none" dates from when that was
        // learned (it may be the engine's older "no tab").
        let none_at = &Mutex::new(Instant::now());
        let youtube = move || async move {
            let (lyrics, at) = self.youtube(browser, video_id).await?;
            *none_at.lock().unwrap_or_else(|e| e.into_inner()) = at;
            Ok(lyrics)
        };
        let outcome = lyrics::find(web, song.as_ref(), youtube).await;
        if outcome.answer.is_none()
            && let Some(e) = outcome.youtube_error
        {
            return Err(e);
        }
        // Nor when the services are there but the engine had no details for the song (a play by
        // id asks before they land): that answer may lack timing the song has.
        let partial = self.web.is_some() && song.is_none();
        if !outcome.failed && !partial {
            let at = *none_at.lock().unwrap_or_else(|e| e.into_inner());
            self.keep(video_id, outcome.answer.clone(), at);
        }
        Ok(outcome.answer)
    }

    /// YouTube Music's plain lyrics (cut to `MAX_TEXT`), and for a "none" when it was learned.
    async fn youtube(
        &self,
        browser: &dyn Browser,
        video_id: &str,
    ) -> Result<(Option<Lyrics>, Instant), Error> {
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
        Ok(match page {
            Some(p) => (browser.lyrics_page(&p).await?.map(capped), Instant::now()),
            None => (None, at),
        })
    }

    /// A kept answer, now the newest used; `None` when there is none, or only a stale "none".
    fn kept(&self, video_id: &str) -> Option<Option<Found>> {
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

    fn keep(&self, video_id: &str, answer: Option<Found>, at: Instant) {
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

/// No lyrics services (`LyricsCache::web` is `None`): never asked, since the chain gets no song.
struct NoWeb;

#[async_trait]
impl LyricsWeb for NoWeb {
    async fn get_json(&self, _: &str) -> lyrics::Fetched {
        lyrics::Fetched::Failed
    }
}

/// The lyrics with their text cut to `MAX_TEXT`, at the last character boundary at or before it,
/// before they become lines: real lyrics are a few KB, and this bounds what one answer costs.
fn capped(mut lyrics: Lyrics) -> Lyrics {
    if lyrics.text.len() > MAX_TEXT {
        let mut end = MAX_TEXT;
        while !lyrics.text.is_char_boundary(end) {
            end -= 1;
        }
        lyrics.text.truncate(end);
    }
    lyrics
}
