# ytmfast

A headless YouTube Music engine for bar widgets. It plays music straight to PipeWire and
takes commands over a local socket, with no browser running.

## Status

Step 1 is in progress: the goal is to play one song. Nothing is usable yet.

## Build

```sh
cargo build --release
```

Building needs Rust, clang, pkg-config and the PipeWire and Opus development files.

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
