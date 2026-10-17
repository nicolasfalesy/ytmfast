//! Clear queue, with an undo for a few seconds.
//!
//! "Clear" takes every song after the current one out of the queue: the song playing keeps
//! playing, and the songs already heard stay behind it (Previous still reaches them). The
//! songs taken are kept for `UNDO_FOR`, to be put back as they were.
//!
//! A cleared queue is "about to run out", which would make the engine ask for radio songs
//! at once and fill it again, undoing the clear. So the radio waits (`Engine::refill_hold`)
//! until the song is nearly over, the same moment the next song is preloaded: the music still
//! carries on after the song, as it does when a queue runs out by itself, with no gap.

use std::time::Duration;

use tokio::time::Instant;

use super::{Engine, PRELOAD_LEAD_SECS};
use crate::queue::UpNext;

/// How long an undo is offered.
pub const UNDO_FOR: Duration = Duration::from_secs(10);

/// What a clear took out, and what the queue's source looked like, so an undo puts back all of
/// it.
pub(super) struct Cleared {
    pub(super) at: Instant,
    /// The queue's revision just after the clear: any later queue change makes it differ, and
    /// an undo then would not know where the songs belong.
    pub(super) rev: u64,
    pub(super) songs: UpNext,
    pub(super) continuation: Option<String>,
    pub(super) continuation_radio: bool,
    pub(super) exhausted: bool,
}

impl Engine {
    /// Takes the songs after the current one out of the queue; how many it took.
    pub(super) fn queue_clear(&mut self) -> usize {
        let songs = self.queue.clear_up_next();
        let count = songs.len();
        if count == 0 {
            return 0;
        }
        // The rest of a list (its next page) went with the songs: what is asked for next is
        // the radio of the song playing, as for any queue that ran out. A radio request already
        // on its way would put songs back that the user just cleared.
        let cleared = Cleared {
            at: Instant::now(),
            rev: 0,
            songs,
            continuation: self.continuation.take(),
            continuation_radio: std::mem::take(&mut self.continuation_radio),
            exhausted: std::mem::take(&mut self.exhausted),
        };
        if let Some(task) = self.refilling.take() {
            task.abort();
        }
        self.refill_hold = true;
        // Every queue event drops a pending undo (`emit_queue_event`), so this one is told
        // first and the undo is kept after it.
        self.emit_queue();
        self.cleared = Some(Cleared {
            rev: self.queue_rev,
            ..cleared
        });
        count
    }

    /// Puts back what the last clear took out; how many came back, 0 when there is nothing
    /// to undo: none was cleared, `UNDO_FOR` has passed, or the queue changed since.
    pub(super) fn queue_undo(&mut self) -> usize {
        let Some(cleared) = self.cleared.take() else {
            return 0;
        };
        if cleared.rev != self.queue_rev || cleared.at.elapsed() > UNDO_FOR {
            return 0;
        }
        let count = cleared.songs.len();
        self.queue.restore_up_next(cleared.songs);
        self.continuation = cleared.continuation;
        self.continuation_radio = cleared.continuation_radio;
        self.exhausted = cleared.exhausted;
        self.refill_hold = false;
        self.emit_queue();
        self.maybe_refill();
        count
    }

    /// Run on each position update: once the song is within `PRELOAD_LEAD_SECS` of its end the
    /// radio held back by a clear is asked for, so the next song is ready when it ends.
    pub(super) fn release_refill_hold(&mut self, position: f64) {
        if self.refill_hold && self.song_end() - position <= PRELOAD_LEAD_SECS {
            self.refill_hold = false;
            self.maybe_refill();
        }
    }
}
