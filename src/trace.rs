//! `YTMFAST_TRACE=1`: one stderr line per phase of a play, in milliseconds since the play
//! command, for finding where a slow start goes. For example:
//!
//! ```text
//! ytmfast trace:     0.0 ms play dQw4w9WgXcQ
//! ytmfast trace:    41.3 ms player version (iframe_api)
//! ytmfast trace:   212.9 ms player request answered
//! ```
//!
//! Off unless the variable is exactly `1`, and free when off: every call is one relaxed atomic
//! load, with no clock read and no formatting. The lines name phases only: never a link, a
//! session value, or any id but the song's video id.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::streams::is_video_id;

/// The environment variable that turns the trace on.
pub const ENV: &str = "YTMFAST_TRACE";

/// A trace: whether it is on, and when the current play started. One process-wide instance
/// (`TRACE`); tests make their own.
struct Trace {
    on: AtomicBool,
    start: Mutex<Option<Instant>>,
}

static TRACE: Trace = Trace::new();

impl Trace {
    const fn new() -> Trace {
        Trace {
            on: AtomicBool::new(false),
            start: Mutex::new(None),
        }
    }

    /// Restarts the clock for a play of `video_id`; its line when the trace is on.
    fn play_line(&self, video_id: &str) -> Option<String> {
        if !self.on.load(Ordering::Relaxed) {
            return None;
        }
        *self.start.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
        // Only a real video id: the play command's id is the client's text until checked.
        let id = if is_video_id(video_id) {
            video_id
        } else {
            "(not a video id)"
        };
        Some(line(Duration::ZERO, &format!("play {id}")))
    }

    /// `phase`'s line, timed from the last play; none before any play or when off.
    fn mark_line(&self, phase: &str) -> Option<String> {
        if !self.on.load(Ordering::Relaxed) {
            return None;
        }
        let start = (*self.start.lock().unwrap_or_else(|e| e.into_inner()))?;
        Some(line(start.elapsed(), phase))
    }
}

fn line(since: Duration, phase: &str) -> String {
    format!(
        "ytmfast trace: {:>7.1} ms {phase}",
        since.as_secs_f64() * 1000.0
    )
}

/// Whether `value` (the variable's value, if set) turns the trace on: exactly `1`.
fn wanted(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

/// Reads `YTMFAST_TRACE` once, at start.
pub fn init_from_env() {
    TRACE
        .on
        .store(wanted(std::env::var_os(ENV).as_deref()), Ordering::Relaxed);
}

/// Whether the trace is on: for a caller that would do extra work to find a phase.
#[inline]
pub fn enabled() -> bool {
    TRACE.on.load(Ordering::Relaxed)
}

/// A play command for `video_id`: the clock restarts here.
#[inline]
pub fn play(video_id: &str) {
    if enabled()
        && let Some(l) = TRACE.play_line(video_id)
    {
        eprintln!("{l}");
    }
}

/// A phase of the current play ended (or happened). `phase` is fixed text.
#[inline]
pub fn mark(phase: &'static str) {
    if enabled()
        && let Some(l) = TRACE.mark_line(phase)
    {
        eprintln!("{l}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn on() -> Trace {
        let t = Trace::new();
        t.on.store(true, Ordering::Relaxed);
        t
    }

    #[test]
    fn off_prints_nothing_and_reads_no_clock() {
        let t = Trace::new();
        assert_eq!(t.play_line("dQw4w9WgXcQ"), None);
        assert_eq!(t.mark_line("decoder open"), None);
        // The play didn't even start the clock.
        assert!(t.start.lock().unwrap().is_none());
    }

    #[test]
    fn lines_are_ms_since_the_play() {
        let t = on();
        assert_eq!(
            t.play_line("dQw4w9WgXcQ").as_deref(),
            Some("ytmfast trace:     0.0 ms play dQw4w9WgXcQ")
        );
        std::thread::sleep(Duration::from_millis(20));
        let l = t.mark_line("decoder open").unwrap();
        let (head, phase) = l.split_once(" ms ").unwrap();
        assert_eq!(phase, "decoder open");
        let ms: f64 = head
            .strip_prefix("ytmfast trace:")
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!((20.0..1000.0).contains(&ms), "{ms}");
    }

    #[test]
    fn a_new_play_restarts_the_clock() {
        let t = on();
        t.play_line("dQw4w9WgXcQ");
        std::thread::sleep(Duration::from_millis(30));
        t.play_line("AAAAAAAAAAA");
        let l = t.mark_line("x").unwrap();
        assert!(l.starts_with("ytmfast trace:     0."), "{l}");
    }

    #[test]
    fn no_phase_before_a_play() {
        assert_eq!(on().mark_line("decoder open"), None);
    }

    #[test]
    fn only_a_real_video_id_is_printed() {
        let l = on().play_line("https://example.com/?a=b").unwrap();
        assert_eq!(l, "ytmfast trace:     0.0 ms play (not a video id)");
    }

    #[test]
    fn only_exactly_1_turns_it_on() {
        assert!(wanted(Some(OsStr::new("1"))));
        assert!(!wanted(Some(OsStr::new("0"))));
        assert!(!wanted(Some(OsStr::new(""))));
        assert!(!wanted(Some(OsStr::new("yes"))));
        assert!(!wanted(None));
    }

    #[test]
    fn line_format() {
        assert_eq!(
            line(Duration::from_micros(1_234_567), "stream running"),
            "ytmfast trace:  1234.6 ms stream running"
        );
    }
}
