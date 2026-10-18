//! How the yt-dlp fallback process is started. The "program" is always `/bin/sh` running a
//! stand-in script: nothing here starts yt-dlp or deno, or touches the network.
//!
//! yt-dlp is handed the user's signed-in cookie file, so what steers it matters: its
//! environment, the plugins it loads, and how much of its output the engine will hold.

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ytmfast::auth::{Cookie, Session};
use ytmfast::error::Error;
use ytmfast::streams::ytdlp::{YtDlp, YtDlpCommand};

/// One of these tests at a time. Why: a test writes its stand-in script and then executes it
/// (the system-yt-dlp test runs the script itself, not through `/bin/sh`). If another test
/// forks a child in the gap between the script's write and its exec, that child briefly holds
/// the script's write descriptor, and the exec fails with ETXTBSY ("Text file busy", seen as
/// `StreamFailed("could not run yt-dlp")`). Measured 2026-10-18 with the binary looped under
/// 32 busy loops: 4 failures in 560 runs. The tests are tiny (0.06 s together), so taking turns
/// costs nothing. It also keeps `set_var` away from another test's spawn and keeps the
/// memory-peak test from counting its neighbours.
static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn session() -> Session {
    Session {
        cookies: vec![Cookie {
            domain: ".youtube.com".into(),
            name: "SID".into(),
            value: "fake".into(),
            path: "/".into(),
            secure: true,
            expires_utc: None,
        }],
        account: None,
        browser_stamp: None,
    }
}

/// Runs a stand-in yt-dlp whose script is `body` after it has logged its arguments and its
/// environment's variable names to `log`; returns the answer and that log.
async fn run(body: &str) -> (Result<Vec<u8>, Error>, String) {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("run");
    std::fs::create_dir(&run_dir).unwrap();
    let log = tmp.path().join("log.txt");
    let script = tmp.path().join("fake-yt-dlp.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf 'ARG %s\\n' \"$@\" >> '{log}'\nenv | cut -d= -f1 | sed 's/^/ENV /' >> '{log}'\n{body}\n",
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cmd = YtDlpCommand::new(run_dir).with_program(
        "/bin/sh".into(),
        vec![script.into_os_string()],
        Duration::from_secs(30),
    );
    let out = cmd.info_json("abcDEF_12-x", &session()).await;
    (out, std::fs::read_to_string(&log).unwrap_or_default())
}

fn has_line(log: &str, line: &str) -> bool {
    log.lines().any(|l| l == line)
}

#[tokio::test]
async fn the_child_gets_an_allowlisted_environment_only() {
    let _turn = ONE_AT_A_TIME.lock().await;
    // SAFETY: names no other test reads.
    unsafe {
        std::env::set_var("YTMFAST_TEST_CANARY", "from-the-daemon");
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/test");
        std::env::set_var("NODE_OPTIONS", "--no-warnings");
    }
    let (out, log) = run("echo '{}'").await;
    assert!(out.is_ok(), "{out:?}");
    for leaked in [
        "YTMFAST_TEST_CANARY",
        "DBUS_SESSION_BUS_ADDRESS",
        "NODE_OPTIONS",
    ] {
        assert!(
            !has_line(&log, &format!("ENV {leaked}")),
            "{leaked} reached yt-dlp: the whole environment was inherited"
        );
    }
    // What yt-dlp and its JS runtime need still arrives.
    assert!(
        has_line(&log, "ENV PATH") && has_line(&log, "ENV HOME"),
        "PATH and HOME are kept:\n{log}"
    );
}

/// Runs a stand-in that records its `PATH`, started as the "system" yt-dlp when `system` is
/// true (the stand-in is at the place named as the system copy) or as the fallback when it is
/// not (the system place is empty, the stand-in is run through `with_program`).
async fn path_the_child_sees(system: bool) -> String {
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().join("run");
    std::fs::create_dir(&run_dir).unwrap();
    let log = tmp.path().join("log.txt");
    let script = tmp.path().join("fake-yt-dlp.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s' \"$PATH\" > '{}'\necho '{{}}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cmd = if system {
        YtDlpCommand::for_system_program(run_dir, &script)
    } else {
        YtDlpCommand::for_system_program(run_dir, &tmp.path().join("missing")).with_program(
            "/bin/sh".into(),
            vec![script.into_os_string()],
            Duration::from_secs(30),
        )
    };
    let out = cmd.info_json("abcDEF_12-x", &session()).await;
    assert!(out.is_ok(), "{out:?}");
    std::fs::read_to_string(&log).unwrap()
}

#[tokio::test]
async fn the_system_yt_dlp_gets_a_fixed_path() {
    let _turn = ONE_AT_A_TIME.lock().await;
    // yt-dlp finds its JS runtime (deno) through PATH, and the service's PATH can begin with
    // folders the user's own tools can write to: a deno found there would run with the
    // user's sign-in. The system copy gets only the system folders.
    // SAFETY: only this test file's tests read PATH, and none depends on its value.
    unsafe { std::env::set_var("PATH", "/home/someone/shims:/usr/bin:/bin") };
    assert_eq!(path_the_child_sees(true).await, "/usr/bin:/bin");
}

#[tokio::test]
async fn a_yt_dlp_found_through_path_keeps_the_engines_path() {
    let _turn = ONE_AT_A_TIME.lock().await;
    // It came from PATH, so it (and what it starts) needs that PATH to be found at all.
    // SAFETY: as above.
    unsafe { std::env::set_var("PATH", "/home/someone/shims:/usr/bin:/bin") };
    assert_eq!(
        path_the_child_sees(false).await,
        "/home/someone/shims:/usr/bin:/bin"
    );
}

#[tokio::test]
async fn yt_dlp_is_told_not_to_load_plugins() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (_, log) = run("echo '{}'").await;
    // `--ignore-config` alone does not stop it loading Python plugins from user-writable
    // plugin folders, and it holds the user's sign-in.
    assert!(
        has_line(&log, "ARG --no-plugin-dirs"),
        "arguments were:\n{log}"
    );
    assert!(has_line(&log, "ARG --ignore-config"));
}

fn peak_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
        .unwrap()
}

#[tokio::test]
async fn output_over_the_cap_is_cut_off_while_it_is_read() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let _ = std::fs::write("/proc/self/clear_refs", "5");
    let before = peak_kib();
    // 160 MiB from the stand-in: five times the 32 MiB answer cap.
    let (out, _) = run("head -c 167772160 /dev/zero | tr '\\0' 'a'").await;
    let grew_mib = peak_kib().saturating_sub(before) / 1024;
    assert!(out.is_err(), "an answer over the cap must be refused");
    assert!(
        grew_mib < 64,
        "the engine held about {grew_mib} MiB of the child's output before refusing it"
    );
}

#[tokio::test]
async fn an_answer_under_the_cap_still_comes_back_whole() {
    let _turn = ONE_AT_A_TIME.lock().await;
    let (out, _) = run("printf '{\"id\":\"x\"}'").await;
    assert_eq!(out.unwrap(), b"{\"id\":\"x\"}");
}
