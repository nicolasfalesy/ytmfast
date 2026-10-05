# ytmfast step 2 ("the queue") Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ytmfast plays whole albums, playlists and radios back to back with no gaps. It continues with radio when
the queue runs out, resumes at the saved second after a restart, counts plays in the user's YouTube history, and
shows real song details (title, artist, album, album art).

**Architecture:** A pure `queue` module (no I/O) owns order, shuffle, repeat and stable queue ids. `innertube::next`
fills it from YouTube Music's `next` endpoint (WEB_REMIX client on music.youtube.com). The engine drives the queue:
it prefetches the next song's link at 50%, hands the next decoded track to the audio thread before the current one
ends (gapless), saves `state.json`, and sends play reports. The socket and MPRIS gain queue commands.

**Tech Stack:** as step 1. No new crates unless a task says so.

**Spec:** `docs/superpowers/specs/2026-10-04-ytmfast-design.md` (the queue, report and resume sections, build
step 2). Step 1's decisions: `docs/superpowers/plans/2026-10-05-ytmfast-step1-rulings.md`, whose "Parked for step 2"
list this plan closes.

## Global Constraints

- Everything in step 1's Global Constraints still holds (MIT, public-repo wording, noreply commits, gitleaks, never
  push red, https + host allowlist, size caps, no URLs or cookies in errors/logs/events, keyring-only session).
- `next` and `browse` use the WEB_REMIX client: `clientName "WEB_REMIX"`, the version in `clients::WEB_REMIX`, origin
  and API host `https://music.youtube.com`, `X-YouTube-Client-Name: 67`. Stream links still use the TV client.
- Queue ids are `u64`, start at 1, never reused within a daemon's life.
- Previous: restarts the song when the position is more than 3 s in, else goes to the previous item.
- Repeat modes on the wire: `"off" | "all" | "one"`.
- Prefetch the next song's link at 50% of the current song; preload (decode-ready) the next track 10 s before the
  end, or at once when the current song has under 10 s left.
- Gap budget: at most 5 ms of silence inserted between two tracks of the same sample rate.
- `state.json` lives in `state_dir()`, mode 0600, written atomically (temp file + rename). Written on pause, song
  change, seek, queue change and quit, and every 30 s while playing. It never holds a cookie or a URL.
- Resume never auto-plays: after a restart the engine loads the saved song paused at the saved second.
- With nothing saved, `play` with no id starts Liked songs (playlist `LM`).
- Play reports: one playback ping when a song really starts, watch-time pings at 10 s, 20 s, 30 s of play and then
  every 40 s, plus one on pause, seek and end. A failed ping is logged by code only and never stops playback.
- Live checks with sound use the user's volume as found and stay short; long checks use the engine at volume 0.

## Review Focus

1. **Shuffle then remove or jump by queue id.** The right song goes, whatever the order (Task 3 test `remove_by_id_after_shuffle`).
2. **The queue runs out mid-radio fetch.** The last song ends while the radio request is still out: the next radio song plays as soon as it arrives, and nothing plays twice (Task 4 test `end_of_queue_waits_for_radio`).
3. **A song in the queue is unplayable.** It is skipped with an `unavailable` event and the next one plays, with no infinite skip loop when every song fails (Task 4 test `all_unplayable_stops_after_one_pass`).
4. **Restart during a song.** After `systemctl --user restart`, the saved song comes back paused within 30 s of where it was (Task 6 test `restart_resumes_paused_near_position`).
5. **Gapless across a sample-rate change.** Opus (48 kHz) then AAC (44.1 kHz): no crash, the second song starts, and any gap is counted and reported (Task 5 test `rate_change_renegotiates`).

---

### Task 1: Capture real answer shapes (controller, no code)

**Files:**
- Create: `tests/fixtures/next_album.json`, `tests/fixtures/next_radio.json`, `tests/fixtures/next_radio_continuation.json`, `tests/fixtures/next_liked.json`

- [ ] **Step 1:** With the user's session, POST WEB_REMIX `next` for: one album track with its album playlist id; one
  song with `RDAMVM<videoId>` (radio); that radio's continuation; and `playlistId: "LM"`.
- [ ] **Step 2:** Scrub each answer: replace every video id, playlist id, channel id, continuation token, URL and
  name with made-up values of the same shape, and delete anything that identifies the account. Keep the structure
  (`singleColumnMusicWatchNextResultsRenderer → … → musicQueueRenderer → playlistPanelRenderer`, the
  `playlistPanelVideoRenderer` and `playlistPanelVideoWrapperRenderer` items, `nextRadioContinuationData`).
- [ ] **Step 3:** `gitleaks detect --no-git --source .` clean, and a grep for the real ids returns nothing. Commit
  `test: scrubbed next-answer fixtures`.

### Task 2: `innertube::next` and song details

**Files:**
- Create: `src/innertube/next.rs`
- Modify: `src/innertube/mod.rs` (use `ClientInfo.api_host`; a second base URL for music.youtube.com), `src/innertube/clients.rs`
- Test: `tests/innertube_next.rs`

**Interfaces:**
- Produces: `Innertube::next(&self, req: NextRequest) -> Result<NextPage, Error>`, where `NextRequest { video_id: Option<String>, playlist_id: Option<String>, index: Option<u32>, params: Option<String>, continuation: Option<String> }`.
- Produces: `NextPage { items: Vec<SongItem>, continuation: Option<String>, playlist_id: Option<String> }`, and `SongItem { video_id, title, artists: Vec<String>, album: Option<String>, thumbnail: Option<String>, length_seconds: u32, playlist_id: Option<String> }`.
- Produces: `fn clean_artist(name: &str) -> String`, which strips a trailing `" - Topic"`.

- [ ] **Step 1: Failing tests** against the Task 1 fixtures:
  - `parses_album_queue`: count, order, titles, artists, album and length.
  - `parses_radio_with_continuation`.
  - `parses_wrapper_items` (`playlistPanelVideoWrapperRenderer`).
  - `continuation_request_shape`: wiremock asserts WEB_REMIX headers, `music.youtube.com`, and the body `continuation`.
  - `thumbnail_must_pass_allowlist`.
  - `topic_suffix_stripped`.
  - `api_host_is_used`: the production URL comes from `ClientInfo.api_host`.
- [ ] **Step 2: Run** `cargo test --test innertube_next`. Expected: FAIL.
- [ ] **Step 3: Implement.** Use the largest thumbnail. Skip items without a video id; an unavailable item stays in the list with no length.
- [ ] **Step 4: Run.** Expected: PASS. **Commit** `feat(innertube): next queue pages and song details`.

### Task 3: Queue model (pure)

**Files:**
- Create: `src/queue.rs`
- Test: `src/queue.rs` (unit)

**Interfaces:**
- Produces: `QueueItem { id: u64, song: SongItem }` and `enum Repeat { Off, All, One }`.
- Produces: `Queue::new() -> Queue` and `replace(songs, start_index) -> Option<&QueueItem>`.
- Produces: `current()`, `next(auto: bool) -> Option<&QueueItem>` (auto = the song ended, so Repeat::One repeats; a skip ignores One), and `previous(position: f64) -> Previous { Restart | Item(&QueueItem) }`.
- Produces: `jump(id)`, `remove(id) -> bool`, `add(songs, at: AddAt { Next | End })`, `move_to(id, index)`, `set_shuffle(bool)`, `set_repeat(Repeat)`, `items()`.
- Produces: `needs_more(&self) -> bool` (true when 2 or fewer items remain after the current one, with Repeat::Off), and `append_radio(songs)`.

- [ ] **Step 1: Failing tests:**
  - `ids_are_stable_and_unique`.
  - `remove_by_id_after_shuffle` (Review Focus 1).
  - `shuffle_keeps_current_first_and_unshuffle_restores_order`.
  - `repeat_all_wraps`.
  - `repeat_one_repeats_on_auto_only`.
  - `previous_restarts_after_3_s`.
  - `add_next_and_end`.
  - `move_to_reorders`.
  - `needs_more_near_end`.
  - `remove_current_moves_to_next`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** Shuffle uses a seeded RNG injected for tests. **Step 4: Run.** PASS. **Commit** `feat: queue model`.

### Task 4: Engine drives the queue

**Files:**
- Modify: `src/engine.rs`
- Test: `src/engine.rs` (unit, with a fake Resolver and a fake `next` source)

**Interfaces:**
- Consumes: `Queue`, `Innertube::next` (behind `trait QueueSource { async fn next(&self, NextRequest) -> Result<NextPage, Error>; }` so tests fake it).
- Produces: new commands `EngineCmd::Play { video_id, playlist_id, index, start_seconds }` (replaces the step 1 variant), `Next`, `Previous`, `QueueGet(oneshot)`, `QueueAdd { songs, at }`, `QueueRemove(u64)`, `QueueJump(u64)`, `QueueMove { id, index }`, `Shuffle(bool)`, `Repeat(Repeat)`.
- Produces: new events `EngineEvent::Queue { items, current_id, shuffle, repeat }` and `Position { seconds, seeked: bool }`.
- Produces: `Status` gains `album`, `queue_id`, `shuffle` and `repeat`.

- [ ] **Step 1: Failing tests:**
  - `play_playlist_fills_queue_and_plays_index`.
  - `ended_plays_next`.
  - `end_of_queue_waits_for_radio` (Review Focus 2).
  - `needs_more_fetches_radio_once`.
  - `unavailable_song_is_skipped`.
  - `all_unplayable_stops_after_one_pass` (Review Focus 3).
  - `next_link_prefetched_at_half`.
  - `seek_past_end_acts_like_next`.
  - `meta_comes_from_queue_item_not_oembed`.
  - `play_without_anything_starts_liked_songs`.
  - `queue_event_on_every_change`.
  - `seek_sets_seeked_flag`.
  - `position_kept_after_mid_song_error`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** Generation numbers stay the single way to drop stale results. **Step 4: Run.** PASS. **Commit** `feat: engine plays through the queue`.

### Task 5: Gapless handover in the audio thread

**Files:**
- Modify: `src/audio/player.rs`, `src/audio/pw.rs`
- Test: `tests/gapless.rs`, plus fixtures `tests/fixtures/tone_a_48k.webm`, `tone_b_48k.webm` and `tone_c_44k.m4a`

**Interfaces:**
- Produces: `AudioPlayer::preload(reader, mime, gain, length_hint)` (next track, decoded only once the current one's buffer is full) and `AudioEvent::Advanced` (the preloaded track became current, at an exact frame).
- Changes: the PipeWire volume is applied once on change and never re-applied on state changes (closes step 1 parked Minor 5).

- [ ] **Step 1: Failing tests:**
  - `same_rate_handover_is_gapless`: NullSink frame accounting, a gap of 5 ms or less.
  - `rate_change_renegotiates` (Review Focus 5).
  - `stop_drops_preload`.
  - `preload_replaced_by_newer_preload`.
  - `volume_not_reapplied_on_pause` (an ignored private-PipeWire test is allowed).
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** **Step 4: Run.** PASS. **Commit** `feat(audio): gapless handover`.

### Task 6: Resume state

**Files:**
- Create: `src/state.rs`
- Modify: `src/engine.rs`, `src/main.rs`
- Test: `src/state.rs` (unit), `tests/resume.rs`

**Interfaces:**
- Produces: `Saved { queue: Vec<SongItem>, current_index: usize, position: f64, volume: f32, shuffle: bool, repeat: Repeat, source_playlist: Option<String>, saved_unix: u64 }`, `state::load(dir) -> Option<Saved>` and `state::save(dir, &Saved) -> io::Result<()>`.

- [ ] **Step 1: Failing tests:**
  - `save_is_atomic_and_0600`.
  - `corrupt_file_is_ignored`.
  - `restart_resumes_paused_near_position` (Review Focus 4, a fake clock and a fake Resolver).
  - `writes_every_30_s_only_while_playing`.
  - `no_url_or_cookie_in_state`.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** Cap the saved queue at 500 items around the current one. **Step 4: Run.** PASS. **Commit** `feat: resume state`.

### Task 7: Play reports (history)

**Files:**
- Create: `src/report.rs`
- Modify: `src/engine.rs`
- Test: `tests/report.rs` (wiremock)

**Interfaces:**
- Produces: `Reporter::start(tracking: Tracking, cpn: String) -> PlayReport`, `PlayReport::tick(played_seconds)`, `pause(at)`, `seek(from, to)` and `end(at)`.

- [ ] **Step 1: Spike (controller):** play one song with the engine at volume 0, sending the pings the way the
  official web player does (the `playbackTracking` URLs plus the documented `cpn`, `ver=2`, `cmt`, `st`, `et`, `len`
  and `rtn` parameters). Then check YouTube Music history (WEB_REMIX browse `FEmusic_history`) for the song. Record
  in the ledger which URLs and parameters made it appear. If the TV client's tracking URLs don't count for YouTube
  Music history, use the WEB_REMIX `player` answer's tracking URLs (one extra small request per song).
- [ ] **Step 2: Failing tests:**
  - `playback_ping_once_at_start`.
  - `watchtime_cadence_10_20_30_then_40`.
  - `pause_and_seek_send_ranges`.
  - `failed_ping_never_stops_playback`.
  - `ping_urls_pass_allowlist`.
  - `no_ping_for_a_song_that_never_started`.
- [ ] **Step 3: Run.** FAIL. **Step 4: Implement** the recipe from the spike. **Step 5: Run.** PASS. **Commit** `feat: play reports`.

### Task 8: Socket and MPRIS gain the queue

**Files:**
- Modify: `src/control/protocol.rs`, `src/control/mod.rs`, `src/mpris.rs`, `docs/protocol.md`
- Test: `tests/control.rs`, `tests/mpris.rs`

- [ ] **Step 1: Failing tests:**
  - socket: `next`, `previous`, `queue.get`, `queue.add`, `queue.remove`, `queue.jump`, `queue.move`, `shuffle` and `repeat` round trips; `queue` event to both clients.
  - MPRIS: `CanGoNext`/`CanGoPrevious` follow the queue; `Next`/`Previous` send the commands; the `Shuffle` and `LoopStatus` properties ("None" | "Track" | "Playlist") map to `shuffle` and `repeat`; a socket seek emits `Seeked`; `xesam:album` and `mpris:artUrl` come from the queue item.
- [ ] **Step 2: Run.** FAIL. **Step 3: Implement.** **Step 4: Run.** PASS. **Commit** `feat: queue over the socket and MPRIS`.

### Task 9: Step 1 parked fixes

**Files:**
- Modify: `src/streams/mod.rs`, `src/innertube/player.rs`, `src/auth/mod.rs`, `docs/superpowers/specs/2026-10-04-ytmfast-design.md`

- [ ] **Step 1: Failing tests:**
  - `different_video_answer_falls_back_to_ytdlp`.
  - `no_audio_format_falls_back_to_ytdlp`.
  - `tv_only_refusal_falls_back_to_ytdlp`.
  - `signed_out_still_never_falls_back`.
  - `bot_check_is_not_signed_out`: the "confirm you're not a bot" reason gives `stream_failed`, so yt-dlp tries.
- [ ] **Step 2: Implement:** map those cases to StreamFailed (revises R26). Fix the stale "loads happen once at start" comment. Make the spec's test-rig and "author's machine" lines neutral.
- [ ] **Step 3: Run.** PASS. **Commit** `fix: yt-dlp rescues more own-code failures`.

### Task 10: Live checks, numbers, publish step 2 (controller)

- [ ] **Step 1:** Live, engine at volume 0 except where a short audible check is named:
  - an album plays three songs through with gapless handover (check the handover frame counts in the log);
  - a radio continues past the queue end;
  - shuffle, then remove by id;
  - `systemctl --user restart ytmfast.service` mid-song resumes paused near the position;
  - the song appears in YouTube Music history;
  - media keys next and previous;
  - a short audible check at the user's volume that the handover between two album songs has no click.
- [ ] **Step 2:** Re-measure RAM, CPU and time to first sound with the step 1 method; add a step 2 column to `docs/benchmarks.md`.
- [ ] **Step 3:** Checks green, gitleaks clean, push, CI green.
