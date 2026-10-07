# ytmfast step 4: the bar widget on ytmfast — design

Date: 2026-10-06. Status: written for the user's review.

The YouTube Music bar widget (`nic.youtube-music`) moves fully onto ytmfast. This replaces the step 4 lines of
the main spec (`2026-10-04-ytmfast-design.md`, "Widget changes" and "Build order" 4), because the user changed
the goal on 2026-10-06:

- **ytmfast only.** The widget drops pear-desktop support. The widget is now the user's own, not a public
  plugin with other users to keep working. The public plugin repo stays frozen at 2.4.0 (the pear version) with
  one README line saying it is no longer updated; the new widget is kept privately with the user's desktop config.
- **It looks exactly the same.** Every screen, size, colour, animation and key stays as it is.
- **As fast and light as it can be.** Rewriting is fine where that is the simplest way there.

## What the user picked (2026-10-06)

| Choice | Pick |
|---|---|
| Code | Fresh logic for ytmfast in a few small files; the visual QML copied over as it is and proved the same with before-and-after screenshots |
| Seeing the engine | Watch ytmfast's MPRIS entry: connect when it appears, let go when it goes |
| Sign-in | ytmfast learns to copy the session from Brave Origin; the panel gets a Sign in button for it |
| Lyrics | KuGou and LRCLIB move into the engine; the widget gets ready lines and words |
| pear-desktop | Removed from the user's laptop as soon as the live check passes |
| pear leftovers | Profile (Google cookies), bridge, token, window rule all removed; a rollback copy is kept 14 days, then deleted |
| Public plugin repo | Frozen with a note |

## Part A: engine changes (ytmfast)

### A1. Timed lyrics

`lyrics` today gives YouTube Music's plain text. It becomes the widget's whole lyrics chain, ported from the
widget's JavaScript (`loadLyrics`, `kugouLookup`, `krcText`, `inflate`, `parseKrc`, `polishKrc`, `lrclibLookup`,
`parseLrc`, `plainLines`, `sameName`), with the same rules:

1. **KuGou word timing** and **LRCLIB** asked at the same time.
   - KuGou: `krcs.kugou.com/search` with "first artist - title" and the length in ms; a candidate must match the
     title and the artist (`sameName`: case, accents and punctuation ignored, substring allowed) and be within
     3 s of the length; then `lyrics.kugou.com/download?fmt=krc`, base64, skip 4 bytes, XOR with the 16-byte key,
     zlib. `parseKrc` drops credit lines, the "Artist - Title" line and KuGou notices, and gives nothing for
     evenly spread fake timing. `polishKrc` takes LRCLIB's spelling (whole line, a run inside a longer line, or
     3 in 4 words alike) and gives nothing when whole-line matches disagree by more than 1.5 s (another version
     of the song).
   - LRCLIB: `/api/get` (artist, title, album, length), else `/api/search` with the cleaned title and first
     artist, closest length within 3 s that has timed lines. `Lrclib-Client` header names ytmfast.
2. Order of use: KuGou words, then LRCLIB lines, then LRCLIB plain, then YouTube Music's plain lyrics (today's
   code).
3. Reply:

```json
{"source": "KuGou", "synced": true, "words": true,
 "lines": [{"t": 12.34, "text": "A line", "words": [{"t": 12.34, "d": 0.41, "text": "A "}]}]}
```

   `synced: false` lines have no `t`; `words` only when `words: true`. `{"none": true}` when nothing has them.
   CJK characters are each their own word unit; Hangul groups by spaces (as the widget does today).
4. **Network:** the https-only rule stays. Three hosts join the allowlist, for lyrics only: `lrclib.net`,
   `krcs.kugou.com`, `lyrics.kugou.com`. Each answer has a size cap (2 MiB, as the widget's curl had), an 8 s
   timeout, and the zlib output a cap (1 MiB, the widget's inflate-bomb cap).
5. **Cache:** the last 20 answers as today; "none" re-asked after an hour; failures never kept.
6. **Golden test:** the widget's current JavaScript is run (deno) on saved KuGou and LRCLIB answers, and the Rust
   output must match it field by field. Live check on 8 songs including Japanese and Korean.

### A2. Sign-in from Brave Origin

- `ytmfast import-session --browser brave-origin` reads `~/.config/BraveSoftware/Brave-Origin/Default/Cookies`
  (opened read-only, `immutable=1`, so it works while Brave runs), decrypting with the "Brave Safe Storage"
  keyring secret. `--profile PATH` keeps working for pear-desktop's profile.
- Before saving, it checks the session works and prints the account's name, so a wrong Google account is seen
  at once.
- Only the cookies ytmfast needs are copied. The browser is never written to.
- Risk, not checked yet: Google rotates session cookies. With the engine and the browser holding copies of one
  session, a rotation on one side could sign the other out. The live check watches for that over the 3 days
  after; if it happens, the answer is a separate sign-in for ytmfast (a later step).

### A3. Small ones

- **"Artist - Topic":** the engine strips the " - Topic" suffix from a song's artist when it comes from the
  player answer (the ~1 s before the queue's details land after a list starts). This fixes MPRIS and the
  desktop's media display too, so it is done in the engine rather than the widget (the user's step 3 pick was
  the widget; same result, one place).
- **`queue.add` takes `albumId`** per song, so the cover click works for songs added to the queue.
- **Queue events only to clients that want them:** a new `watch` command, `{"queue": false}`, stops `queue`
  events to that client (on by default, so the protocol stays as it is). The widget turns them on only while the
  panel is open; the bar needs only `state` and `position`. A 1,000-song queue is about 400 KB of JSON per
  change, parsed in the shell.

## Part B: the widget (ytmfast only)

### Files

| File | Job |
|---|---|
| `manifest.json` | `bar-widget` + `service` entry points (as `nic.bitwarden` does) |
| `Service.qml` | Once per shell: the engine link, player state, the queue, the browse cache, lyrics state, last-song memory, notifications, the World Radio rule, the IPC target |
| `Widget.qml` | Once per monitor: the bar item and the panel, a view of the service. Panel QML copied from today's file |
| `LyricsView.qml` | The lyrics list, word fill and break dots, copied from today's file |
| `Rows.js` | Small pure helpers (row keys, time format, art host check), shared and unit-tested |

Gone: `Page.js`, `tools/` (cdp-bridge, setup, lock-api), the token, the REST and WebSocket client, the install
and Set up screens, the "starting" screen, the Hyprland window rule, and every pear workaround (`songReal`,
`expect`, `stateEpoch`, `timeOffset`, the play/pause settle, the volume curve, page checks and probes).

### One engine link for the whole shell

Today every monitor's copy has its own connections. The service holds one socket, parses each line once, and
every copy binds to its properties.

- **Connect** when ytmfast's MPRIS entry appears (Quickshell's MPRIS service, `org.mpris.MediaPlayer2.ytmfast`),
  when the panel has been open 400 ms, or on any command (play, a media key, a row). Connecting starts the
  engine through its socket unit when it is not running.
- **Let go** when the engine closes the socket (it quit). Never reconnect by itself: with socket activation that
  would start the engine again.
- **Nothing runs while the engine is quit:** no timers, no polling, no processes.

### State

- From `state` events: song, play state, volume, mute, shuffle, repeat, like, album id. The engine's word is
  trusted as it is (no pear-style checks).
- Position: from `position` events, run forward locally only while something on screen needs it (the panel's
  seek bar, or the lyrics), never for the bar alone.
- **While the engine is quit:** the bar shows the last song dimmed, read from ytmfast's
  `$XDG_STATE_HOME/ytmfast/state.json` (read-only) at shell start and when the engine goes away. The widget's own
  `last.json` is gone; play resumes through the engine's own resume.

### Panel

- Every view as today: Home, Library (5 pages), search with filters, albums, playlists, artists and podcasts in
  place with Back, paging as you scroll, "Show all", Queue, Lyrics, Up next with the Autoplay divider.
- Browse, search, more, playPage and play `endpoint` map one to one onto the engine's commands (step 3 made the
  row shapes identical to Page.js's).
- Home and Library are cached 10 minutes in the service (one cache for all monitors), refreshed quietly after.
- Queue rows act by `queueId` (`queue.jump`, `queue.remove`); add next / add to end with `queue.add`.
- **Cover click opens the album** (`albumId`); with no album id it does nothing (no window to show any more).
- Up next = the songs after the current one in the `queue` event; the "Autoplay" divider goes where the list's
  own songs end and radio begins (the engine marks radio items; small engine addition: `radio: true` on queue
  items).
- **Signed out:** the panel shows a sign-in screen in the old Set up screen's style: one line saying the session
  ended, and a **Sign in** button that runs `ytmfast import-session --browser brave-origin` and reconnects.
- Messages: as today, in the panel when it is open, as a desktop notification when it is not.

### Lyrics

The widget asks `lyrics` once per song while the Lyrics tab is open, and draws what comes back exactly as today
(line glide, blur, faded lines, word fill, break dots, tap to seek). The clock is the engine's position events,
run forward between them; a `seeked` event jumps it. Target: within ±30 ms of the engine, as today.

### Kept as they are

Bar look, equalizer at 5 Hz while playing, title width, left/right/middle click, wheel skips, all panel keys,
tooltips, the IPC names (`status` keeps its shape for the media-key script), `showTitle` and `maxLabelWidth`
settings. `idleMinutes` goes: the engine's own idle quit (5 min on AC, 2 on battery) is the only one.

### One player at a time, and media keys

- When ytmfast starts playing, the service stops World Radio (as today, once, not per monitor). World Radio's
  `pause` call reaches the service, which pauses the engine only if connected (never starts it).
- The media-key script: a playing or paused ytmfast is driven through MPRIS like any player (Omarchy's media
  service); with no player at all, the key goes to the widget, which starts the engine and resumes. The
  pear-specific branches go.

## Part C: on the user's laptop

1. **Before anything changes, measure** the current widget with pear-desktop (below).
2. Build A and B, test, check in the nested compositor, then live.
3. After the live check passes: remove `pear-desktop` and `electron42` (only pear needs it), its profile
   `~/.config/YouTube Music`, the menu entry, `nic-ytm-lock`, `nic-ytm-publish`, the token, the `special:music`
   window rule in `hyprland.lua` and its backup exclude note. A rollback copy (packages from the cache, the
   profile, the old plugin folder) goes in `~/.local/share/nic-rollbacks/ytmfast-<date>-step4/`, 0700, and
   a timer deletes it 14 days later.
4. The public plugin repo gets its README note and is otherwise left alone.
5. The new widget goes into the desktop config backup (it was left out while it was public).

## Numbers (measured before and after)

| Measure | How |
|---|---|
| Shell memory | The shell's PSS with the widget loaded, panel closed, music playing; and with the panel open on Home |
| Shell CPU | Shell CPU over 120 s playing with the panel closed; with the panel open; with word lyrics on screen |
| Whole player | Shell + player processes (pear + bridge, or ytmfast): RAM, CPU, RAPL watts above idle |
| Processes spawned | Per song change, and per lyrics lookup (curl today) |
| Wakeups | Shell wakeups per second while the engine is quit |
| Response | Panel open to Home shown (cached, cold); click a row to sound; play/pause click to the icon change |

## Testing

- **Engine (CI):** lyrics golden tests (A1), KRC decode and zlib caps, cookie decryption for Brave's key name
  against a generated database, `watch`, the Topic strip, `albumId` on `queue.add`, `radio` on queue items.
- **Widget:** QML tests (qmltestrunner) with a stub socket that feeds recorded engine lines: connect on MPRIS
  appear, never reconnect after close, state mapping, queue rows by id, cache ages, lyrics clock, notifications
  with the panel closed, the World Radio rule, the sign-in screen.
- **Looks the same:** in the nested compositor test rig, the bar and each panel view (Home, Library, an album, a
  search, Queue, Lyrics with words) screenshotted before (today's widget) and after, with the same
  account pages; any layout or colour difference is a bug.
- **Live**, speakers checked first: every item of the main spec's feature parity list, media keys, World Radio
  both ways, sign-in from Brave, idle quit and the bar's dimmed last song after it.

## Out of scope

- A sign-in flow of ytmfast's own (only if the Brave session sharing causes sign-outs).
- Changes to how anything looks.
