# ytmfast

A headless YouTube Music engine for bar widgets. It plays music straight to PipeWire and
takes commands over a local socket, with no browser running.

## Status

Step 3 is done. The engine plays YouTube Music Premium quality (Opus, about 256 kbps) with pause, seek, volume and
status over the socket and over MPRIS, and has a full queue: albums, playlists and Liked songs, a song's radio, radio
when the queue runs out, shuffle and repeat, gapless playback between songs, resume after a restart, and play history
on the account. MPRIS has next, previous, shuffle and loop too. Step 3 added browsing over the socket, in the shapes
the bar widget already reads: browse (Home, the Library, playlists, albums, artists and podcasts, page by page),
search (mixed results and the Songs, Albums and other filters), like and dislike, mute, and a song's lyrics.

It is built to replace the Electron app behind a bar widget (the widget switches over in step 4), at a fraction of
the cost (full method in [docs/benchmarks.md](docs/benchmarks.md)):

| Measure | YouTube Music desktop app (Electron) | ytmfast (step 2) |
|---|---|---|
| RAM while playing | 677 MB | 45.5 MB |
| Processes | 10 | 1 |
| CPU while playing | 3.9% of one core | 0.68% |
| Time from play to sound | 2.5 s | 0.74 s |
| Extra CPU power while playing | 0.71 W | 0.10 W (measured on step 1) |

The power figure is step 1's (when the engine used 31.5 MB and 0.51% CPU); it was not measured again for step 2.
Step 3 did not change how songs play, so these are step 2's figures; its own (page load times, and memory while
browsing) are in the benchmarks file.

Coming next: **step 4, the widget.** The Omarchy bar widget switches to the engine. Its engine side is under way:
lyrics are now timed, word by word from KuGou and line by line from LRCLIB (both free, keyless services), with
YouTube Music's own plain lyrics after them. Those requests go only to `lrclib.net`, `krcs.kugou.com` and
`lyrics.kugou.com`, over https, and carry only the song's title, artists, album and length; everything else the
engine sends goes to YouTube and Google alone. See [docs/protocol.md](docs/protocol.md) (`lyrics`).

## Resume and history

The queue, the current song and the second it was at, volume, mute, shuffle and repeat are kept in
`$XDG_STATE_HOME/ytmfast/state.json` (else `~/.local/state/ytmfast/state.json`; mode 0600), so a restart comes back
to the same place, paused. That file is a list of the songs the user queued and played, so it is listening history
on disk: it holds song details and public thumbnail links, never a cookie, a session value or a stream link.
Delete it, with the engine stopped, to start fresh.

## Sign in

ytmfast reuses a sign-in the user already has: the Brave Origin browser's, or the YouTube Music desktop app's.

From Brave Origin (sign in to music.youtube.com there first; Brave Origin can stay open):

```sh
ytmfast import-session --browser brave-origin
```

It reads the first profile (`~/.config/BraveSoftware/Brave-Origin/Default`; `--profile <folder>` picks another) and
opens its cookies with the key Brave keeps in the login keyring, the item labelled "Brave Safe Storage" (an unlock
prompt may show). Brave writes new cookies to disk about every 30 seconds, so right after signing in, wait half a minute
if the import says Brave Origin isn't signed in.

From the desktop app (close it first):

```sh
ytmfast import-session
```

Either way, it first asks YouTube Music whose session it is and prints `Signed in as <name>`, so a browser signed
in to another Google account is seen at once. It refuses a profile that isn't signed in, or whose sign-in YouTube
Music no longer takes, and saves nothing when that check can't be made. Then it copies only the YouTube and Google
sign-in cookies into the login keyring (never into a plain file) and prints how many it took, never a value. If the
engine is running, it is stopped, so the next play starts it with the new session.

## Build

```sh
cargo build --release
```

Building needs Rust, clang, pkg-config, and the PipeWire and libopus development files (Arch:
`pipewire` and `opus`). Running needs the same two libraries (`libpipewire-0.3.so.0` and
`libopus.so.0`). `yt-dlp` is used as a fallback for stream links when it is installed.

## Run with systemd

The engine is meant to start on demand through a systemd user socket, and to quit by itself
when idle. To install it for the current user:

```sh
cargo install --path . --root ~/.local
mkdir -p ~/.config/systemd/user
cp dist/systemd/ytmfast.socket dist/systemd/ytmfast.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ytmfast.socket
```

The first connection to `$XDG_RUNTIME_DIR/ytmfast/socket` then starts `ytmfast daemon`. The
socket protocol is described in [docs/protocol.md](docs/protocol.md).

### Keep it running

To start the engine at login and keep it up instead (`ytmfast daemon --stay`; paused, it uses
no CPU and about 8 MB of memory), add the drop-in and have the login target pull it in:

```sh
mkdir -p ~/.config/systemd/user/ytmfast.service.d
cp dist/systemd/ytmfast.service.d/stay.conf ~/.config/systemd/user/ytmfast.service.d/
systemctl --user daemon-reload
systemctl --user add-wants default.target ytmfast.service
systemctl --user restart ytmfast.service
```

It restarts after a crash. To undo, delete
`~/.config/systemd/user/default.target.wants/ytmfast.service` and the drop-in, then run
`systemctl --user daemon-reload`.

## Startup trace

With `YTMFAST_TRACE=1` in its environment, the engine prints one line to stderr per phase of
each play (session load, player version, player script, solver, player request, download,
decoder, output), in milliseconds since the play command. The lines name phases only, never a
link or a session value.

```sh
YTMFAST_TRACE=1 ytmfast play dQw4w9WgXcQ --null-sink --seconds 3
```

## Licence

MIT. See [LICENSE](LICENSE).
