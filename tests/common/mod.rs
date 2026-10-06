//! A private PipeWire daemon for the ignored output tests, shared by the test binaries.
//!
//! The daemon runs from a config written here: a null sink and the modules a client stream
//! needs, nothing that opens a sound card, no session manager and no D-Bus. Its socket is in
//! a temp folder, and the test binary's `PIPEWIRE_RUNTIME_DIR` points there, so the user's
//! own PipeWire is never reached. Without a session manager nothing links the stream, so the
//! tests link it to the null sink themselves (`pw-cli`, `pw-link`).

// Each test binary uses its own part of this.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The private daemon: a 48 kHz null sink (`test-sink`) driving the graph, and the protocol,
/// client-node and adapter modules a client stream needs. No ALSA, no D-Bus, no RT module.
pub const CONFIG: &str = r#"
context.properties = {
    core.daemon = true
    core.name   = pipewire-0
    default.clock.rate = 48000
}
context.spa-libs = {
    audio.convert.* = audioconvert/libspa-audioconvert
    audio.adapt     = audioconvert/libspa-audioconvert
    support.*       = support/libspa-support
}
context.modules = [
    { name = libpipewire-module-protocol-native }
    { name = libpipewire-module-metadata }
    { name = libpipewire-module-spa-node-factory }
    { name = libpipewire-module-client-node }
    { name = libpipewire-module-access args = { } }
    { name = libpipewire-module-adapter }
    { name = libpipewire-module-link-factory }
]
context.objects = [
    { factory = metadata args = { metadata.name = default } }
    { factory = spa-node-factory
        args = { factory.name = support.node.driver node.name = Dummy-Driver
                 node.group = pipewire.dummy priority.driver = 20000 } }
    { factory = adapter
        args = { factory.name = support.null-audio-sink node.name = test-sink
                 media.class = Audio/Sink audio.position = [ FL FR ] audio.rate = 48000 } }
]
"#;

/// One private daemon run. Dropping it kills the daemon.
pub struct Daemon {
    child: Child,
}

impl Daemon {
    pub fn start(dir: &Path) -> Daemon {
        let socket = dir.join("pipewire-0");
        let _ = std::fs::remove_file(&socket);
        let config = dir.join("test.conf");
        std::fs::write(&config, CONFIG).unwrap();
        let child = Command::new("pipewire")
            .arg("-c")
            .arg(&config)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("PIPEWIRE_RUNTIME_DIR", dir)
            .env("XDG_RUNTIME_DIR", dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            // A bus address that leads nowhere: the daemon must not reach the user's bus.
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}", dir.join("nobus").display()),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the pipewire program (this test is run by hand: see the file's top)");
        let until = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                Instant::now() < until,
                "the private daemon never made its socket"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        // The null sink has no ports until it is told its layout (a session manager's job).
        let configured = pw_tool(
            dir,
            "pw-cli",
            &["s", "test-sink", "PortConfig", &port_config("Input")],
        );
        assert!(configured.is_some(), "could not set up the null sink");
        Daemon { child }
    }

    /// Stops it the way `systemctl restart` does: SIGTERM, then wait.
    pub fn stop(mut self) {
        // SAFETY: kill(2) on our own child's pid; it can't touch any other process while we
        // haven't reaped it yet.
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `pw-cli` or `pw-link` against the private daemon only.
pub fn pw_tool(dir: &Path, program: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(program)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("PIPEWIRE_RUNTIME_DIR", dir)
        .env("XDG_RUNTIME_DIR", dir)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Stereo F32 ports at 48 kHz, one per channel, on a node's `direction` side.
fn port_config(direction: &str) -> String {
    format!(
        r#"{{ "direction": "{direction}", "mode": "dsp", "format": {{ "mediaType": "audio",
             "mediaSubtype": "raw", "format": "F32P", "rate": 48000, "channels": 2,
             "position": [ "FL", "FR" ] }} }}"#
    )
}

/// The id of the newest node called `ytmfast`, from `pw-cli ls Node`.
pub fn ytmfast_node(dir: &Path) -> Option<u32> {
    let listing = pw_tool(dir, "pw-cli", &["ls", "Node"])?;
    let mut id = None;
    let mut found = None;
    for line in listing.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id ") {
            id = rest.split(',').next().and_then(|n| n.trim().parse().ok());
        } else if line == "node.name = \"ytmfast\"" {
            found = id;
        }
    }
    found
}

/// Links the stream to the null sink, as a session manager would.
pub fn link(dir: &Path) -> u32 {
    let until = Instant::now() + Duration::from_secs(5);
    let node = loop {
        if let Some(n) = ytmfast_node(dir) {
            break n;
        }
        assert!(
            Instant::now() < until,
            "the stream never showed up in the daemon"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    pw_tool(
        dir,
        "pw-cli",
        &["s", &node.to_string(), "PortConfig", &port_config("Output")],
    );
    for channel in ["FL", "FR"] {
        let from = format!("ytmfast:output_{channel}");
        let to = format!("test-sink:playback_{channel}");
        let until = Instant::now() + Duration::from_secs(5);
        while pw_tool(dir, "pw-link", &[&from, &to]).is_none() {
            assert!(Instant::now() < until, "could not link {from}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    node
}

/// The stream's first channel volume, from the node's `Props` (`pw-cli enum-params`).
pub fn volume(dir: &Path, node: u32) -> Option<f32> {
    let props = pw_tool(dir, "pw-cli", &["e", &node.to_string(), "Props"])?;
    // `Prop: key Spa:Pod:Object:Param:Props:channelVolumes`, then an array of `Float x`.
    let at = props.find("Props:channelVolumes")?;
    props[at..]
        .lines()
        .find_map(|l| l.trim().strip_prefix("Float "))
        .and_then(|v| v.trim().parse().ok())
}

/// Sets the stream's channel volumes the way a mixer does (a `Props` param on its node).
pub fn set_volume(dir: &Path, node: u32, v: f32) {
    let props = format!("{{ \"channelVolumes\": [ {v}, {v} ] }}");
    assert!(
        pw_tool(dir, "pw-cli", &["s", &node.to_string(), "Props", &props]).is_some(),
        "could not set the stream's volume"
    );
}

/// Waits up to 5 s for the stream's volume to be `want`.
pub fn wait_for_volume(dir: &Path, node: u32, want: f32) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let now = volume(dir, node);
        if now.is_some_and(|v| (v - want).abs() < 1e-4) {
            return;
        }
        assert!(
            Instant::now() < until,
            "the volume stayed {now:?}, not {want}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Points this process's PipeWire clients at the private daemon.
pub fn use_private_daemon(dir: &Path) -> PathBuf {
    // SAFETY: only called by a test that runs alone (the one ignored test in its binary, run
    // with `--ignored`), before it starts any thread that could read the environment (the
    // player and PipeWire threads come after).
    unsafe {
        std::env::remove_var("PIPEWIRE_REMOTE");
        std::env::set_var("PIPEWIRE_RUNTIME_DIR", dir);
        std::env::set_var("XDG_RUNTIME_DIR", dir);
    }
    dir.to_path_buf()
}
