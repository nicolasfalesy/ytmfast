//! Lets a log line through the first time and then once a minute at most.

use std::time::{Duration, Instant};

/// Lets a log line through the first time, and then once per `EVERY` at most, counting what
/// it held back. For lines a client or the system can make the daemon write over and over
/// (a refused connection, a failing accept, a state file that can't be written): one a minute
/// is enough to see it, and a journal flooded at ten lines a second helps nobody.
pub struct LogGate {
    last: Option<Instant>,
    held_back: u64,
}

impl LogGate {
    const EVERY: Duration = Duration::from_secs(60);

    pub fn new() -> LogGate {
        LogGate {
            last: None,
            held_back: 0,
        }
    }

    /// `Some(n)` when the line may be written now, `n` being how many were held back since the
    /// last one; `None` when it is held back.
    pub fn allow(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(last) if now.duration_since(last) < Self::EVERY => {
                self.held_back += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.held_back))
            }
        }
    }
}

impl Default for LogGate {
    fn default() -> Self {
        Self::new()
    }
}

/// " (and N more since the last)" for a log line that held some back, else nothing.
pub fn more_since(held_back: u64) -> String {
    if held_back == 0 {
        String::new()
    } else {
        format!(" ({held_back} more since the last such line)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repeating_log_line_is_written_once_a_minute() {
        // A failing accept comes every 100 ms (`ACCEPT_BACKOFF`): 120 lines in 12 s without the
        // gate, one with it, and one a minute after.
        let start = Instant::now();
        let mut gate = LogGate::new();
        let mut written = Vec::new();
        for tick in 0..1300u64 {
            let now = start + Duration::from_millis(100 * tick);
            if let Some(held_back) = gate.allow(now) {
                written.push((tick, held_back));
            }
        }
        // At 0 s, then at 60 s and 120 s: each says how many it held back.
        assert_eq!(written, [(0, 0), (600, 599), (1200, 599)]);
        assert_eq!(more_since(0), "");
        assert!(more_since(599).contains("599 more"));
    }
}
