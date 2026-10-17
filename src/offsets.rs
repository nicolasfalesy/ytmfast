//! How far each song's synced lyrics are shifted, remembered between runs.
//!
//! A song's lyrics can drift against the audio (the timing was made for another version of
//! the song). The widget shifts them by a quarter second at a time; this keeps the shift per
//! song, in a small file of its own beside `state.json` (so the play state's format is
//! untouched), 0600, written atomically.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The furthest a song's lyrics can be shifted, either way. Enough for a different edit of
/// a song, and it bounds what a client can ask for.
pub const MAX_SECONDS: f64 = 30.0;

/// How many songs are remembered; the one set longest ago is forgotten first.
pub const MAX_SONGS: usize = 500;

/// A bigger file is not ours (500 songs come to about 20 KB): it is ignored, not read.
const MAX_FILE_BYTES: u64 = 64 * 1024;

const FILE_NAME: &str = "lyrics-offsets.json";

/// The file's format version; a file with another is not read (and is replaced by the next
/// save).
const VERSION: u64 = 1;

/// The offsets in memory, and where they are kept (nowhere, for tests and a daemon with no
/// state folder).
pub struct LyricsOffsets {
    dir: Option<PathBuf>,
    /// Oldest set first, so the cap drops the song set longest ago.
    songs: Mutex<Vec<(String, f64)>>,
}

impl LyricsOffsets {
    /// Offsets that are not kept anywhere.
    pub fn in_memory() -> LyricsOffsets {
        LyricsOffsets {
            dir: None,
            songs: Mutex::new(Vec::new()),
        }
    }

    /// The offsets kept in `dir`. A missing, damaged, oversized or foreign file means no
    /// offsets, never an error: a nudge is a convenience, not state worth stopping for.
    pub fn open(dir: &Path) -> LyricsOffsets {
        LyricsOffsets {
            dir: Some(dir.to_owned()),
            songs: Mutex::new(load(&dir.join(FILE_NAME)).unwrap_or_default()),
        }
    }

    /// The song's shift in seconds; 0 when it has none.
    pub fn get(&self, video_id: &str) -> f64 {
        self.lock()
            .iter()
            .find(|(id, _)| id == video_id)
            .map_or(0.0, |(_, s)| *s)
    }

    /// Sets the song's shift (rounded to the millisecond and held within `MAX_SECONDS`); 0
    /// forgets the song. Returns the value kept. The memory changes even when the file can't
    /// be written; the error says so.
    pub fn set(&self, video_id: &str, seconds: f64) -> io::Result<f64> {
        let kept = clean(seconds);
        let snapshot = {
            let mut songs = self.lock();
            songs.retain(|(id, _)| id != video_id);
            if kept != 0.0 {
                songs.push((video_id.to_owned(), kept));
                if songs.len() > MAX_SONGS {
                    let extra = songs.len() - MAX_SONGS;
                    songs.drain(..extra);
                }
            }
            songs.clone()
        };
        if let Some(dir) = &self.dir {
            save(dir, &snapshot)?;
        }
        Ok(kept)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<(String, f64)>> {
        // A panic elsewhere must not take the lyrics' shifts down with it.
        self.songs.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A shift as it is kept: to the millisecond, within `MAX_SECONDS`; 0 for what is not a
/// number.
fn clean(seconds: f64) -> f64 {
    if !seconds.is_finite() {
        return 0.0;
    }
    // `+ 0.0` turns a negative zero (from rounding -0.0004) into a plain one.
    (seconds.clamp(-MAX_SECONDS, MAX_SECONDS) * 1000.0).round() / 1000.0 + 0.0
}

/// Reads the file; `None` for anything but a small regular file of this version. Entries
/// that fail their checks are left out one by one; a repeated song keeps its last entry.
fn load(path: &Path) -> Option<Vec<(String, f64)>> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // O_NOFOLLOW: a link in its place is not followed to wherever it points.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return None;
    }
    let mut text = String::new();
    file.take(MAX_FILE_BYTES).read_to_string(&mut text).ok()?;
    let doc: serde_json::Value = serde_json::from_str(&text).ok()?;
    if doc.get("version")?.as_u64()? != VERSION {
        return None;
    }
    let mut songs: Vec<(String, f64)> = Vec::new();
    for entry in doc.get("songs")?.as_array()? {
        let (Some(id), Some(seconds)) = (
            entry.get(0).and_then(serde_json::Value::as_str),
            entry.get(1).and_then(serde_json::Value::as_f64),
        ) else {
            continue;
        };
        let seconds = clean(seconds);
        if !crate::streams::is_video_id(id) || seconds == 0.0 {
            continue;
        }
        songs.retain(|(known, _)| known != id);
        songs.push((id.to_owned(), seconds));
    }
    let extra = songs.len().saturating_sub(MAX_SONGS);
    songs.drain(..extra);
    Some(songs)
}

/// Writes the file: a fresh 0600 temp file (never through a link), synced, then renamed over
/// the old one, so a crash leaves the old file or the new, never half of one.
fn save(dir: &Path, songs: &[(String, f64)]) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let doc = serde_json::json!({ "version": VERSION, "songs": songs });
    let bytes = serde_json::to_vec(&doc).map_err(io::Error::other)?;
    let temp = dir.join(format!("{FILE_NAME}.tmp"));
    // One a crash left behind is removed, not reused, so its mode and owner can't carry over.
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_data());
    drop(file);
    if let Err(e) = written.and_then(|()| std::fs::rename(&temp, dir.join(FILE_NAME))) {
        let _ = std::fs::remove_file(&temp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn id(n: usize) -> String {
        format!("song{n:07}")[..11].to_owned()
    }

    #[test]
    fn a_song_without_an_offset_has_none() {
        let o = LyricsOffsets::in_memory();
        assert_eq!(o.get("dQw4w9WgXcQ"), 0.0);
    }

    #[test]
    fn an_offset_is_kept_rounded_and_held_within_the_limit() {
        let o = LyricsOffsets::in_memory();
        assert_eq!(o.set("dQw4w9WgXcQ", 0.25).unwrap(), 0.25);
        assert_eq!(o.get("dQw4w9WgXcQ"), 0.25);
        // To the millisecond.
        assert_eq!(o.set("dQw4w9WgXcQ", 0.123_456).unwrap(), 0.123);
        // Held within 30 s, whatever was asked.
        assert_eq!(o.set("dQw4w9WgXcQ", 99.0).unwrap(), MAX_SECONDS);
        assert_eq!(o.set("dQw4w9WgXcQ", -99.0).unwrap(), -MAX_SECONDS);
        // Not a number: nothing is kept.
        assert_eq!(o.set("dQw4w9WgXcQ", f64::NAN).unwrap(), 0.0);
        assert_eq!(o.get("dQw4w9WgXcQ"), 0.0);
    }

    #[test]
    fn zero_forgets_the_song() {
        let dir = tempfile::tempdir().unwrap();
        let o = LyricsOffsets::open(dir.path());
        o.set("dQw4w9WgXcQ", 1.5).unwrap();
        o.set("dQw4w9WgXcQ", 0.0).unwrap();
        let again = LyricsOffsets::open(dir.path());
        assert_eq!(again.get("dQw4w9WgXcQ"), 0.0);
        let text = std::fs::read_to_string(dir.path().join(FILE_NAME)).unwrap();
        assert!(!text.contains("dQw4w9WgXcQ"), "{text}");
    }

    #[test]
    fn offsets_survive_a_restart_in_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let o = LyricsOffsets::open(dir.path());
        o.set("dQw4w9WgXcQ", 0.5).unwrap();
        o.set("aaaaaaaaaaa", -1.25).unwrap();
        let file = dir.path().join(FILE_NAME);
        assert_eq!(mode(&file), 0o600);
        let again = LyricsOffsets::open(dir.path());
        assert_eq!(again.get("dQw4w9WgXcQ"), 0.5);
        assert_eq!(again.get("aaaaaaaaaaa"), -1.25);
        // No temp file is left beside it.
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from(FILE_NAME)]);
    }

    #[test]
    fn the_oldest_song_is_forgotten_past_the_cap() {
        let o = LyricsOffsets::in_memory();
        for n in 0..=MAX_SONGS {
            o.set(&id(n), 1.0).unwrap();
        }
        assert_eq!(o.get(&id(0)), 0.0, "the first one set is the one dropped");
        assert_eq!(o.get(&id(1)), 1.0);
        assert_eq!(o.get(&id(MAX_SONGS)), 1.0);
        // Setting a song again makes it the newest, so it outlives the older ones.
        o.set(&id(1), 2.0).unwrap();
        o.set(&id(MAX_SONGS + 1), 1.0).unwrap();
        assert_eq!(o.get(&id(1)), 2.0);
        assert_eq!(o.get(&id(2)), 0.0);
    }

    #[test]
    fn a_damaged_oversized_or_foreign_file_means_no_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(FILE_NAME);
        for body in [
            "not json".to_owned(),
            r#"{"version":99,"songs":[["dQw4w9WgXcQ",1.0]]}"#.to_owned(),
            r#"{"version":1,"songs":"x"}"#.to_owned(),
            // Bigger than a real file ever is: not even read.
            format!(
                r#"{{"version":1,"songs":[["dQw4w9WgXcQ",1.0]],"pad":"{}"}}"#,
                "x".repeat(70_000)
            ),
        ] {
            std::fs::write(&file, body).unwrap();
            assert_eq!(LyricsOffsets::open(dir.path()).get("dQw4w9WgXcQ"), 0.0);
        }
        // A symlink in its place is not followed.
        std::fs::remove_file(&file).unwrap();
        let target = dir.path().join("elsewhere.json");
        std::fs::write(&target, r#"{"version":1,"songs":[["dQw4w9WgXcQ",1.0]]}"#).unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert_eq!(LyricsOffsets::open(dir.path()).get("dQw4w9WgXcQ"), 0.0);
    }

    #[test]
    fn a_loaded_file_is_checked_entry_by_entry() {
        let dir = tempfile::tempdir().unwrap();
        // One good entry, then a bad id, a number out of range and a duplicate: only the
        // good ones come in, held within the limit.
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"version":1,"songs":[["dQw4w9WgXcQ",1.0],["bad id",1.0],["bbbbbbbbbbb",900.0],["dQw4w9WgXcQ",2.0],["ccccccccccc",null]]}"#,
        )
        .unwrap();
        let o = LyricsOffsets::open(dir.path());
        assert_eq!(o.get("dQw4w9WgXcQ"), 2.0);
        assert_eq!(o.get("bbbbbbbbbbb"), MAX_SECONDS);
        assert_eq!(o.get("ccccccccccc"), 0.0);
        assert_eq!(o.get("bad id"), 0.0);
    }

    #[test]
    fn a_folder_that_cannot_be_written_still_keeps_the_offset_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("not-here");
        let o = LyricsOffsets::open(&gone);
        assert!(o.set("dQw4w9WgXcQ", 1.0).is_err());
        assert_eq!(o.get("dQw4w9WgXcQ"), 1.0);
    }
}
