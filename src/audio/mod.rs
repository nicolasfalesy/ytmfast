//! Audio: getting a track's bytes (`fetch`), decoding them (`decode`), and playing them
//! (`player`, through a `sink`: PipeWire in `pw`, or the counting `NullSink`).

pub mod decode;
pub mod fetch;
pub mod player;
pub mod pw;
pub mod sink;
