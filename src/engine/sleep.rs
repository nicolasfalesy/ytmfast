//! The sleep timer: after a set time, or at the end of the song, the music fades out over
//! 10 s and pauses.
//!
//! The fade turns the stream's volume down in steps (`STEP`) instead of scaling samples, so it
//! is the same volume control a mixer shows, and `Status::volume` (the user's volume, saved
//! and shown over MPRIS) is never touched: when the music has stopped, the stream goes back to
//! that volume, so the next play is not silent. Nothing is saved while fading.
//!
//! A timer costs nothing until it is set, and then one wake-up for its fade, one every `STEP`
//! while fading, and one for the end (`Engine::sleep_wake`). A volume change from anyone (the
//! volume command, a mixer, mute) while fading means the listener is awake: the timer ends.

use std::time::Duration;

use tokio::time::Instant;

use super::{Engine, PlayState};

/// How long the fade lasts.
pub const FADE: Duration = Duration::from_secs(10);

/// The longest timer: 12 hours.
pub const SLEEP_MAX: Duration = Duration::from_secs(12 * 60 * 60);

/// How often the volume steps down while fading. A quarter second is smooth to the ear and a
/// few dozen small control changes over the whole fade.
const STEP: Duration = Duration::from_millis(250);

/// How often the volume steps in a fade of `len`: `STEP` for the real 10 s (40 steps), finer
/// for a shorter one (tests), so any fade is dozens of small steps and never a jump.
fn step_for(len: Duration) -> Duration {
    (len / 40).clamp(Duration::from_millis(10), STEP)
}

/// A fade never shorter than this, so a timer set to run out almost at once still fades.
const MIN_FADE: Duration = Duration::from_millis(50);

/// For "end of the song": how long after the fade is over the song may take to end by itself
/// before the engine pauses anyway (a song whose length was a little off).
const END_GRACE: Duration = Duration::from_secs(3);

/// How many of the latest volume steps are remembered, to tell the sound server's echo of
/// one from a mixer being turned (`Engine::sleep_is_our_echo`).
const RECENT: usize = 4;

/// What `sleep` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepSet {
    /// Fade out and pause after this long.
    In(Duration),
    /// Fade out over the end of the song playing now, and pause there.
    SongEnd,
    /// No timer.
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepKind {
    Timer,
    SongEnd,
}

/// The timer as the state shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SleepView {
    pub kind: SleepKind,
    /// Whole seconds left, for a timer (rounded up, so "1" is the last second).
    pub left: Option<u64>,
    /// The fade has begun.
    pub fading: bool,
}

pub(super) struct SleepTimer {
    mode: Mode,
    fade: Option<Fade>,
}

enum Mode {
    /// Fade out so as to end at this time.
    At(Instant),
    /// Fade out over the end of the song playing.
    SongEnd,
}

struct Fade {
    from: Instant,
    len: Duration,
    /// When the volume steps next.
    next: Instant,
    /// The last volumes set, newest last.
    recent: Vec<f32>,
}

/// How loud the stream is `elapsed` into a fade of `len`, as a share of the volume it had.
/// Squared, so it falls faster at first than a straight line: the ear hears loudness, not
/// amplitude, and a straight fade seems to hang on and then drop at the end.
pub(super) fn fade_gain(elapsed: Duration, len: Duration) -> f32 {
    if len.is_zero() {
        return 0.0;
    }
    let t = (elapsed.as_secs_f32() / len.as_secs_f32()).clamp(0.0, 1.0);
    (1.0 - t) * (1.0 - t)
}

impl Engine {
    /// Sets, replaces or turns off the timer.
    pub(super) fn sleep_set(&mut self, set: SleepSet) {
        let was_set = self.sleep.is_some();
        // A fade in progress ends first, so its quiet volume is not what the new timer starts
        // from.
        self.sleep_end(true);
        self.sleep = match set {
            SleepSet::Off => None,
            SleepSet::In(d) => Some(SleepTimer {
                mode: Mode::At(Instant::now() + d.min(SLEEP_MAX)),
                fade: None,
            }),
            SleepSet::SongEnd => Some(SleepTimer {
                mode: Mode::SongEnd,
                fade: None,
            }),
        };
        if was_set || self.sleep.is_some() {
            self.emit_state_unsaved();
        }
    }

    /// The timer as the state shows it.
    pub(super) fn sleep_view(&self) -> Option<SleepView> {
        let s = self.sleep.as_ref()?;
        let fading = s.fade.is_some();
        Some(match s.mode {
            Mode::At(at) => SleepView {
                kind: SleepKind::Timer,
                left: Some(
                    at.saturating_duration_since(Instant::now())
                        .as_secs_f64()
                        .ceil() as u64,
                ),
                fading,
            },
            Mode::SongEnd => SleepView {
                kind: SleepKind::SongEnd,
                left: None,
                fading,
            },
        })
    }

    /// When the engine next has something to do for the timer; `None` while there is nothing
    /// (no timer, or "end of the song" with the song not playing). Worked out afresh each turn
    /// of the loop, so a seek or a pause moves it.
    pub(super) fn sleep_wake(&self) -> Option<Instant> {
        let s = self.sleep.as_ref()?;
        if let Some(f) = &s.fade {
            return Some(f.next);
        }
        match s.mode {
            // Music playing: the fade starts a fade-length before the end. Not playing: nothing
            // to fade, so the timer's own time (it ends quietly, `on_sleep`).
            Mode::At(at) if self.status.state == PlayState::Playing => {
                Some(at.checked_sub(self.sleep_fade).unwrap_or_else(Instant::now))
            }
            Mode::At(at) => Some(at),
            Mode::SongEnd => {
                let left = self.song_left()?;
                let lead = left.saturating_sub(self.sleep_fade);
                Some(Instant::now() + lead)
            }
        }
    }

    /// How long the song playing has left, when it is playing and its length is known.
    fn song_left(&self) -> Option<Duration> {
        if self.status.state != PlayState::Playing || !(self.loaded && self.started) {
            return None;
        }
        let length = self.status.meta.as_ref().map(|m| m.length_seconds)?;
        if length == 0 {
            return None;
        }
        let left = f64::from(length) - self.player.position();
        Some(Duration::from_secs_f64(left.max(0.0)))
    }

    /// The volume the stream goes back to: the user's.
    fn sleep_base(&self) -> f32 {
        if self.status.muted {
            0.0
        } else {
            self.status.volume
        }
    }

    /// The timer's turn: start the fade, take a step of it, or end it.
    pub(super) fn on_sleep(&mut self) {
        let now = Instant::now();
        let Some((at, fading)) = self.sleep.as_ref().map(|s| {
            let at = match s.mode {
                Mode::At(at) => Some(at),
                Mode::SongEnd => None,
            };
            (at, s.fade.is_some())
        }) else {
            return;
        };
        if !fading {
            // Time to fade: over `sleep_fade`, or over what is left of the timer or the song.
            let left = match at {
                Some(at) => Some(at.saturating_duration_since(now)),
                None => self.song_left(),
            };
            let Some(left) = left else {
                // "End of the song" with the song gone (paused since the wake was set).
                return;
            };
            if self.status.state != PlayState::Playing {
                // Nothing audible to fade. A timer past its time just ends (a song still
                // loading is held, so it does not start by itself later); one not due yet
                // waits for the music to play (`sleep_wake` is its time, not its fade's).
                if left.is_zero() {
                    self.sleep_finish(false);
                }
                return;
            }
            let len = left.min(self.sleep_fade).max(MIN_FADE);
            if let Some(s) = self.sleep.as_mut() {
                s.fade = Some(Fade {
                    from: now,
                    len,
                    next: now,
                    recent: Vec::new(),
                });
            }
            // The state says "fading" from here.
            self.emit_state_unsaved();
        }
        let Some((from, len, song_mode)) = self.sleep.as_ref().and_then(|s| {
            let f = s.fade.as_ref()?;
            Some((f.from, f.len, matches!(s.mode, Mode::SongEnd)))
        }) else {
            return;
        };
        let elapsed = now.saturating_duration_since(from);
        if (song_mode && elapsed >= len + END_GRACE) || (!song_mode && elapsed >= len) {
            return self.sleep_finish(true);
        }
        let volume = self.sleep_base() * fade_gain(elapsed, len);
        if let Some(f) = self.sleep.as_mut().and_then(|s| s.fade.as_mut()) {
            // Done fading but the song is still ending: hold at silence until it does.
            f.next = if elapsed >= len {
                from + len + END_GRACE
            } else {
                (now + step_for(len)).min(from + len)
            };
            f.recent.push(volume);
            if f.recent.len() > RECENT {
                f.recent.remove(0);
            }
        }
        self.player.set_volume(volume);
    }

    /// The fade is over: pause, and then put the volume back (pause first, so no sound slips
    /// out at the old volume). `faded` says the volume was turned down and needs putting back.
    fn sleep_finish(&mut self, faded: bool) {
        self.sleep = None;
        if matches!(self.status.state, PlayState::Playing | PlayState::Buffering) {
            self.pause();
        }
        if faded {
            self.player.set_volume(self.sleep_base());
        }
        self.emit_state_unsaved();
    }

    /// Ends the timer for good. With `restore`, a fade in progress gives the volume back.
    /// Does not tell the clients (the caller does what it was doing and emits).
    pub(super) fn sleep_end(&mut self, restore: bool) {
        let faded = self.sleep.take().is_some_and(|s| s.fade.is_some());
        if faded && restore {
            self.player.set_volume(self.sleep_base());
        }
    }

    /// The user paused by hand while the music was fading: the timer is over and the stream
    /// goes back to the user's volume (the pause has already gone to the audio thread).
    pub(super) fn sleep_paused(&mut self) {
        if self.sleep.as_ref().is_some_and(|s| s.fade.is_some()) {
            self.sleep_end(true);
        }
    }

    /// The user changed the volume (or mute) while the music was fading: they are awake, and
    /// the timer ends. The volume they set is what stands, so nothing is put back.
    pub(super) fn sleep_volume_touched(&mut self) {
        if self.sleep.as_ref().is_some_and(|s| s.fade.is_some()) {
            self.sleep_end(false);
        }
    }

    /// Whether `v` reported by a mixer is just the sound server telling us about one of our
    /// own latest steps (which can arrive after the next step was sent). Only meaningful
    /// while fading.
    pub(super) fn sleep_is_our_echo(&self, v: f32) -> bool {
        self.sleep
            .as_ref()
            .and_then(|s| s.fade.as_ref())
            .is_some_and(|f| f.recent.iter().any(|r| (r - v).abs() < 0.002))
    }

    /// Whether a fade is in progress.
    pub(super) fn sleep_fading(&self) -> bool {
        self.sleep.as_ref().is_some_and(|s| s.fade.is_some())
    }

    /// The song changed because the user skipped or played another: "end of the song" is
    /// about the new one, so a fade begun for the old one stops and the volume comes back.
    pub(super) fn sleep_song_changed(&mut self) {
        let Some(s) = self.sleep.as_mut() else {
            return;
        };
        if matches!(s.mode, Mode::SongEnd) && s.fade.take().is_some() {
            self.player.set_volume(self.sleep_base());
        }
    }

    /// The song ended by itself. With "end of the song" set, the timer is spent: returns
    /// whether the volume was turned down, for `sleep_song_over` to finish once the next song
    /// is current. `None` when no such timer was set.
    pub(super) fn sleep_take_song_end(&mut self) -> Option<bool> {
        let s = self.sleep.as_ref()?;
        if !matches!(s.mode, Mode::SongEnd) {
            return None;
        }
        let faded = s.fade.is_some();
        self.sleep = None;
        Some(faded)
    }

    /// Finishes "end of the song" once the next song is current: it waits, paused, and the
    /// volume is back for when it plays.
    pub(super) fn sleep_song_over(&mut self, faded: Option<bool>) {
        let Some(faded) = faded else {
            return;
        };
        self.pause();
        if faded {
            self.player.set_volume(self.sleep_base());
        }
        self.emit_state_unsaved();
    }
}
