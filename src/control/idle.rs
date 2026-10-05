//! When the daemon quits on its own: after some minutes with nothing playing, fewer on
//! battery. Under socket activation the next connection starts it again, so an idle engine
//! costs nothing.

use std::path::Path;
use std::time::Duration;

use tokio::time::Instant;

use crate::engine::PlayState;

/// Where the kernel lists power supplies.
pub const POWER_SUPPLY_ROOT: &str = "/sys/class/power_supply";

/// Minutes with nothing playing before the daemon quits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdlePolicy {
    pub ac_minutes: u64,
    pub battery_minutes: u64,
}

impl Default for IdlePolicy {
    fn default() -> Self {
        IdlePolicy {
            ac_minutes: 5,
            battery_minutes: 2,
        }
    }
}

impl IdlePolicy {
    /// How long nothing may play before quitting, on this power source.
    pub fn limit(&self, on_battery: bool) -> Duration {
        let minutes = if on_battery {
            self.battery_minutes
        } else {
            self.ac_minutes
        };
        Duration::from_secs(minutes.saturating_mul(60))
    }

    /// The shorter of the two limits: the first moment a quit could be due, whatever the
    /// power does in the meantime.
    pub fn earliest(&self) -> Duration {
        self.limit(true).min(self.limit(false))
    }
}

/// Whether the daemon should quit now.
pub fn should_quit(
    last_active: Instant,
    now: Instant,
    playing: bool,
    on_battery: bool,
    policy: IdlePolicy,
) -> bool {
    !playing && now.saturating_duration_since(last_active) >= policy.limit(on_battery)
}

/// "Playing" for the idle clock: a song is playing or about to (buffering).
pub fn is_playing(state: PlayState) -> bool {
    matches!(state, PlayState::Playing | PlayState::Buffering)
}

/// True when no power supply of type `Mains` is online.
pub fn on_battery() -> bool {
    on_battery_in(Path::new(POWER_SUPPLY_ROOT))
}

/// `on_battery` over another folder (tests). A folder that can't be read counts as battery:
/// the shorter wait is the cheaper mistake.
pub fn on_battery_in(root: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(root) else {
        return true;
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    !entries.flatten().any(|e| {
        let dir = e.path();
        read(&dir.join("type")).trim() == "Mains" && read(&dir.join("online")).trim() == "1"
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);
    const SEC: Duration = Duration::from_secs(1);

    #[test]
    fn idle_quits_after_5_min_on_ac() {
        let p = IdlePolicy::default();
        let t = Instant::now();
        assert!(!should_quit(t, t + 5 * MIN - SEC, false, false, p));
        assert!(should_quit(t, t + 5 * MIN, false, false, p));
        assert!(should_quit(t, t + 60 * MIN, false, false, p));
    }

    #[test]
    fn idle_quits_after_2_min_on_battery() {
        let p = IdlePolicy::default();
        let t = Instant::now();
        assert!(!should_quit(t, t + 2 * MIN - SEC, false, true, p));
        assert!(should_quit(t, t + 2 * MIN, false, true, p));
    }

    #[test]
    fn never_quits_while_playing() {
        let p = IdlePolicy::default();
        let t = Instant::now();
        for battery in [false, true] {
            assert!(!should_quit(t, t + 24 * 60 * MIN, true, battery, p));
        }
        assert!(is_playing(PlayState::Playing));
        assert!(is_playing(PlayState::Buffering));
        assert!(!is_playing(PlayState::Paused));
        assert!(!is_playing(PlayState::Stopped));
    }

    #[test]
    fn earliest_is_the_shorter_limit() {
        assert_eq!(IdlePolicy::default().earliest(), 2 * MIN);
        let p = IdlePolicy {
            ac_minutes: 1,
            battery_minutes: 3,
        };
        assert_eq!(p.earliest(), MIN);
    }

    fn supply(root: &Path, name: &str, kind: &str, online: Option<&str>) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("type"), format!("{kind}\n")).unwrap();
        if let Some(o) = online {
            std::fs::write(dir.join("online"), format!("{o}\n")).unwrap();
        }
    }

    #[test]
    fn battery_means_no_mains_online() {
        let root = tempfile::tempdir().unwrap();
        // Nothing listed at all, and a missing folder: battery.
        assert!(on_battery_in(root.path()));
        assert!(on_battery_in(&root.path().join("missing")));

        supply(root.path(), "BAT0", "Battery", None);
        // A USB-C port that is online is not mains.
        supply(root.path(), "ucsi-source-psy-1", "USB", Some("1"));
        supply(root.path(), "AC", "Mains", Some("0"));
        assert!(on_battery_in(root.path()));

        supply(root.path(), "AC", "Mains", Some("1"));
        assert!(!on_battery_in(root.path()));
    }
}
