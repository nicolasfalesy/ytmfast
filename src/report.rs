//! Play reports: the pings that make a song count in the account's YouTube Music history
//! (and so in its recommendations), sent the way the official web player sends them.
//!
//! When a song is first heard, its play-history links are fetched (one music web `player`
//! request, `ReportApi::tracking`), then:
//! - one playback ping (`videostatsPlaybackUrl` + `ver`, `c`, `cpn`): the one that puts the
//!   song in the history;
//! - watch-time pings (`videostatsWatchtimeUrl` + `cmt`, `st`, `et`, `len`, `state`) after
//!   10, 20 and 30 s of play and then every 40 s, plus one on every pause and seek, and a
//!   last one (`final=1`) when the song stops being played.
//!
//! The pings are best-effort (ruling S12): history only needs the playback ping. Everything
//! runs in one spawned task per play, so the engine only drops messages into a channel and
//! never waits; a failed ping is logged once per play, by its code only (ruling R6), and
//! never reaches playback. A play stopped within its first second, before its links came
//! back, sends nothing.
//!
//! "Seconds of play" are counted from the positions the engine reports (the ranges played),
//! not from a clock: a paused song adds nothing, and nothing here runs a timer. The engine's
//! own once-a-second position ticker, which only runs while playing, drives `tick`.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::sync::mpsc;
use url::Url;

use crate::error::Error;
pub use crate::innertube::Tracking;

/// What the reports need from YouTube. The resolver (`Streams`) in production: it knows the
/// current player script's signature timestamp, and owns the session's `Innertube`. Used as
/// `Arc<dyn ReportApi>` (ruling R1).
#[async_trait]
pub trait ReportApi: Send + Sync {
    /// The song's play-history links and visitor id (`Innertube::play_tracking`).
    async fn tracking(&self, video_id: &str) -> Result<Tracking, Error>;
    /// One GET of a history link, with the music web client's headers (`Innertube::ping`).
    async fn ping(&self, url: Url, visitor_data: Option<String>) -> Result<(), Error>;
}

/// The first watch-time ping comes after this much play, then every `STEP_EARLY` until
/// `EARLY_UNTIL`, then every `STEP_LATE` (the official player's cadence: 10, 20, 30, 70, 110…).
const FIRST_DUE: f64 = 10.0;
const STEP_EARLY: f64 = 10.0;
const EARLY_UNTIL: f64 = 30.0;
const STEP_LATE: f64 = 40.0;

/// A play that ends with less play than this before its links came back was skipped, not
/// heard: it is not reported (the engine starts a report the moment a song is heard).
const MIN_HEARD: f64 = 1.0;

/// The `cpn` alphabet: 64 URL-safe characters, as the web player uses.
const CPN_CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A new client playback nonce: 16 characters of `[A-Za-z0-9_-]`, one per play of a song (a
/// repeat or a replay gets a new one). It only has to be unique, not unguessable, so it comes
/// from std's randomly keyed hasher rather than a random-number crate.
pub fn cpn() -> String {
    // A counter, so two nonces made with the same keys still differ.
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let state = RandomState::new();
    let mut bits = [0u64; 2];
    for (i, b) in bits.iter_mut().enumerate() {
        let mut h = state.build_hasher();
        h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        h.write_usize(i);
        *b = h.finish();
    }
    // 16 characters of 6 bits each: 10 from the first word, 6 from the second.
    (0..16)
        .map(|i| {
            let (word, shift) = if i < 10 {
                (0, i * 6)
            } else {
                (1, (i - 10) * 6)
            };
            CPN_CHARS[((bits[word] >> shift) & 63) as usize] as char
        })
        .collect()
}

/// Starts play reports. Cheap to clone.
#[derive(Clone)]
pub struct Reporter {
    api: Arc<dyn ReportApi>,
}

impl Reporter {
    pub fn new(api: Arc<dyn ReportApi>) -> Reporter {
        Reporter { api }
    }

    /// One play of `video_id`, heard from `at` seconds, `length` seconds long (the `len` of
    /// the watch-time pings). Fetches the song's links and sends the playback ping in the
    /// background. Must be called inside a tokio runtime.
    pub fn start(&self, video_id: &str, cpn: String, at: f64, length: f64) -> PlayReport {
        let (tx, rx) = mpsc::unbounded_channel();
        let play = Play {
            api: self.api.clone(),
            video_id: video_id.to_string(),
            cpn,
            length: length.max(0.0),
            watch: Watch::new(at),
            logged: false,
        };
        tokio::spawn(play.run(rx));
        PlayReport { tx }
    }
}

/// One play's report: what the engine tells it, as it happens. Every call only queues a
/// message for the play's task; none waits. Positions are the song's, in seconds.
pub struct PlayReport {
    /// Unbounded on purpose: a full channel would make the engine wait or drop a message, and
    /// the engine sends at most about one message a second per play.
    tx: mpsc::UnboundedSender<Msg>,
}

impl PlayReport {
    /// The song is playing at `at` (the engine's once-a-second position tick).
    pub fn tick(&self, at: f64) {
        self.send(Msg::Tick(at));
    }
    /// Paused at `at`.
    pub fn pause(&self, at: f64) {
        self.send(Msg::Pause(at));
    }
    /// Playing again from `at` (where it paused, or where a seek while paused moved it).
    pub fn resume(&self, at: f64) {
        self.send(Msg::Resume(at));
    }
    /// Jumped from `from` to `to`.
    pub fn seek(&self, from: f64, to: f64) {
        self.send(Msg::Seek(from, to));
    }
    /// The play is over at `at`: it ended, or another song (or nothing) replaced it.
    pub fn end(self, at: f64) {
        self.send(Msg::End(at));
    }

    fn send(&self, msg: Msg) {
        // The task is gone only after an `End` (or a failed start): nothing left to report.
        let _ = self.tx.send(msg);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Msg {
    Tick(f64),
    Pause(f64),
    Resume(f64),
    Seek(f64, f64),
    End(f64),
}

/// One watch-time ping's values.
#[derive(Debug, Clone, Copy, PartialEq)]
struct WatchPing {
    /// The range played since the last ping.
    st: f64,
    et: f64,
    paused: bool,
    last: bool,
}

/// The watch-time bookkeeping of one play: which range is being played, how much has been
/// played in all, and when the next cadence ping is due. Pure, so the cadence is testable
/// without a server.
#[derive(Debug, Clone)]
struct Watch {
    /// Where the range being played began (the start, the last ping, a resume or a seek).
    from: f64,
    /// Seconds played in the ranges already closed.
    played: f64,
    /// The play total at which the next cadence ping is due.
    due: f64,
    paused: bool,
}

impl Watch {
    fn new(at: f64) -> Watch {
        Watch {
            from: at.max(0.0),
            played: 0.0,
            due: FIRST_DUE,
            paused: false,
        }
    }

    /// Closes the range at `at` and returns its ping.
    fn close(&mut self, at: f64, last: bool) -> WatchPing {
        let et = at.max(self.from);
        self.played += et - self.from;
        let ping = WatchPing {
            st: self.from,
            et,
            paused: self.paused,
            last,
        };
        self.from = et;
        ping
    }

    fn apply(&mut self, msg: Msg) -> Option<WatchPing> {
        match msg {
            Msg::Tick(at) => {
                if self.paused || self.played + (at - self.from).max(0.0) < self.due {
                    return None;
                }
                let ping = self.close(at, false);
                // Past every due point the range crossed: one ping, then the next one ahead.
                while self.due <= self.played {
                    self.due += if self.due < EARLY_UNTIL {
                        STEP_EARLY
                    } else {
                        STEP_LATE
                    };
                }
                Some(ping)
            }
            Msg::Pause(at) => {
                if self.paused {
                    return None;
                }
                let mut ping = self.close(at, false);
                ping.paused = true;
                self.paused = true;
                Some(ping)
            }
            Msg::Resume(at) => {
                self.paused = false;
                self.from = at.max(0.0);
                None
            }
            Msg::Seek(from, to) => {
                let ping = if self.paused {
                    // Nothing was played since the pause: the range is empty.
                    self.close(self.from, false)
                } else {
                    self.close(from, false)
                };
                self.from = to.max(0.0);
                Some(ping)
            }
            Msg::End(at) => {
                let at = if self.paused { self.from } else { at };
                Some(self.close(at, true))
            }
        }
    }
}

/// One play's task.
struct Play {
    api: Arc<dyn ReportApi>,
    video_id: String,
    cpn: String,
    length: f64,
    watch: Watch,
    /// A failure was logged for this play: one line per play at most.
    logged: bool,
}

impl Play {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        // The links first. What the engine says meanwhile is kept, in order. A play that ends
        // before they come after less than `MIN_HEARD` of play was skipped at once, not
        // listened to: it is never reported. One that played longer (the answer was slow)
        // still is, once they come.
        let mut early = Vec::new();
        let api = self.api.clone();
        let video_id = self.video_id.clone();
        let fetch = api.tracking(&video_id);
        tokio::pin!(fetch);
        let mut ended = false;
        let tracking = loop {
            tokio::select! {
                // Messages first: when both are ready, an end decides before the links do.
                biased;
                msg = rx.recv(), if !ended => match msg {
                    None => return,
                    Some(msg @ Msg::End(_)) => {
                        early.push(msg);
                        if self.played_by(&early) < MIN_HEARD {
                            return;
                        }
                        ended = true;
                    }
                    Some(msg) => early.push(msg),
                },
                t = &mut fetch => break t,
            }
        };
        let tracking = match tracking {
            Ok(t) => t,
            Err(e) => return self.log(&e),
        };
        if let Some(url) = with_params(tracking.playback_url.as_deref(), &self.playback_params()) {
            self.send(url, &tracking).await;
        }
        let mut pending = early.into_iter();
        loop {
            let msg = match pending.next() {
                Some(m) => m,
                None => match rx.recv().await {
                    Some(m) => m,
                    // Dropped without an end (the engine quit): nothing more to say.
                    None => return,
                },
            };
            let last = matches!(msg, Msg::End(_));
            if let Some(ping) = self.watch.apply(msg) {
                let params = self.watch_params(ping);
                if let Some(url) = with_params(tracking.watchtime_url.as_deref(), &params) {
                    self.send(url, &tracking).await;
                }
            }
            if last {
                return;
            }
        }
    }

    /// Seconds of play once `msgs` are applied (to a copy: nothing is reported yet).
    fn played_by(&self, msgs: &[Msg]) -> f64 {
        let mut w = self.watch.clone();
        for m in msgs {
            w.apply(*m);
        }
        w.played
    }

    /// The playback ping's parameters: exactly the spike's variant 2.
    fn playback_params(&self) -> Vec<(&'static str, String)> {
        vec![
            ("ver", "2".into()),
            ("c", "WEB_REMIX".into()),
            ("cpn", self.cpn.clone()),
        ]
    }

    fn watch_params(&self, p: WatchPing) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("ver", "2".into()),
            ("c", "WEB_REMIX".into()),
            ("cpn", self.cpn.clone()),
            // Where the song is now: the end of the range just reported.
            ("cmt", seconds(p.et)),
            ("st", seconds(p.st)),
            ("et", seconds(p.et)),
            ("len", seconds(self.length)),
            ("state", if p.paused { "paused" } else { "playing" }.into()),
        ];
        if p.last {
            params.push(("final", "1".into()));
        }
        params
    }

    async fn send(&mut self, url: Url, tracking: &Tracking) {
        if let Err(e) = self.api.ping(url, tracking.visitor_data.clone()).await {
            self.log(&e);
        }
    }

    fn log(&mut self, e: &Error) {
        if !self.logged {
            self.logged = true;
            // The code only (R6): the links carry the song's tracking tokens.
            eprintln!("ytmfast: a play report was not sent ({})", e.code());
        }
    }
}

/// `base` with `params` appended to its query, or `None` when there is no link (or it is not
/// one). The base's own parameters stay exactly as YouTube sent them.
fn with_params(base: Option<&str>, params: &[(&str, String)]) -> Option<Url> {
    let mut url = Url::parse(base?).ok()?;
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in params {
            q.append_pair(k, v);
        }
    }
    Some(url)
}

/// Seconds as the web player writes them: up to three decimals, no trailing zeros.
fn seconds(s: f64) -> String {
    let s = if s.is_finite() { s.max(0.0) } else { 0.0 };
    let text = format!("{s:.3}");
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ranges(w: &mut Watch, msgs: &[Msg]) -> Vec<(f64, f64, bool, bool)> {
        msgs.iter()
            .filter_map(|m| w.apply(*m))
            .map(|p| (p.st, p.et, p.paused, p.last))
            .collect()
    }

    #[test]
    fn cadence_from_ticks() {
        let mut w = Watch::new(0.0);
        let ticks: Vec<Msg> = (1..=150).map(|s| Msg::Tick(f64::from(s))).collect();
        let got: Vec<f64> = ranges(&mut w, &ticks).iter().map(|r| r.1).collect();
        assert_eq!(got, vec![10.0, 20.0, 30.0, 70.0, 110.0, 150.0]);
    }

    #[test]
    fn cadence_counts_play_not_position() {
        // Started at 100 s (a resumed song): the first ping after 10 s of play, at 110.
        let mut w = Watch::new(100.0);
        let ticks: Vec<Msg> = (101..=125).map(|s| Msg::Tick(f64::from(s))).collect();
        let got: Vec<f64> = ranges(&mut w, &ticks).iter().map(|r| r.1).collect();
        assert_eq!(got, vec![110.0, 120.0]);
        // A seek forward adds no play.
        let mut w = Watch::new(0.0);
        let msgs = [Msg::Tick(1.0), Msg::Seek(1.5, 100.0), Msg::Tick(101.0)];
        assert_eq!(ranges(&mut w, &msgs), vec![(0.0, 1.5, false, false)]);
    }

    #[test]
    fn a_late_tick_skips_past_due_points() {
        // A stall: the next tick comes 25 s of play later. One ping, then due at 30.
        let mut w = Watch::new(0.0);
        let got = ranges(&mut w, &[Msg::Tick(25.0), Msg::Tick(29.0), Msg::Tick(30.0)]);
        assert_eq!(
            got,
            vec![(0.0, 25.0, false, false), (25.0, 30.0, false, false)]
        );
    }

    #[test]
    fn paused_adds_nothing_and_ends_where_it_paused() {
        let mut w = Watch::new(0.0);
        let msgs = [
            Msg::Tick(4.0),
            Msg::Pause(4.5),
            // A second pause (not sent by the engine, but harmless) is no new ping.
            Msg::Pause(4.5),
            Msg::Seek(4.5, 50.0),
            Msg::End(80.0),
        ];
        assert_eq!(
            ranges(&mut w, &msgs),
            vec![
                (0.0, 4.5, true, false),
                (4.5, 4.5, true, false),
                // Ended while paused: at the seek's target, not at a later position.
                (50.0, 50.0, true, true),
            ]
        );
    }

    #[test]
    fn positions_never_go_backwards_in_a_range() {
        let mut w = Watch::new(10.0);
        // A position a hair before the range start (rounding) is an empty range, not a
        // negative one.
        assert_eq!(
            ranges(&mut w, &[Msg::End(9.99)]),
            vec![(10.0, 10.0, false, true)]
        );
    }

    #[test]
    fn seconds_are_written_short() {
        assert_eq!(seconds(0.0), "0");
        assert_eq!(seconds(10.0), "10");
        assert_eq!(seconds(150.5), "150.5");
        assert_eq!(seconds(1.23456), "1.235");
        assert_eq!(seconds(-1.0), "0");
        assert_eq!(seconds(f64::NAN), "0");
    }

    #[test]
    fn params_are_appended_to_the_link_as_it_came() {
        let url = with_params(
            Some("https://s.youtube.com/api/stats/playback?cl=1&docid=x&ei=a%2Bb"),
            &[("ver", "2".into()), ("cpn", "AbC-_".into())],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://s.youtube.com/api/stats/playback?cl=1&docid=x&ei=a%2Bb&ver=2&cpn=AbC-_"
        );
        assert_eq!(with_params(None, &[]), None);
        assert_eq!(with_params(Some("not a link"), &[]), None);
    }
}
