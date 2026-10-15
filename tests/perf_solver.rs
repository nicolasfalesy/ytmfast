//! The challenge solver stays fast and its kept player stays small. Ignored by default because a
//! real YouTube player script is not committed to the repo: point `YTMFAST_PERF_PLAYERS` at a
//! folder that holds a `players/` folder with copies of cached players (`<id>.base.js` and
//! `<id>.js` pairs, from `~/.cache/ytmfast/players`; at least 2 versions) and run
//!
//! `YTMFAST_PERF_PLAYERS=<folder> cargo test --release --test perf_solver -- --ignored --nocapture`
//!
//! The files are copied first; the folder given is never written to. Nothing touches the
//! network. Release builds, on a machine in the state the limits were measured in
//! (`perf_common`). Limits are the audit's measured values (2026-10-14) plus 20%.

mod perf_common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ytmfast::solver::{ChallengeKind, ChallengeSolver, Solver};

/// Cold: the player script is prepared (parsed and rewritten) and run: 204 ms.
const COLD_MS: f64 = 245.0;
/// Warm start from the prepared copy on disk: 173 ms.
const DISK_LOAD_MS: f64 = 208.0;
/// The first solve on a loaded player (4.1 ms) and the later ones (2.0 ms).
const FIRST_SOLVE_MS: f64 = 4.9;
const LATER_SOLVE_MS: f64 = 2.4;
/// The anonymous memory of a child process after nine version switches of the kept player:
/// 33.4 MB with a fresh runtime per switch (42.5 MB without it), plus 20%. (The audit's first
/// harness, a smaller process, saw 24.2 and 31.9 to 37.9 MB.)
const SWITCHED_ANON_KIB: u64 = 40_100;

fn players_given() -> (PathBuf, Vec<String>) {
    let given = std::env::var("YTMFAST_PERF_PLAYERS").expect("set YTMFAST_PERF_PLAYERS");
    let dir = PathBuf::from(given).join("players");
    let mut ids: Vec<String> = std::fs::read_dir(&dir)
        .expect("a players/ folder")
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".base.js")
                .map(String::from)
        })
        .collect();
    ids.sort();
    assert!(ids.len() >= 2, "need at least 2 player versions in {dir:?}");
    (dir, ids)
}

fn copy_into(from: &Path, ids: &[String], prepared: bool) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let players = tmp.path().join("players");
    std::fs::create_dir(&players).unwrap();
    for id in ids {
        std::fs::copy(
            from.join(format!("{id}.base.js")),
            players.join(format!("{id}.base.js")),
        )
        .unwrap();
        if prepared {
            std::fs::copy(
                from.join(format!("{id}.js")),
                players.join(format!("{id}.js")),
            )
            .unwrap();
        }
    }
    tmp
}

async fn solve(s: &Solver, id: &str, code: Option<String>, n: &str) -> f64 {
    let t = Instant::now();
    let r = s
        .solve_batch(id, code, vec![(ChallengeKind::N, vec![n.to_string()])])
        .await
        .unwrap();
    assert!(r[0].contains_key(n));
    t.elapsed().as_secs_f64() * 1000.0
}

fn anon_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("RssAnon:"))
        .and_then(|v| v.split_whitespace().next()?.parse().ok())
        .unwrap()
}

/// The memory half of the test, run by the test above in a child process (does nothing when
/// run on its own).
#[tokio::test]
#[ignore = "run by the_solver_stays_within_its_speed_and_memory_limits"]
async fn memory_phase_in_a_child() {
    if std::env::var("YTMFAST_PERF_CHILD").as_deref() != Ok("1") {
        return;
    }
    let (from, ids) = players_given();
    let tmp = copy_into(&from, &ids, true);
    let s = Solver::new(tmp.path().to_path_buf());
    for i in 0..9 {
        let id = &ids[i % ids.len()];
        solve(&s, id, None, "abcDEFghiJKLmnop").await;
        // The worker cleans up after it replies: let it finish before counting.
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    println!("ANON_KIB={}", anon_kib());
}

#[tokio::test]
#[ignore = "needs YTMFAST_PERF_PLAYERS: copies of real cached player scripts"]
async fn the_solver_stays_within_its_speed_and_memory_limits() {
    let speed = perf_common::speed_limits_apply("perf_solver");
    let (from, ids) = players_given();

    // Cold, for each version in a fresh solver and a folder with no prepared copy.
    let mut cold = Vec::new();
    for id in &ids {
        let tmp = copy_into(&from, std::slice::from_ref(id), false);
        let code =
            std::fs::read_to_string(tmp.path().join(format!("players/{id}.base.js"))).unwrap();
        let s = Solver::new(tmp.path().to_path_buf());
        cold.push(solve(&s, id, Some(code), "abcDEFghiJKLmnop").await);
    }
    let cold = perf_common::median(&mut cold);
    println!("cold: median {cold:.1} ms");

    // From the prepared copy on disk, then warm.
    let id = &ids[0];
    let tmp = copy_into(&from, &ids, true);
    let mut disk = Vec::new();
    let mut first = Vec::new();
    let mut later = Vec::new();
    for _ in 0..5 {
        let s = Solver::new(tmp.path().to_path_buf());
        disk.push(solve(&s, id, None, "abcDEFghiJKLmnop").await);
        first.push(solve(&s, id, None, "abcDEFghiJKLmnq0").await);
        for i in 0..10 {
            later.push(solve(&s, id, None, &format!("abcDEFghiJKLmn{i:02}")).await);
        }
    }
    let (disk, first, later) = (
        perf_common::median(&mut disk),
        perf_common::median(&mut first),
        perf_common::median(&mut later),
    );
    println!("from disk: {disk:.1} ms; first solve {first:.2} ms; later solves {later:.2} ms");

    // Memory: the kept player changing version nine times, in a process of its own (this one
    // has run other solvers, and their freed memory is still part of its size).
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "memory_phase_in_a_child",
            "--nocapture",
        ])
        .env("YTMFAST_PERF_CHILD", "1")
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    let anon: u64 = out
        .lines()
        .find_map(|l| l.strip_prefix("ANON_KIB="))
        .unwrap_or_else(|| panic!("the child printed no size:\n{out}"))
        .trim()
        .parse()
        .unwrap();
    println!("anonymous memory after 9 version switches: {anon} KiB");

    assert!(
        anon <= SWITCHED_ANON_KIB,
        "{anon} KiB anonymous after 9 switches, the limit is {SWITCHED_ANON_KIB}"
    );
    if speed {
        assert!(cold <= COLD_MS, "cold {cold:.1} ms, the limit is {COLD_MS}");
        assert!(
            disk <= DISK_LOAD_MS,
            "from disk {disk:.1} ms, the limit is {DISK_LOAD_MS}"
        );
        assert!(
            first <= FIRST_SOLVE_MS,
            "first solve {first:.2} ms, the limit is {FIRST_SOLVE_MS}"
        );
        assert!(
            later <= LATER_SOLVE_MS,
            "later solves {later:.2} ms, the limit is {LATER_SOLVE_MS}"
        );
    }
}
