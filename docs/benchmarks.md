# Benchmarks

ytmfast against the Electron app it replaces (`pear-desktop`), on the same laptop (Intel Core Ultra, Arch Linux,
PipeWire 1.6.8), playing the same 5:18 song from the user's library, signed in with Premium.

## Method

- **RAM:** the sum of `Pss` from `/proc/<pid>/smaps_rollup` over every process of the player, 60 s into playback.
  For `pear-desktop` that is all its Electron processes plus the widget's `cdp-bridge` helper, which exists only
  for it.
- **CPU:** the change in `utime + stime` (from `/proc/<pid>/stat`) over the same processes during 300 s of
  playback, as a percentage of one core.
- **Time to first sound:** from the play command to the player's PipeWire output stream reaching state
  `running`, polled with `pw-dump` (resolution about 0.1 s). The app starts from closed each time.
- **Power:** see the power section below (CPU package energy from Intel RAPL; a whole-laptop battery test was too
  noisy to separate the two players).
- **Idle:** what is left running after the player has been paused for 10 minutes.
- Three runs each; the table gives the median and the range.

## pear-desktop 3.12.0 (AUR source package, Electron 42), 2026-10-04, on AC power

| Measure | Median | Range |
|---|---|---|
| RAM (PSS) | 677 MB | 672 to 680 MB |
| Processes | 10 | 10 |
| CPU while playing | 3.9% of one core | 3.7 to 4.0% |
| Time to first sound (from closed) | 2.5 s | 2.3 to 2.9 s |
| Idle | 0 processes (the widget quits the app after 5 idle minutes) | |
| Power | see the power section | |

## ytmfast 0.1.0 (step 1), 2026-10-05, on AC power

Started cold by socket activation for each run, stream volume set to 0 (PipeWire still receives and mixes every
decoded sample, so the CPU figure is the full decode-and-output cost).

| Measure | Median | Range |
|---|---|---|
| RAM (PSS) | 31.5 MB | 31.3 to 31.5 MB |
| Processes | 1 (7 threads) | |
| CPU while playing | 0.51% of one core | 0.49 to 0.53% |
| Time to first sound (from closed) | 0.68 s | 0.65 to 0.70 s |
| Idle | 0 processes (quits after 5 idle minutes on AC, 2 on battery; the systemd socket costs nothing) | |
| Idle engine, before it quits | 9.3 MB PSS | |
| Decode cost alone (into a null output, no PipeWire) | Opus 256k: about 0.14% of a performance core; AAC 256k: about 0.05 to 0.06% | |
| Power | see the power section | |

The decode-cost row comes from a release build decoding 3 minutes of each format into the engine's null output as
fast as it can (CPU time over audio time, median of 3 rounds); on an efficiency core Opus costs about 0.2%. The
difference to the 0.51% above is mostly the cost of waking at real time and of the PipeWire stream's own processing.

Stream link resolution, same song, signed in:

| Path | Time |
|---|---|
| Own code, the first time | 462 ms |
| Own code, warm | 310 ms |
| Own code, a new YouTube player version (one cold solve, once per version, about weekly) | 3 to 5.5 s |
| yt-dlp fallback | 3.9 s |

## ytmfast step 2 (the queue), 2026-10-06, on AC power

Same method and song as step 1. A play now also fetches the song's radio queue, sends play reports, and
preloads the next song for gapless playback; the 300 s window covers the song's end, the handover and the start
of the next song.

| Measure | Median | Range |
|---|---|---|
| RAM (PSS) | 45.5 MB | 45.1 to 46.2 MB |
| Processes | 1 (8 threads) | |
| CPU while playing | 0.68% of one core | 0.65 to 0.68% |
| Time to first sound (from closed) | 0.74 s | 0.66 to 0.89 s |

The extra 14 MB over step 1 is probably mostly the preloaded next song (its whole download is held in memory, about
7 MB for a 4-minute song) and the queue; this was not broken down further.

## ytmfast step 3 (browsing), 2026-10-06, on AC power

Measured live on the user's account, with a private engine on a silent output. Each time is the median of 3 tries; "cold"
is the first request after the engine starts.

| Measure | Median | Range |
|---|---|---|
| Browse a page, cold | 770 ms | 641 to 781 ms |
| Browse a page, warm | 316 ms | 248 to 317 ms |
| Search, cold | 615 ms | 548 to 680 ms |
| Search, warm | 589 ms | 419 to 630 ms |
| Next page (a continuation), warm | 181 ms | 169 to 197 ms |
| RAM (PSS), idle after a search | 13.6 MB | |
| RAM added by scrolling a 500-song playlist to the end | 0.0 MB | 24.2 MB before and after |

The 500-song playlist came in 5 pages (about 72 KB of replies per 138 rows). Pages go straight to the client and
nothing is kept, so scrolling does not grow the engine. Lyrics took 68 ms and 88 ms (one try each) and 0 ms when
asked again, from the cache.

### Matches the widget

The same live YouTube answers were read by ytmfast and by the bar widget's own parser (`Page.js`), and the two
results compared field by field. After one fix (YouTube Music's own tile art on `www.gstatic.com`, such as the Liked
songs tile, had come out blank), there were 0 differences on: Home and its next 2 pages, Liked albums, Library
artists, a search (mixed results, the Songs filter and its next page), an album, an artist and its "Show all" page,
a 140-song playlist and its next page, a podcast page, and lyrics.

## Power (CPU package energy), 2026-10-05, on AC power

Measured with the processor's own energy counter (Intel RAPL, `intel-rapl:0` package) over 240 s of playback after
a 30 s warm-up, both players started cold, speakers muted (both still decode and stream everything). The runs
alternate pear, ytmfast, ytmfast, pear (twice) so slow drift cancels out. An earlier battery test (whole-laptop
draw on battery) could not separate the two: the laptop's own draw drifted from 6.8 W to 2.6 W during it.

| Measure | pear-desktop | ytmfast |
|---|---|---|
| CPU package power while playing, median | 3.45 W (3.35 to 3.54) | 2.84 W (2.78 to 2.96) |
| Above the idle laptop (2.74 W) | about 0.71 W | about 0.10 W |

## Side by side

| Measure | pear-desktop | ytmfast | Change |
|---|---|---|---|
| RAM | 677 MB | 45.5 MB | 15x less |
| Processes | 10 | 1 | |
| CPU while playing | 3.9% | 0.68% | 5.7x less |
| Time to first sound | 2.5 s | 0.74 s | 3.4x faster |
| CPU package power above idle | 0.71 W | 0.10 W | about 7x less |

The ytmfast numbers are from step 2 (2026-10-06), except the power row, which was measured in step 1.
