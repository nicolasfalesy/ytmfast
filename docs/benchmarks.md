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

## ytmfast

Not measured yet (step 1 in progress).
