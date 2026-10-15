//! Shared by the `perf_*` test binaries: when a speed limit may be checked, and small helpers.
//!
//! The limits in these tests are the numbers measured on an AC-powered laptop in the
//! `performance` power profile, plus 20% (the audit's perf pass, 2026-10-14). A limit only
//! means something in the state it was measured in, so a test that checks one first asks
//! `speed_limits_apply`, and skips with a printed line when the machine is not in that state
//! (battery or the balanced profile, a restricted CPU set, a niced process such as a
//! `nic-build`, a busy machine). Counts that do not depend on the machine (allocations, bytes
//! written, CPU ticks of an idle process) are checked everywhere.

// Each test binary uses its own part of this.
#![allow(dead_code)]

use std::process::Command;

/// The 1-minute load average above which the machine is too busy for a few-millisecond timing
/// (on the 16-thread laptop the desktop alone keeps it near 2).
const MAX_LOAD: f64 = 4.0;

/// True when time-based limits can be checked here; otherwise prints why not.
pub fn speed_limits_apply(test: &str) -> bool {
    match why_not() {
        None => true,
        Some(why) => {
            println!("{test}: speed limits skipped: {why}");
            false
        }
    }
}

fn why_not() -> Option<String> {
    if cfg!(debug_assertions) {
        return Some("a debug build (the limits are for release builds)".into());
    }
    let profile = Command::new("powerprofilesctl").arg("get").output();
    match profile {
        Ok(out) if out.status.success() => {
            let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if name != "performance" {
                return Some(format!(
                    "the power profile is {name:?}, not \"performance\""
                ));
            }
        }
        _ => return Some("powerprofilesctl is not available (no power profile to check)".into()),
    }
    // SAFETY: plain syscalls on this process; the set is a zeroed bit mask the kernel fills.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) == 0 {
            let allowed = (0..libc::CPU_SETSIZE as usize)
                .filter(|c| libc::CPU_ISSET(*c, &set))
                .count();
            let online = libc::sysconf(libc::_SC_NPROCESSORS_ONLN);
            if (allowed as i64) < online {
                return Some(format!("restricted to {allowed} of {online} CPUs"));
            }
        }
        let nice = libc::getpriority(libc::PRIO_PROCESS, 0);
        if nice > 0 {
            return Some(format!("niced to {nice} (a nic-build?)"));
        }
    }
    let load: f64 = std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0);
    if load > MAX_LOAD {
        return Some(format!("the 1-minute load average is {load}"));
    }
    None
}

pub fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// The process's own user + system CPU time in seconds.
pub fn cpu_seconds() -> f64 {
    // SAFETY: getrusage fills the struct it is given.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

/// Voluntary context switches of this process so far.
pub fn voluntary_switches() -> i64 {
    // SAFETY: as above.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_nvcsw
}
