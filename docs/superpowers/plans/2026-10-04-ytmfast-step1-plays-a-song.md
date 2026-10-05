# ytmfast step 1 ("plays a song") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A `ytmfast` daemon that, given a video id over its socket or MPRIS, plays that one song in Premium
Opus through PipeWire, with pause, seek, volume and status. Measured against `pear-desktop`.

**Architecture:** One Rust binary with a library crate (`src/lib.rs`) so every module is unit-tested. Tokio for
I/O; one dedicated audio thread (decode, then PipeWire). The engine is a single task that owns all state and
takes `EngineCmd`s from the socket and MPRIS, and broadcasts `EngineEvent`s back.

**Tech Stack:** Rust 2024 (rustc 1.98), tokio 1.53, reqwest 0.13 (rustls), rusqlite 0.40 (bundled), rquickjs 0.14,
symphonia 0.6 (`mkv`, `isomp4`, `aac`), opus 0.4 (libopus), pipewire 0.10, mpris-server 0.10 (zbus 5),
serde/serde_json, crossbeam-channel, sha1 0.11, pbkdf2 0.13 + aes 0.9 + cbc 0.2, oo7 0.6 (keyring), thiserror 2, log lines as `eprintln!` to stderr (journald captures it under systemd), wiremock 0.6
and tempfile 3 and hyper 1 (tests).

**Spec:** `docs/superpowers/specs/2026-10-04-ytmfast-design.md`. This plan covers its build step 1.
Steps 2 to 4 get their own plans.

## Global Constraints

- MIT licence. Public repo: no personal names, places or home paths in code, comments, fixtures or docs; say "the user".
- Commits use the GitHub noreply address; `gitleaks detect --no-git --source .` must be clean before every push.
- Never push red: `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test` before every push; watch CI after.
- Builds go outside `/tmp` (the build folder is large, and `/tmp` is often in RAM).
- Network: https only, hosts limited to `*.youtube.com`, `*.googlevideo.com`, `*.google.com`, `*.ytimg.com`,
  `*.ggpht.com`, `*.googleusercontent.com` (one allowlist function, used by every request).
- Caps: 32 MiB per API answer and per track; 1 MiB per socket line; API timeout 10 s.
- Session values and signed stream URLs never appear in logs, errors or test output.
- Session: in the login keyring via Secret Service (`oo7` 0.6), label "ytmfast session", attribute `application=ytmfast`; never a plain file.
- State: `$XDG_STATE_HOME/ytmfast/` (0700). Cache: `$XDG_CACHE_HOME/ytmfast/`.
  Socket: `$XDG_RUNTIME_DIR/ytmfast/socket` (folder 0700, socket 0600).
- Format order: itag 774 (Opus), then 141 (AAC), then the highest-bitrate other audio-only format.
- Loudness gain = 10^(−loudnessDb / 20) when loudnessDb > 0, else 1.0.
- Idle quit: 5 minutes on AC, 2 on battery, with nothing playing.
- PipeWire stream properties: `application.name` = "YouTube Music", `node.name` = "ytmfast", `media.role` = "Music".
- MPRIS bus name: `org.mpris.MediaPlayer2.ytmfast`.
- Speakers: before any live play, check `wpctl get-volume @DEFAULT_AUDIO_SINK@` and the hardware sink; test at muted or low volume.

## Review Focus

1. **Two clients at once.** The widget runs one copy per monitor, so two sockets connect. Both must get every event, and commands from either must work (Task 9 test `two_clients_both_get_events`).
2. **Play pressed twice quickly.** A second `play` while the first is still resolving: the later one wins, and only one song ever sounds (Task 8 test `latest_play_wins`).
3. **Seek edge cases.** Seek past the end clamps to end minus 1 s, a negative seek clamps to 0, and a seek beyond the downloaded part waits for the bytes and doesn't hang (Task 6 test `seek_beyond_downloaded_waits`, Task 7 test `seek_clamps`).
4. **Signed out or session missing.** No session file, or YouTube answering LOGIN_REQUIRED, gives the error `signed_out`, never a panic (Task 4 test `login_required_is_signed_out`, Task 8 test `no_session_reports_signed_out`).
5. **No Premium format.** A song with neither 774 nor 141 still plays the best other audio, and an unplayable song gives `unavailable` (Task 5 tests `falls_back_to_best_audio`, `unplayable_is_unavailable`).

---

### Task 1: Project skeleton, paths and CI

**Files:**
- Create: `Cargo.toml`, `src/main.rs`, `src/lib.rs`, `src/paths.rs`, `src/net.rs`, `src/error.rs`, `LICENSE`, `README.md`, `.gitignore`, `.github/workflows/ci.yml`
- Test: `src/paths.rs` (unit), `src/net.rs` (unit)

**Interfaces:**
- Produces: `paths::state_dir() -> io::Result<PathBuf>`, `paths::cache_dir()`, `paths::runtime_dir()`. Each creates the folder with mode 0700 if it's missing and fixes the mode if it's wider. All three honour the XDG variables, falling back to `~/.local/state`, `~/.cache` and `/run/user/$UID`.
- Produces: `net::allowed_host(url: &url::Url) -> bool` (https and the host allowlist); `net::client(user_agent: &str) -> reqwest::Client` (rustls, 10 s timeout, a redirect policy that re-checks `allowed_host`).
- Produces: `error::Error` (thiserror) with variants `SignedOut`, `Unavailable(String)`, `Network(String)`, `StreamFailed(String)`, `Internal(String)`, plus `Error::code(&self) -> &'static str` returning `signed_out|unavailable|network|stream_failed|internal`.

- [ ] **Step 1: Write failing tests**: `paths_created_0700` (temp XDG_STATE_HOME; the folder exists with mode 0o700), `paths_tighten_mode` (a pre-made 0755 folder becomes 0700), `host_allowlist` (true for `https://rr3---sn-x.googlevideo.com/v`, `https://music.youtube.com/`; false for `http://music.youtube.com/`, `https://youtube.com.evil.example/`, `https://evilyoutube.com/`), `error_codes` (each variant maps to its code).
- [ ] **Step 2: Run** `cargo test`. Expected: compile errors / FAIL.
- [ ] **Step 3: Implement** the three modules; `main.rs` parses subcommands `daemon`, `import-session`, `play <videoId>` (debug helper that plays one song to the default sink and exits at the end), `--version`; unknown subcommands print usage and exit 2.
- [ ] **Step 4: Add CI** `.github/workflows/ci.yml` on ubuntu-latest: apt install `libpipewire-0.3-dev libopus-dev libclang-dev pkg-config`, then `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- [ ] **Step 5: Run** the three checks locally. Expected: PASS.
- [ ] **Step 6: Commit** `feat: project skeleton, paths, host allowlist, CI`.

### Task 2: pear-desktop baseline numbers (no code)

**Files:**
- Create: `docs/benchmarks.md`

- [ ] **Step 1:** Write the method into `docs/benchmarks.md`. Same song (a 3 to 4 minute track from the user's library), volume muted at the sink.
  - **RAM:** the sum of `Pss` from `/proc/<pid>/smaps_rollup` over the app's whole process tree, taken 60 s into playback.
  - **CPU:** the change in `utime+stime` from `/proc/<pid>/stat` over 300 s of playback, divided by 300 s × clock ticks, as % of one core.
  - **Power:** the average of `/sys/class/power_supply/BAT*/power_now` sampled every 1 s over 300 s, on battery, screen at a fixed brightness. This needs the user to unplug: ask first.
  - **Time to first sound:** from the play command to the PipeWire stream reaching state `running`, by polling `pw-dump` every 10 ms.
  - **Idle:** the number of processes and the PSS 10 minutes after pausing.
- [ ] **Step 2:** Measure the Electron app (`pear-desktop`) through its bar widget (play and pause from the widget, three runs each) and record median and spread.
- [ ] **Step 3: Commit** `docs: pear-desktop baseline numbers`.

### Task 3: Session import (`auth`)

**Files:**
- Create: `src/auth/mod.rs`, `src/auth/chromium.rs`, `src/auth/sidhash.rs`
- Test: same files (unit), `tests/fixtures/` not used (test DBs are generated in tests)

**Interfaces:**
- Produces: `struct Session { cookies: Vec<Cookie> }`, where `Cookie { domain, name, value, path, secure, expires_utc: Option<i64> }`.
- Produces: `trait SessionStore: Send + Sync { async fn load(&self) -> Result<Session, Error>; async fn save(&self, s: &Session) -> Result<(), Error>; }` with `KeyringStore` (oo7, tokio feature; the session is serialised to JSON as the secret) and `MemoryStore` (tests). `load` gives `SignedOut` when no item exists and `Internal("keyring locked or unavailable")` when the Secret Service can't be reached or unlocked.
- Produces: `Session::cookie_header(&self, url: &Url) -> String`; `Session::apply_set_cookie(&mut self, url: &Url, header: &str) -> bool` (true when something changed, so the caller saves).
- Produces: `sidhash::authorization(session: &Session, origin: &str, now_unix: u64) -> Option<String>`, which follows yt-dlp `_get_sid_authorization_header`: the schemes `SAPISIDHASH` (SAPISID, else `__Secure-3PAPISID`), `SAPISID1PHASH` (`__Secure-1PAPISID`) and `SAPISID3PHASH` (`__Secure-3PAPISID`). Each is `"{scheme} {ts}_{sha1_hex("{ts} {sid} {origin}")}"`, and they are joined by spaces.
- Produces: `chromium::import(profile: &Path) -> Result<Session, Error>`.

- [ ] **Step 1: Write failing tests**:
  - `import_plain_values`: a generated SQLite `Cookies` DB with meta version 24, a plain `value` and an empty `encrypted_value`. Yields the cookie.
  - `import_v10`: encrypted with AES-128-CBC, key = PBKDF2-HMAC-SHA1("peanuts", "saltysalt", 1 iteration, 16 bytes), IV = 16 spaces. For meta version ≥ 24 the plaintext starts with SHA256(host_key) (32 bytes), which must be stripped.
  - `import_v11_is_clear_error`: an Internal error naming v11.
  - `import_filters_hosts`: keeps only `.youtube.com`, `.google.com`, `accounts.google.com`.
  - `sidhash_vector`: SAPISID "abc", origin "https://www.youtube.com", ts 1700000000 gives `SAPISIDHASH 1700000000_` + the sha1 of `"1700000000 abc https://www.youtube.com"`.
  - `memory_store_roundtrip`.
  - `set_cookie_rotation_updates_value`.
  - `missing_item_is_signed_out` (MemoryStore with no session).
- [ ] **Step 2: Run** `cargo test auth`. Expected: FAIL.
- [ ] **Step 3: Implement.** `import` copies the DB to a temp file first (Chromium may hold a lock), opens it read-only, and refuses with a clear message when a `pear-desktop` main process is running (look for `app.asar` with the profile path in `/proc/*/cmdline`). `main.rs import-session [--profile PATH]` defaults to `~/.config/YouTube Music` and prints only the cookie count.
- [ ] **Step 4: Run** `cargo test auth`. Expected: PASS.
- [ ] **Step 5: Live check:** `ytmfast import-session` with pear closed prints `Imported 50 cookies` (or near); `secret-tool search application ytmfast` shows the item (don't print the secret); no session file exists under `~/.local/state/ytmfast/`.
- [ ] **Step 6: Commit** `feat(auth): import the pear-desktop session, SAPISIDHASH`.

### Task 4: InnerTube `player` request (`innertube`)

**Files:**
- Create: `src/innertube/mod.rs`, `src/innertube/clients.rs`, `src/innertube/player.rs`, `tests/fixtures/player_tv_premium.json`, `tests/fixtures/player_login_required.json`, `tests/fixtures/player_unplayable.json`
- Test: `tests/innertube_player.rs` (wiremock)

**Interfaces:**
- Consumes: `Session`, `sidhash::authorization`, `net::client`.
- Produces: `clients::TV: ClientInfo { name: "TVHTML5", version: "5.20260707", name_id: 7, user_agent: "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version", origin: "https://www.youtube.com", api_host: "www.youtube.com" }`. That is the one table to edit when YouTube changes something. Also `clients::WEB_REMIX` for later steps (`"WEB_REMIX"`, name_id 67, origin `https://music.youtube.com`).
- Produces: `Innertube::new(session: Arc<Mutex<Session>>, store: Arc<dyn SessionStore>, base: Url) -> Innertube` (base is overridable for tests; a changed cookie from `Set-Cookie` is saved through `store`) and `Innertube::player(&self, video_id: &str, sts: u32) -> Result<PlayerResponse, Error>`.
- Produces: `PlayerResponse { video_id, title, author, length_seconds: u32, thumbnail: Option<String>, loudness_db: Option<f32>, formats: Vec<AudioFormat>, tracking: Tracking }`, `AudioFormat { itag: u32, mime: String, bitrate: u32, content_length: Option<u64>, url: Option<String>, signature_cipher: Option<String> }` (audio-only formats, i.e. mime starting with `audio/`), `Tracking { playback_url: Option<String>, watchtime_url: Option<String> }`.

- [ ] **Step 1: Write the fixtures by hand** in the shape of a real TV player answer, with made-up ids and URLs (`https://rr1---sn-test.googlevideo.com/videoplayback?...&n=abc`) and nothing from a real account.
- [ ] **Step 2: Write failing tests**:
  - `player_request_shape`: wiremock asserts POST `/youtubei/v1/player?prettyPrint=false`, the headers `X-YouTube-Client-Name: 7`, `X-YouTube-Client-Version`, `Origin`, `X-Origin`, an `Authorization` starting with `SAPISIDHASH `, `Cookie`, `User-Agent`, and the body JSON `context.client.clientName == "TVHTML5"`, `videoId`, `playbackContext.contentPlaybackContext.signatureTimestamp == sts`, `html5Preference == "HTML5_PREF_WANTS"`, `contentCheckOk == true`, `racyCheckOk == true`.
  - `parses_premium_formats`: itags 774 and 141 are present, and `loudness_db` comes from `playerConfig.audioConfig.loudnessDb`.
  - `login_required_is_signed_out`: playabilityStatus LOGIN_REQUIRED gives `Error::SignedOut`.
  - `unplayable_is_unavailable`: UNPLAYABLE gives `Unavailable(reason)`.
  - `oversize_answer_rejected` (33 MiB gives Network).
  - `set_cookie_saved`: a response `Set-Cookie` reaches `apply_set_cookie`.
- [ ] **Step 3: Run** `cargo test --test innertube_player`. Expected: FAIL.
- [ ] **Step 4: Implement.** Read bodies with the 32 MiB cap while streaming, not after.
- [ ] **Step 5: Run** the tests. Expected: PASS.
- [ ] **Step 6: Commit** `feat(innertube): TV player request with session auth`.

### Task 5: Challenge solver and link resolution (`solver`, `streams`)

**Files:**
- Create: `src/solver/mod.rs`, `src/solver/player_js.rs`, `assets/ejs/lib.min.js`, `assets/ejs/core.min.js`, `assets/ejs/LICENSE` (Unlicense), `assets/ejs/VERSION` (`0.8.0`), `src/streams/mod.rs`, `src/streams/ytdlp.rs`
- Test: `src/solver/mod.rs` (unit), `tests/streams.rs`, `tests/network.rs` (`#[ignore]`, run by hand)

**Interfaces:**
- Consumes: `Innertube::player`, `Session`, `net::client`, `paths::cache_dir`.
- Produces: `player_js::current_player_id(client) -> Result<String>`: GET `https://www.youtube.com/iframe_api`, regex `player\\?/([0-9a-fA-F]{8})\\?/`. Also `player_js::url(id) -> String` = `https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js`, and `player_js::sts(code: &str) -> Option<u32>`, regex `(?:signatureTimestamp|sts)\s*:\s*([0-9]{5})`.
- Produces: `enum ChallengeKind { N, Sig }`; `Solver::new(cache_dir) -> Solver`; `Solver::solve(&self, player_id: &str, player_code: Option<&str>, kind: ChallengeKind, challenges: &[String]) -> Result<HashMap<String, String>, Error>`.
- Produces: `trait Resolver { async fn resolve(&self, video_id: &str) -> Result<Stream, Error>; }`, with `Streams` (own code, falling back to yt-dlp) implementing it. `Stream { video_id, url: String, itag: u32, mime: String, content_length: Option<u64>, expires_unix: u64, loudness_db: Option<f32>, meta: TrackMeta, tracking: Tracking }` and `TrackMeta { title, artist, length_seconds, thumbnail }`.
- Produces: `streams::pick_format(formats: &[AudioFormat]) -> Option<&AudioFormat>`.

- [ ] **Step 1: Bundle the scripts.** Copy `lib.min.js` and `core.min.js` from yt-dlp-ejs 0.8.0 (`/usr/lib/python3.14/site-packages/yt_dlp_ejs/yt/solver/`) into `assets/ejs/` with the upstream Unlicense text and the version; include them with `include_str!`.
- [ ] **Step 2: Write failing solver tests**:
  - `sandbox_has_no_io`: `typeof require`, `typeof fetch`, `typeof std`, `typeof os` and `typeof Deno` are all `"undefined"`.
  - `infinite_loop_is_cut`: `while(true){}` returns Err within 6 s.
  - `memory_hog_is_cut`: allocating 200 MiB returns Err.
  - `jsc_protocol`: a stand-in `jsc` that echoes is called with `{type:"player"|"preprocessed", player|preprocessed_player, requests:[{type:"n"|"sig", challenges:[..]}], output_preprocessed:true}`, and the output is parsed from `{type:"result", preprocessed_player?, responses:[{type:"result", data:{challenge: answer}}]}`.
  - `preprocessed_cache_keeps_three`: the cache keeps three players and the oldest file is removed.
- [ ] **Step 3: Implement the solver** with an rquickjs `Runtime`: `set_memory_limit(64 MiB)`, an interrupt handler with a 5 s deadline, a `Context::base` plus only the intrinsics the scripts need (no std/os modules). Run `lib`, then `Object.assign(globalThis, lib)`, then `core`, then `JSON.stringify(jsc(<data>))`. Keep the context warm in the struct. Write the preprocessed player to `cache_dir()/players/{player_id}.js`.
- [ ] **Step 4: Write failing streams tests**:
  - `prefers_opus_then_aac`: 774 over 141 over others.
  - `falls_back_to_best_audio`: no 774/141 means the highest-bitrate audio.
  - `deciphers_signature_cipher`: `s=AB&sp=sig&url=U` with a fake solver mapping AB→XY gives `U&sig=XY`.
  - `replaces_n`: `n=abc` with fake abc→def gives `n=def`.
  - `expiry_from_url`: `expire=` minus 30 min.
  - `uses_ytdlp_when_solver_fails`: a fake solver error means the fake yt-dlp runner is called, and its `-j` JSON (`url`, `format_id`) becomes a Stream with `loudness_db: None`.
  - `ytdlp_cookie_file_removed`: the temp Netscape cookie file exists with mode 0600 during the run and is gone after.
  - `cache_reuses_link_until_expiry`.
- [ ] **Step 5: Implement `Streams`.** Get the player id, then the base.js code (cached on disk by id) and the sts, then `player`, `pick_format`, collect the challenges, solve them in one call per kind, and assemble the URL. On any error from `current_player_id` through the solve, log the error code (never the URL) and run `yt-dlp --no-warnings -q --cookies <file> -f 774/141/bestaudio -j https://music.youtube.com/watch?v=<id>` with a 30 s timeout. `ytdlp.rs` is behind a `trait YtDlp` so tests fake it.
- [ ] **Step 6: Run** `cargo test`. Expected: PASS.
- [ ] **Step 7: Write the network test** `resolves_premium_opus` (`#[ignore]`): resolve the song id from `YTMFAST_TEST_VIDEO` (env var, so no id lives in the repo) with the real session, check itag 774, and check that GET `Range: bytes=0-1023` gives 206. Then `matches_ytdlp`: the same itag and content_length as `yt-dlp -j`. Run it by hand with `YTMFAST_TEST_VIDEO=<id> cargo test --test network -- --ignored`. Expected: PASS, and record the resolve time (the target is under 1 s with a warm cache).
- [ ] **Step 8: Commit** `feat(streams): own link resolution with QuickJS solver, yt-dlp fallback`.

### Task 6: Track download (`audio::fetch`)

**Files:**
- Create: `src/audio/mod.rs`, `src/audio/fetch.rs`
- Test: `tests/fetch.rs` (a local hyper test server)

**Interfaces:**
- Consumes: `Stream`, `net::client`.
- Produces: `TrackBuffer::start(stream: Stream, relink: Box<dyn Fn() -> BoxFuture<'static, Result<String, Error>> + Send + Sync>) -> TrackBuffer`. It starts the background download.
- Produces: `TrackBuffer::reader(&self) -> TrackReader`, where `TrackReader: Read + Seek + Send` blocks until bytes arrive, gives EOF at content_length, and returns an io error when the download failed for good. Also `TrackBuffer::downloaded(&self) -> u64`.

- [ ] **Step 1: Write failing tests** against a test server serving a 3 MiB body:
  - `downloads_whole_track_in_bursts`: Range requests of 10 MiB chunks (one here), and the bytes match.
  - `resumes_after_drop`: the server cuts the connection at 1 MiB, and the next request has `Range: bytes=1048576-`.
  - `relinks_on_403`: the first URL answers 403 at 2 MiB, `relink` is called once, and the download continues from 2 MiB on the new URL.
  - `cap_32_mib`: a 33 MiB content_length gives an Err.
  - `seek_beyond_downloaded_waits`: a slow server, `seek(2 MiB)` then `read`, returns the right bytes once they arrive and never hangs past the 10 s test timeout.
  - `gives_up_after_retries`: 5 drops in a row with backoff 1, 2, 4, 8, 16 s (the test uses a paused tokio clock), then the reader gets an io error.
- [ ] **Step 2: Run** `cargo test --test fetch`. Expected: FAIL.
- [ ] **Step 3: Implement.** A `Vec<u8>` pre-sized to content_length behind a `Mutex`, plus a `Condvar` for waiting readers. Requests for 10 MiB ranges go back to back, which is the burst. The `TrackReader` is used from the audio thread, so it is blocking.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat(audio): burst download with resume and relink`.

### Task 7: Decode and PipeWire output (`audio::decode`, `audio::sink`)

**Files:**
- Create: `src/audio/decode.rs`, `src/audio/sink.rs`, `src/audio/pw.rs`, `src/audio/player.rs`, `tests/fixtures/sine440_48k.webm`, `tests/fixtures/sine440_44k.m4a`
- Test: `tests/decode.rs`, `src/audio/player.rs` (unit)

**Interfaces:**
- Consumes: `TrackReader`, `Stream.loudness_db`.
- Produces: `trait Sink: Send { fn open(&mut self, rate: u32, channels: u16) -> Result<()>; fn write(&mut self, frames: &[f32]) -> Result<()>; fn pause(&mut self, paused: bool); fn flush(&mut self); fn set_volume(&mut self, v: f32); fn delay_frames(&self) -> u64; }`, with `PipeWireSink` (the real one) and `NullSink` (tests and benchmarks; it counts frames).
- Produces: `Decoder::open(reader: TrackReader, mime: &str) -> Result<Decoder>`, `Decoder::next_frames(&mut self) -> Result<Option<&[f32]>>` (interleaved stereo f32), `Decoder::seek(&mut self, seconds: f64) -> Result<f64>` (returns the actual position), and `Decoder::rate()`.
- Produces: `loudness_gain(loudness_db: Option<f32>) -> f32`.
- Produces: `AudioPlayer::spawn(sink: Box<dyn Sink>) -> AudioPlayer`, which runs its own thread. Its methods `load(TrackReader, mime, gain, start_seconds)`, `play()`, `pause()`, `seek(seconds)`, `set_volume(0.0..=1.0)` and `stop()` send messages to that thread. `AudioPlayer::position() -> f64` = (frames written − sink delay) / rate. Events come on `crossbeam_channel::Receiver<AudioEvent>`, where `AudioEvent` is `Started | Paused | Resumed | Ended | Error(String)`.

- [ ] **Step 1: Make the fixtures** with `ffmpeg -f lavfi -i sine=440:duration=2 -c:a libopus -b:a 64k sine440_48k.webm` and the AAC twin at 44.1 kHz. Both are under 40 KB.
- [ ] **Step 2: Write failing tests**:
  - `decodes_opus_webm`: 2 s ±20 ms at 48 kHz, with a dominant frequency of 440 Hz ±5 Hz (from zero crossings).
  - `decodes_aac_m4a`: the same at 44.1 kHz.
  - `gain_math`: None → 1.0, -3.0 → 1.0, 6.0 → 0.501 ±0.001.
  - `seek_clamps`: seek(-5) → 0.0, seek(99) → length − 1.0.
  - `player_position_counts_delay`: NullSink with a fixed delay of 4800 frames, after 96000 frames written, gives position 1.9 s.
  - `pause_stops_writes`.
  - `ended_event_at_eof`.
- [ ] **Step 3: Run** `cargo test --test decode`. Expected: FAIL.
- [ ] **Step 4: Implement.** Use symphonia for demux (mkv, isomp4), the opus crate for Opus packets (48 kHz stereo, f32), and symphonia aac for AAC. Seeking uses symphonia `SeekMode::Accurate`. `PipeWireSink` runs a pw main loop on its own thread with an `F32LE` stereo stream, the Global Constraints properties, and a lock-free ring buffer of 200 ms between threads. `pause` sets the stream inactive. `delay_frames` comes from `pw_stream_get_time_n` delay plus the frames in the ring.
- [ ] **Step 5: Run** the tests. Expected: PASS.
- [ ] **Step 6: Live check:** `ytmfast play <videoId>` (muted sink first) appears in `wpctl status` as "YouTube Music" and plays to the end, and `pw-top` shows the stream running.
- [ ] **Step 7: Commit** `feat(audio): Opus/AAC decode, loudness gain, PipeWire sink`.

### Task 8: Engine

**Files:**
- Create: `src/engine.rs`
- Test: `src/engine.rs` (unit, with fakes)

**Interfaces:**
- Consumes: `Resolver`, `TrackBuffer`, `AudioPlayer`.
- Produces: `enum EngineCmd { Play { video_id: Option<String>, start_seconds: f64 }, Pause, Toggle, Seek(f64), Volume(f32), Status(oneshot::Sender<Status>), Quit }`.
- Produces: `enum EngineEvent { State(Status), Position { seconds: f64 }, Error { code: &'static str, message: String } }`, sent on a `tokio::sync::broadcast` channel with capacity 64.
- Produces: `Status { state: PlayState /* Playing|Paused|Buffering|Stopped */, video_id: Option<String>, meta: Option<TrackMeta>, position: f64, volume: f32 }`.
- Produces: `Engine::new(resolver: Arc<dyn Resolver>, player: AudioPlayer) -> (Engine, mpsc::Sender<EngineCmd>, broadcast::Sender<EngineEvent>)` and `Engine::run(self)`. A `Position` event goes out every 1 s while playing and right after every seek.

- [ ] **Step 1: Write failing tests** with a fake Resolver (configurable delay and result) and NullSink:
  - `play_emits_buffering_then_playing`.
  - `latest_play_wins`: play A (resolve takes 500 ms), then play B at once, gives only B loaded, and A's late result is dropped.
  - `no_session_reports_signed_out`: the resolver returns SignedOut, giving an Error event with code `signed_out` and state Stopped.
  - `toggle_pauses_and_resumes`.
  - `seek_emits_position`.
  - `position_ticks_each_second_only_while_playing` (paused tokio clock).
  - `play_without_id_and_nothing_loaded_is_noop_error` (step 2 adds Liked songs; here it gives `internal` with message "nothing to play").
- [ ] **Step 2: Run** `cargo test engine`. Expected: FAIL.
- [ ] **Step 3: Implement.** Each play gets a generation number, and results from older generations are ignored.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Commit** `feat: engine state machine`.

### Task 9: Control socket, systemd activation, idle quit (`control`)

**Files:**
- Create: `src/control/mod.rs`, `src/control/protocol.rs`, `src/control/idle.rs`, `dist/systemd/ytmfast.socket`, `dist/systemd/ytmfast.service`
- Modify: `src/main.rs` (the `daemon` subcommand)
- Test: `src/control/protocol.rs` (unit), `src/control/idle.rs` (unit), `tests/control.rs`

**Interfaces:**
- Consumes: `EngineCmd`, `EngineEvent`.
- Produces: the wire protocol from the spec. A request is `{"id":u64,"cmd":str,"args":obj?}`, and the M1 commands are `status`, `play {videoId?, startSeconds?}`, `pause`, `toggle`, `seek {seconds}`, `volume {percent 0..=100}` and `quit`. A reply is `{"id","ok":true,"data"}` or `{"id","ok":false,"error":{"code","message"}}`, and an event is `{"event":"state"|"position"|"error",...}` with camelCase fields. The error code for bad input is `bad_request`; an unknown command gives `bad_request` with the message `unknown command`.
- Produces: `idle::IdlePolicy { ac_minutes: 5, battery_minutes: 2 }` and `idle::on_battery() -> bool` (true when no `/sys/class/power_supply/*` of type `Mains` has `online=1`). Also `idle::should_quit(last_active: Instant, now: Instant, playing: bool, on_battery: bool, policy) -> bool`.

- [ ] **Step 1: Write failing tests**:
  - `protocol_roundtrip` for each command.
  - `bad_json_gets_bad_request_reply_and_connection_stays_open`.
  - `line_over_1_mib_closes_connection`.
  - `two_clients_both_get_events`: two sockets, play from the first, and both see `state`.
  - `peer_uid_checked`: a unit test of `fn peer_allowed(peer_uid: u32, my_uid: u32) -> bool`.
  - `idle_quits_after_5_min_on_ac`.
  - `idle_quits_after_2_min_on_battery`.
  - `never_quits_while_playing`.
  - `quit_command_exits_cleanly`.
- [ ] **Step 2: Run** `cargo test control`. Expected: FAIL.
- [ ] **Step 3: Implement.** Take the listening fd from systemd (`LISTEN_FDS`/`LISTEN_PID`, fd 3) when it's set; otherwise bind `runtime_dir()/socket` yourself (removing a stale socket only when nothing answers on it) and chmod it 0600. Check the peer with `UnixStream::peer_cred`. The units are:
  - `ytmfast.socket`: `ListenStream=%t/ytmfast/socket`, `SocketMode=0600`, `DirectoryMode=0700`.
  - `ytmfast.service`: `ExecStart=%h/.local/bin/ytmfast daemon`, `Restart=no`. It has no `[Install]`; the socket has `WantedBy=sockets.target`.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Live check:**
  1. Install with `cargo install --path . --root ~/.local` and copy the units to `~/.config/systemd/user/`.
  2. Run `systemctl --user enable --now ytmfast.socket`, then `printf '{"id":1,"cmd":"status"}\n' | socat - UNIX-CONNECT:$XDG_RUNTIME_DIR/ytmfast/socket`. That starts the service and returns a reply.
  3. After idle minutes, `systemctl --user is-active ytmfast.service` says `inactive`.
- [ ] **Step 6: Commit** `feat(control): socket protocol, systemd activation, idle quit`.

### Task 10: MPRIS (`mpris`)

**Files:**
- Create: `src/mpris.rs`
- Test: `tests/mpris.rs` (a private `dbus-daemon --session --print-address --nofork`, started with a config that has NO service directories)

**Interfaces:**
- Consumes: `EngineCmd` sender and `EngineEvent` receiver.
- Produces: `mpris::serve(cmds: mpsc::Sender<EngineCmd>, events: broadcast::Receiver<EngineEvent>) -> Result<()>`. It registers `org.mpris.MediaPlayer2.ytmfast` with Identity "YouTube Music", `CanPlay/CanPause/CanSeek` true, and `CanGoNext/CanGoPrevious` false (they come in step 2). Metadata carries `mpris:trackid` = `/org/ytmfast/track/<sanitised id>`, `xesam:title`, `xesam:artist`, `mpris:length` (µs) and `mpris:artUrl`.

- [ ] **Step 1: Write failing tests** on the private bus:
  - `playpause_sends_toggle`.
  - `seek_sends_seek_relative`: Seek(+10 s) at position 5 gives `Seek(15.0)`.
  - `metadata_follows_state_event`.
  - `volume_set_sends_volume`.
- [ ] **Step 2: Run** `cargo test --test mpris`. Expected: FAIL.
- [ ] **Step 3: Implement** with `mpris-server` (`Player` builder, or `LocalServer` + trait); start it from the daemon next to the socket.
- [ ] **Step 4: Run.** Expected: PASS.
- [ ] **Step 5: Live check:** `playerctl -p ytmfast play-pause` toggles, and `playerctl -p ytmfast metadata` shows the title.
- [ ] **Step 6: Commit** `feat: MPRIS player`.

### Task 11: Measure, document, publish step 1

**Files:**
- Modify: `docs/benchmarks.md`, `README.md`

- [ ] **Step 1:** Run every live check from Tasks 3, 5, 7, 9 and 10 in one go, on a fresh daemon started by socket activation.
- [ ] **Step 2:** Measure ytmfast with the Task 2 method: same song, three runs, median and spread. Add a resolve-time row (own code with a cold cache, with a warm cache, and yt-dlp fallback). Add a row for the `NullSink` decode cost (CPU % of one core).
- [ ] **Step 3:** Put a before/after table in `README.md`, with install steps (build, `cargo install`, units, `ytmfast import-session`) and the step 2 to 4 roadmap.
- [ ] **Step 4:** Run `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`, then `gitleaks detect --no-git --source .`. All must be clean.
- [ ] **Step 5: Commit** `docs: step 1 numbers`, push, and watch the CI run to green.
