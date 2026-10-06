//! Finds the system's libopus with pkg-config, so a missing `opus` package is a clear build
//! error (and a libopus outside the default linker path still links). See `src/audio/opus.rs`.

fn main() {
    // 1.3: opus_decode_float and the controls used have been stable since well before it.
    if let Err(e) = pkg_config::Config::new()
        .atleast_version("1.3")
        .probe("opus")
    {
        panic!("libopus was not found with pkg-config (Arch: the `opus` package): {e}");
    }
}
