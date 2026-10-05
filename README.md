# ytmfast

A headless YouTube Music engine for bar widgets. It plays music straight to PipeWire and
takes commands over a local socket, with no browser running.

## Status

Step 1 is done: the engine plays one song in YouTube Music Premium quality (Opus, about 256 kbps), with pause,
seek, volume and status over the socket and over MPRIS. It replaces the Electron app behind a bar widget at a
fraction of the cost (full method in [docs/benchmarks.md](docs/benchmarks.md)):

| Measure | YouTube Music desktop app (Electron) | ytmfast |
|---|---|---|
| RAM while playing | 677 MB | 31.5 MB |
| Processes | 10 | 1 |
| CPU while playing | 3.9% of one core | 0.51% |
| Time from play to sound | 2.5 s | 0.68 s |

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

Building needs Rust, clang, pkg-config, cmake, make and the PipeWire development files. `yt-dlp` is used as a fallback for stream links when it is installed.

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

## Licence

MIT. See [LICENSE](LICENSE).
