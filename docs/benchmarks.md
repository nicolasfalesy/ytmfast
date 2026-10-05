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
- **Power:** the average of `/sys/class/power_supply/BAT*/power_now`, sampled every 1 s over 300 s of playback on
  battery. Not measured yet.
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
| Power | not measured yet | |

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
| Power | not measured yet | |

Stream link resolution (own code against the yt-dlp fallback, same song, signed in): own code 462 ms the first
time and 310 ms warm; yt-dlp 3.9 s. A new YouTube player version costs one cold solve of 3 to 5.5 s, once.

## Side by side

| Measure | pear-desktop | ytmfast | Change |
|---|---|---|---|
| RAM | 677 MB | 31.5 MB | 21x less |
| Processes | 10 | 1 | |
| CPU while playing | 3.9% | 0.51% | 7.6x less |
| Time to first sound | 2.5 s | 0.68 s | 3.7x faster |
