# ytmfast

A headless YouTube Music engine for bar widgets. It plays music straight to PipeWire and
takes commands over a local socket, with no browser running.

## Status

Step 1 is done: the engine plays one song in YouTube Music Premium quality (Opus, about 256 kbps), with pause,
seek, volume and status over the socket and over MPRIS. It is built to replace the Electron app behind a bar widget
(the widget switches over in step 4), at a fraction of the cost (full method in [docs/benchmarks.md](docs/benchmarks.md)):

| Measure | YouTube Music desktop app (Electron) | ytmfast |
|---|---|---|
| RAM while playing | 677 MB | 31.5 MB |
| Processes | 10 | 1 |
| CPU while playing | 3.9% of one core | 0.51% |
| Time from play to sound | 2.5 s | 0.68 s |
| Extra CPU power while playing | 0.71 W | 0.10 W |

Coming next:

1. **Queue:** albums, playlists, radio when the queue runs out, gapless playback, resume, play history.
2. **Browsing:** Home, Library, search, like and dislike, lyrics.
3. **Widget:** the Omarchy bar widget switches to the engine.

## Sign in

ytmfast reuses the sign-in of the YouTube Music desktop app. Close that app, then run:

```sh
ytmfast import-session
```

It copies the session into the login keyring (never into a plain file) and prints how many cookies it took. It
refuses a profile that isn't signed in. If the engine is running, it is stopped, so the next play starts it with the
new session.

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
