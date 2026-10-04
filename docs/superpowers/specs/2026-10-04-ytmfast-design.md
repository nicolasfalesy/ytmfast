# ytmfast: design

Date: 2026-10-04. Status: approved design, awaiting spec review.

## Goal

A small, headless Rust engine that plays YouTube Music, built to replace the Electron app
(`pear-desktop`) behind the `nic.youtube-music` Omarchy bar widget. Same idea as Spotifast for Spotify:
no browser engine, a fraction of the memory, instant start. The widget stays the only UI.

Success means:

- Every widget feature that works with `pear-desktop` today works with ytmfast (list under "Feature parity").
- Measured against `pear-desktop` on the same machine: less RAM, less CPU while playing, lower power draw,
  faster time from "play" to first sound. Before and after numbers are recorded in the README.
- Nothing runs while no music is playing.

## Decisions (made with the user, 2026-10-04)

| Topic | Decision |
|---|---|
| Shape | Headless engine. No window. The widget is the UI. |
| Account | YouTube Music Premium, signed in. |
| Streams | Own Rust code first; `yt-dlp` as the fallback. |
| Audio | Pure Rust pipeline to PipeWire; libopus for Opus decode. No mpv. |
| Format | Opus 257k (itag 774). AAC 256k (itag 141) when a song has no Opus stream. |
| Loudness | Normalise with YouTube's own per-song loudness value (turn down only, like the official player). |
| History | Report plays to YouTube so history and recommendations stay accurate. |
| Sign-in | Import once from the `pear-desktop` profile; the engine owns that session afterwards, stored in the login keyring. |
| Lifecycle | systemd socket activation; quits after idle minutes. |
| Widget | Uses ytmfast when installed, keeps the `pear-desktop` path otherwise. |
| Cover click | With ytmfast there is no app window, so a cover click opens the playing song's album in the panel. |
| Licence | MIT. Public repo `nicolasfalesy/ytmfast`. |

## Feasibility check (done 2026-10-04, yt-dlp 2026.08.19 + deno 2.9.6)

- Signed out: best audio was Opus 129k (itag 251), via the `visionos` client. Link in 2.3 s.
- Signed in with the `pear-desktop` cookies: itag 141 (AAC 258k) and itag 774 (Opus 257k) came from the
  "tv downgraded" client. Link in 3.7 to 3.9 s with yt-dlp. The first 1 MiB of each downloaded with HTTP 206
  in 0.14 to 0.17 s; `ffprobe` confirmed AAC at 256019 b/s and Opus at 48 kHz.
- Both the signature and the `n` challenge had to be solved by running YouTube's player JavaScript.
  No PO token was needed for the tv client.

## Architecture

One binary, `ytmfast`, built from focused modules. Each module has one job and a small interface, and is
tested on its own.

```
widget (QML) ──unix socket, JSON lines──┐
media keys / desktop ──MPRIS (D-Bus)────┤
                                        ▼
                                    control ──► queue ──► audio ──► PipeWire
                                        │         │         ▲
                                        ▼         ▼         │
                                     innertube ◄── streams ─┘
                                        │         │
                                       auth     solver (QuickJS) / yt-dlp fallback
                                                  │
                                               report (play history)
```

### auth

- `ytmfast import-session` reads the Chromium cookie database of the `pear-desktop` profile
  (`~/.config/YouTube Music/Cookies`), decrypts it (v10 fixed key; v11 key from the Secret Service), and
  keeps only the `youtube.com` and `google.com` cookies that are needed.
- It only runs while `pear-desktop` is closed, because Chromium holds the database open.
- The session is stored encrypted in the user's login keyring (Secret Service, item label "ytmfast session",
  attribute `application=ytmfast`), never in a plain file. A locked or missing keyring gives a clear error. It is never logged, never sent anywhere except Google hosts, and never part of a backup.
- The engine builds the `SAPISIDHASH` authorisation header from the session, and saves every `Set-Cookie`
  rotation it receives, so the session stays valid after `pear-desktop` is removed.
- A rejected session gives the error `signed_out`. The widget then tells the user to sign in again.

### innertube

The YouTube Music API client (`music.youtube.com/youtubei/v1/*`, the `WEB_REMIX` client for browsing).

- `browse` (Home, Library pages, playlists, albums, artists, continuations), `search` (with filters and
  continuations), `next` (watch queue, radio, related, lyrics tab id), `like/like`, `like/removelike`,
  `like/dislike`, lyrics browse (`MPLYt…`).
- It parses the answers into the **same row shapes `Page.js` gives the widget today**, so the widget's list
  code stays the same. Those shapes are written down as Rust types with serde and checked by fixture tests.
- Every answer has a size cap (32 MiB) and a timeout (10 s).

### streams

Turns a video id into a playable audio link.

1. `player` request with the tv client and the session (the client that gave the Premium formats in the
   feasibility check). The client settings live in one table, so a change by YouTube is a one-line fix.
2. Pick itag 774, else 141, else the best other audio.
3. Solve the signature and the `n` challenge with **solver**.
4. Fallback: if any step fails, run `yt-dlp --cookies <temp 0600 file> -f 774/141/bestaudio -g` and use
   its link. The temp cookie file is deleted when yt-dlp exits.
5. Links are cached per video until 30 minutes before their `expire` value.
6. The next song's link is fetched when the current song is 50% through.

Each answer also carries the song's loudness value (`playerConfig.audioConfig.loudnessDb`) and the
play-history URLs for **report**.

### solver

- Runs yt-dlp's own challenge solver scripts (`yt-dlp/ejs`, public domain) in an embedded QuickJS
  (`rquickjs`). The scripts are pinned and bundled into the binary.
- The JS runtime has no file, network or OS access. Memory limit 64 MiB, time limit 5 s per call.
- The player script is downloaded once per player version and kept in `$XDG_CACHE_HOME/ytmfast/players/`
  (the last three versions), and the JS context stays warm while the engine runs.
- Target: a link in under 1 s once the player version is cached (yt-dlp takes about 4 s).

### audio

- **Fetch:** an HTTP range reader (`reqwest`, rustls) downloads the whole track in large bursts. That lets the
  Wi-Fi radio sleep between songs. Cap 32 MiB a track. If the connection drops, it resumes from the last byte.
  If the link expires mid-song (HTTP 403), it fetches a new link and resumes from the same byte.
- **Demux:** `symphonia` (Matroska/WebM for Opus, ISO MP4 for AAC), with seeking through the file's index.
- **Decode:** libopus (the `opus` crate) for Opus; symphonia's pure-Rust decoder for AAC.
- **Loudness:** gain = 10^(−loudnessDb / 20) when loudnessDb > 0, otherwise 1. It only turns songs down,
  never up.
- **Output:** a PipeWire stream (`pipewire` crate), f32 stereo, named "YouTube Music" so mixers and
  per-app rules show it correctly. The volume slider sets the stream's own volume.
- **Gapless:** the next track is decoded into the same stream with no gap. A switch between 48 kHz Opus and
  44.1 kHz AAC renegotiates the stream; that rare case may leave a tiny gap.
- **Position:** played samples minus PipeWire's reported delay. That is accurate enough to drive word-by-word
  lyrics.

### queue

The engine owns the queue (in `pear-desktop` the web page owned it).

- Play a song, an album, a playlist, a search result or a radio. Every queue item has a stable `queueId`, so
  remove and jump stay correct after a shuffle.
- Shuffle, repeat (off, all, one), add next, add to end, remove, jump, move.
- When the queue runs out, it continues with YouTube Music's radio for the last song (from `next`).
- **Resume:** `$XDG_STATE_HOME/ytmfast/state.json` holds the queue source, current song, position, volume,
  shuffle and repeat. It is written on pause, song change, seek and quit, and every 30 s while playing.
  Play after a restart picks up at the saved second. With no saved song, play starts Liked songs (`LM`).

### report

- Sends the same playback and watch-time pings as the official web player (the `playbackTracking` URLs from
  `player`), so plays show in the user's history.
- A failed ping is logged and dropped. It never stops playback.

### control

- **Socket:** `$XDG_RUNTIME_DIR/ytmfast/socket` (folder 0700, socket 0600). The engine also checks
  that the peer has the user's uid.
- **Protocol:** newline-delimited JSON, one message per line, 1 MiB line cap.
  - Request: `{"id": 7, "cmd": "seek", "args": {"seconds": 42.5}}`
  - Reply: `{"id": 7, "ok": true, "data": {...}}` or `{"id": 7, "ok": false, "error": {"code": "...", "message": "..."}}`
  - Event (no id): `{"event": "state" | "position" | "queue" | "error", ...}`
- **Commands:** `status`, `play` (videoId / playlistId / index / startSeconds), `pause`, `toggle`, `next`,
  `previous`, `seek`, `volume`, `mute`, `shuffle`, `repeat`, `like`, `queue.get`, `queue.add`,
  `queue.remove`, `queue.jump`, `queue.move`, `browse`, `search`, `lyrics`, `quit`.
- **Events:** `state` on every change (playing, paused, buffering, stopped, plus song info), `position` once
  a second while playing and on every seek (the widget runs the clock forward between them), `queue` on
  change, `error` with a code (`signed_out`, `unavailable`, `network`, `stream_failed`, `internal`).
- **MPRIS:** `org.mpris.MediaPlayer2.ytmfast` (`zbus`): play, pause, next, previous, seek, metadata with
  cover art URL, volume. Media keys and Omarchy's media display work through it.
- **Lifecycle:** `ytmfast.socket` + `ytmfast.service` (systemd user units). The first connection starts the
  engine. It quits after `idle_minutes` with nothing playing (5 on AC, 2 on battery, both configurable),
  saving state first. The socket unit costs nothing while the engine is stopped.

## Widget changes (`nic.youtube-music`)

- New backend file for ytmfast next to the existing `pear-desktop` code. The widget picks ytmfast when the
  `ytmfast.socket` unit exists. It checks once, lazily, never at plugin import (heavy work at import has
  crashed the shell before).
- The widget connects only when it needs the engine (panel open, play, media key). **It never reconnects
  automatically after the engine quits**, because with socket activation a reconnect would restart the
  engine straight away.
- Cover click opens the playing song's album in the panel (no app window).
- Lyrics keep their sources (KuGou, LRCLIB, then YouTube Music's plain lyrics, now via the `lyrics` command)
  and take their clock from `position` events.
- `pear-desktop` users see no change.

## Feature parity checklist

Bar equalizer and title; left, right and middle click; wheel skips songs; panel transport (shuffle, previous,
play, next, repeat, like, dislike, volume, mute, seek, Left/Right ±10 s); Home and Library (cached
10 minutes); albums, playlists, artists and podcasts opening in place, with Back; long lists loading as you
scroll; search with filters; queue tab (jump, remove by queue id); lyrics (word-by-word, line, plain);
resume at the saved second; play with nothing saved starts Liked songs; one player at a time with World
Radio; media keys; messages as notifications when the panel is closed; idle quit.

## Error handling

- Every failure reaches the widget as an `error` event with a code, and the widget shows its existing message
  or a Try again button. Nothing fails silently.
- If the own-code stream path fails, yt-dlp is tried. If both fail, it skips to the next song after showing
  `stream_failed` for that song.
- A song YouTube marks unplayable gives `unavailable` and is skipped.
- Network loss pauses with `network` and retries with backoff (1, 2, 4, 8, max 30 s) while the user still
  wants playback.
- Logs go to the journal. Session values, links with signatures, and personal data are never logged.

## Security

- The session lives in the login keyring (no plain file); the yt-dlp fallback's temp cookie file is 0600 and deleted on exit; the socket is 0600 with a peer-uid check.
- Network: https only, and only to Google hosts (`*.youtube.com`, `*.googlevideo.com`, `*.google.com`,
  `*.ytimg.com`, `*.ggpht.com`, `*.googleusercontent.com`).
- QuickJS is sandboxed (no I/O) with memory and time limits.
- Every network answer and socket line has a size cap.
- `gitleaks` runs before every push; fixtures are scrubbed of account data.

## Testing

- **Unit (CI):** innertube parsers against scrubbed JSON fixtures; the queue (shuffle, ids, repeat, radio
  hand-off); the protocol (requests, caps, bad input); loudness maths; the idle timer; cookie decryption
  against a generated test database; the range reader against a local test server (drops, 403 then a new
  link).
- **Network tests (manual, not CI):** link resolution through the solver for a known song, checked against
  yt-dlp; yt-dlp fallback.
- **Live checks:** with speakers checked muted first: first sound, pause and resume, seek, gapless across two
  album tracks, link expiry, idle quit, resume after a restart, a play appearing in history, media keys.
- **Widget:** QML tests with a fake engine socket; the nested Hyprland test rig for the panel.
- **Numbers (README):** RAM (PSS), CPU over 5 minutes of playback, power draw on battery over 5 minutes,
  time from play to first sound, and processes while idle. Each for `pear-desktop` (measured first) and for
  ytmfast.
- CI: `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test` on GitHub Actions. Never push red.

## Build order

1. **Plays a song:** auth import, innertube `player`, streams + solver + yt-dlp fallback, audio, a minimal
   control socket (`play`, `pause`, `seek`, `status`), MPRIS. Measure against the `pear-desktop` baseline.
2. **Queue:** queue, radio, resume, loudness, report.
3. **Browsing:** browse, search, like, lyrics.
4. **Widget:** the backend switch, tests, rig check, publish. Then `pear-desktop` comes off the author's
   machine, with a rollback kept.

## Risks

- **YouTube changes.** Clients, the player script, PO tokens. Mitigated by the yt-dlp fallback and by the
  client table and solver scripts being easy to update.
- **Terms of service.** An unofficial client using a signed-in account. History reporting and Premium make it
  look like normal use, but the risk is not zero.
- **Session rotation.** Google rotates session cookies. The engine saves rotations, and an `import-session`
  re-run (or a later sign-in flow) recovers.
