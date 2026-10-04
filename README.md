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

## Licence

MIT. See [LICENSE](LICENSE).
