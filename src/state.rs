//! Resume state: `state.json` in `paths::state_dir()`, so a restart (an update, a
//! `systemctl --user restart`, an idle quit) comes back to the same queue, song and second.
//!
//! What it holds: the queue's songs (their details, never a link), which one is current and
//! where in it, the volume, shuffle (with the order from before shuffling), repeat, and where
//! the queue came from (the playlist, and the radio's next-page token so a resumed radio keeps
//! refilling). Never a cookie, a session value or a stream link. The one kind of URL in it is
//! a song's thumbnail: a public image link on an allowed host, checked again on load.
//!
//! The file is 0600 and written atomically (a temp file in the same folder, synced, then
//! renamed over the old one), so a crash or a power loss mid-write leaves the old file or
//! the new one, never half of one. Writes happen on a thread of their own (`Writer`), never on
//! the engine's task: at most one is in flight, and a newer snapshot replaces one still
//! waiting.
//!
//! A file that can't be used (not JSON, an unknown version) is never deleted: it is renamed
//! to `state.json.bad` for the user to look at, with one log line, and the engine starts
//! fresh.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::ops::Range;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use url::Url;

use crate::innertube::SongItem;
use crate::net;
use crate::queue::Repeat;
use crate::streams::is_video_id;

/// The file's format. A file with any other version is set aside, not guessed at.
pub const VERSION: u32 = 1;

pub const FILE_NAME: &str = "state.json";
const BAD_NAME: &str = "state.json.bad";
const TEMP_NAME: &str = "state.json.tmp";

/// At most this many songs are saved, around the current one (the plan's cap): a radio
/// left running for days can't grow the file (or each write) without bound.
pub const MAX_ITEMS: usize = 500;

/// Read cap. 500 songs come to well under 1 MiB; anything far bigger is not ours.
const MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// A playlist id: YouTube's are short and URL-safe (`LM`, `PL…`, `OLAK5uy_…`, `RDAMVM…`).
const MAX_PLAYLIST_ID: usize = 256;
/// A `next` continuation token: opaque, base64-like, a few hundred bytes in practice.
const MAX_CONTINUATION: usize = 16 * 1024;

/// A loaded song's text fields (title, album, each artist, its playlist id): real ones are
/// tens of bytes. Each is sent to every widget on every queue change, so a hand-edited file
/// must not be able to make them huge. The socket's `queue.add` takes the same caps, so a
/// song it took is never dropped when the state is loaded again.
pub const MAX_TEXT: usize = 4 * 1024;
/// A loaded song's artists: real bylines name a handful.
pub const MAX_ARTISTS: usize = 20;

/// A second to resume at, for a song whose length isn't known: a day is past any song.
const MAX_POSITION_UNKNOWN_LENGTH: f64 = 24.0 * 60.0 * 60.0;

/// Where the queue came from. Saved so a resumed queue keeps the same refill behaviour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// A finite list: an album, a playlist, Liked songs (it may still page).
    #[default]
    List,
    /// An endless radio (a song's radio or a mix): it refills from `continuation`.
    Radio,
}

/// Everything a restart needs. `queue` is in play order (the shuffled order while shuffle
/// is on), as the user saw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Saved {
    /// Always `VERSION` when written.
    pub version: u32,
    pub queue: Vec<SongItem>,
    /// Index into `queue` of the current song (0 for an empty queue).
    pub current_index: usize,
    /// Seconds into the current song.
    pub position: f64,
    /// 0.0 to 1.0.
    pub volume: f32,
    pub shuffle: bool,
    /// While shuffled: the order from before shuffling, as indexes into `queue`, so turning
    /// shuffle off after a restart still goes back to it. `None` while shuffle is off.
    ///
    /// Why the shuffled order plus these positions (and not the original order plus a
    /// permutation): `queue` then reads as the user's queue on its own, and the positions are
    /// small numbers that need no ids (queue ids are renumbered on load).
    #[serde(default)]
    pub original_order: Option<Vec<usize>>,
    pub repeat: Repeat,
    /// The playlist the queue was made from (a radio's is `RDAMVM` + the seed song).
    pub source_playlist: Option<String>,
    #[serde(default)]
    pub source_kind: SourceKind,
    /// The token for the queue's next page. Not a secret (it names a list, not a user), but
    /// opaque: it is never logged.
    #[serde(default)]
    pub continuation: Option<String>,
    /// YouTube had no more songs for this queue: no more radio requests.
    #[serde(default)]
    pub exhausted: bool,
    /// When this was written (seconds since 1970).
    pub saved_unix: u64,
}

impl Default for Saved {
    fn default() -> Self {
        Saved {
            version: VERSION,
            queue: Vec::new(),
            current_index: 0,
            position: 0.0,
            volume: 1.0,
            shuffle: false,
            original_order: None,
            repeat: Repeat::Off,
            source_playlist: None,
            source_kind: SourceKind::List,
            continuation: None,
            exhausted: false,
            saved_unix: 0,
        }
    }
}

/// The part of a `len`-song queue that is saved: at most `max` songs, the current one in the
/// middle where it can be, the window slid back inside the queue at either end.
pub fn window(len: usize, current: usize, max: usize) -> Range<usize> {
    if len <= max {
        return 0..len;
    }
    let start = current.saturating_sub(max / 2).min(len - max);
    start..start + max
}

/// Writes `saved` to `dir/state.json`: 0600, atomically.
///
/// The bytes go to a fresh temp file (0600 from its creation, never through a symlink),
/// which is synced and then renamed over `state.json`; the folder is synced too so the
/// rename survives a power loss. A reader (or a crash) sees the old file or the new one.
pub fn save(dir: &Path, saved: &Saved) -> io::Result<()> {
    let bytes = serde_json::to_vec(saved).map_err(io::Error::other)?;
    let temp = dir.join(TEMP_NAME);
    // A temp file left by a crash: removed, not reused, so its mode and owner can't carry
    // over. (`create_new` below then refuses anything that appears in between.)
    match std::fs::remove_file(&temp) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut file = OpenOptions::new()
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
    // The rename itself is only durable once the folder is synced. A failure here leaves a
    // complete file that a crash might roll back to the previous one: not worth an error.
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Reads `dir/state.json`. `None` when there is none, or when it can't be used (then it is
/// renamed to `state.json.bad`, with one log line). Every song is checked again as if it had
/// come from YouTube: a bad video id or a thumbnail off the allowed hosts drops the song.
pub fn load(dir: &Path) -> Option<Saved> {
    let path = dir.join(FILE_NAME);
    let bytes = match read_capped(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return None,
        Err(e) => {
            // A symlink (ELOOP), a folder, a file too big: not ours to trust.
            set_aside(dir, &format!("could not be read ({:?})", e.kind()));
            return None;
        }
    };
    let saved = match serde_json::from_slice::<Saved>(&bytes) {
        Ok(s) if s.version == VERSION => s,
        Ok(_) => {
            set_aside(dir, "is from another version");
            return None;
        }
        // The serde error names a line and column only, but nothing of it is needed: the
        // file is kept for the user.
        Err(_) => {
            set_aside(dir, "is not valid");
            return None;
        }
    };
    Some(sanitize(saved))
}

/// The file's bytes, refusing a symlink and anything over `MAX_FILE_BYTES`.
fn read_capped(path: &Path) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(io::Error::from(io::ErrorKind::FileTooLarge));
    }
    Ok(bytes)
}

/// Keeps an unusable file as `state.json.bad` (replacing an older one) and says so once.
/// Never deletes it: it may be the user's only copy of a long queue.
fn set_aside(dir: &Path, why: &str) {
    let kept = std::fs::rename(dir.join(FILE_NAME), dir.join(BAD_NAME)).is_ok();
    if kept {
        eprintln!("ytmfast: {FILE_NAME} {why}; kept it as {BAD_NAME} and started fresh");
    } else {
        eprintln!("ytmfast: {FILE_NAME} {why}; started fresh");
    }
}

/// Makes a loaded file safe to use: songs checked like fresh ones from YouTube (dropped
/// otherwise, with the indexes moved to match), tokens checked, numbers clamped.
fn sanitize(mut s: Saved) -> Saved {
    let keep: Vec<bool> = s.queue.iter().map(song_ok).collect();
    // Old index -> new index, for the songs kept.
    let mut new_index = Vec::with_capacity(keep.len());
    let mut n = 0;
    for &k in &keep {
        new_index.push(k.then_some(n));
        n += usize::from(k);
    }
    // An index past the end starts at the first song, as `Queue::replace` does (ruling S8);
    // its second can't be trusted then either.
    let in_range = s.current_index < s.queue.len();
    let old_current = if in_range { s.current_index } else { 0 };
    // The current song if kept; else the next kept one after it; else the last before it.
    let current = new_index.get(old_current).copied().flatten();
    let moved = current.is_none() || !in_range;
    let current = current
        .or_else(|| new_index.iter().skip(old_current).flatten().next().copied())
        .or_else(|| new_index.iter().take(old_current).flatten().last().copied())
        .unwrap_or(0);
    let length = s
        .queue
        .get(old_current)
        .filter(|_| !moved)
        .map_or(0, |song| song.length_seconds);

    let mut i = 0;
    s.queue.retain(|_| {
        i += 1;
        keep[i - 1]
    });
    s.current_index = current;

    s.original_order = if s.shuffle {
        let order: Vec<usize> = s
            .original_order
            .unwrap_or_default()
            .into_iter()
            .filter_map(|p| new_index.get(p).copied().flatten())
            .collect();
        Some(if is_permutation(&order, s.queue.len()) {
            order
        } else {
            (0..s.queue.len()).collect()
        })
    } else {
        None
    };

    // A second past the song's end (or nonsense) means its start; the current song's
    // second belongs to it alone.
    let fine = s.position.is_finite()
        && s.position >= 0.0
        && (length == 0 || s.position < f64::from(length));
    if moved || !fine {
        s.position = 0.0;
    } else if length == 0 {
        s.position = s.position.min(MAX_POSITION_UNKNOWN_LENGTH);
    }
    s.volume = if s.volume.is_finite() {
        s.volume.clamp(0.0, 1.0)
    } else {
        1.0
    };
    s.source_playlist = s.source_playlist.filter(|p| is_playlist_id(p));
    // Continuations are base64 (with `-_` or `+/`), sometimes URL-escaped (`%3D`).
    s.continuation = s
        .continuation
        .filter(|c| token_ok(c, MAX_CONTINUATION, b"%=+/."));

    // The same cap as a save (`Engine::saved`): an old or hand-written file can't restore a
    // queue the engine would never have saved (tens of thousands of songs fit under the read
    // cap), which would then go to every widget on every queue change.
    let range = window(s.queue.len(), s.current_index, MAX_ITEMS);
    if range.len() < s.queue.len() {
        s.queue.truncate(range.end);
        s.queue.drain(..range.start);
        s.current_index -= range.start;
        s.original_order = s.original_order.map(|order| {
            order
                .into_iter()
                .filter(|p| range.contains(p))
                .map(|p| p - range.start)
                .collect()
        });
    }
    s
}

/// A song as `next` would have let it through: a real video id, and a thumbnail (if any) on
/// an allowed https host. The id goes into links and yt-dlp arguments, the thumbnail to the
/// bar widgets, which load it. Text fields over `MAX_TEXT` (or over `MAX_ARTISTS` artists)
/// drop the whole song, like a bad id: only a hand-edited file has them, and a song with a
/// trimmed title would be a different song.
fn song_ok(song: &SongItem) -> bool {
    let text_ok = |t: &str| t.len() <= MAX_TEXT;
    is_video_id(&song.video_id)
        && text_ok(&song.title)
        && song.album.as_deref().is_none_or(text_ok)
        && song.playlist_id.as_deref().is_none_or(text_ok)
        && song.artists.len() <= MAX_ARTISTS
        && song.artists.iter().all(|a| text_ok(a))
        && song
            .thumbnail
            .as_deref()
            .is_none_or(|t| Url::parse(t).is_ok_and(|u| net::allowed_host(&u)))
}

/// Every index in `0..len` exactly once.
fn is_permutation(order: &[usize], len: usize) -> bool {
    if order.len() != len {
        return false;
    }
    let mut seen = vec![false; len];
    order
        .iter()
        .all(|&p| p < len && !std::mem::replace(&mut seen[p], true))
}

/// A playlist id as the engine takes it (from the socket or a saved state): not empty, at
/// most 256 bytes of ASCII letters, digits, `-` and `_`.
pub fn is_playlist_id(p: &str) -> bool {
    token_ok(p, MAX_PLAYLIST_ID, b"")
}

/// Not empty, at most `max` bytes, ASCII letters, digits, `-`, `_` and `extra`.
fn token_ok(t: &str, max: usize, extra: &[u8]) -> bool {
    !t.is_empty()
        && t.len() <= max
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || extra.contains(&b))
}

/// What `Writer::with` saves with: `save` into a folder in production, a recorder in tests.
pub type SaveFn = Box<dyn FnMut(&Saved) -> io::Result<()> + Send>;

/// The snapshot waiting for the writer thread, and whether it should stop.
#[derive(Default)]
struct Slot {
    pending: Option<Saved>,
    closed: bool,
}

type Shared = Arc<(Mutex<Slot>, Condvar)>;

/// Saves snapshots on a thread of its own. At most one write is in flight; a snapshot that
/// arrives while one is being written waits, and a newer one replaces it (only the latest
/// matters).
///
/// A plain thread rather than `spawn_blocking`: a write stuck on a hung disk must not hold up
/// the runtime's shutdown (tokio waits for its blocking tasks; nothing waits for this thread
/// past `finish`'s bound, and the process exit ends it).
pub struct Writer {
    shared: Shared,
    /// Fires when the thread has written its last snapshot and ended. `None` when the thread
    /// could not be started.
    done: Option<oneshot::Receiver<()>>,
}

impl Writer {
    /// Saves into `dir` (see `save`).
    pub fn spawn(dir: PathBuf) -> Writer {
        Self::with(Box::new(move |s| save(&dir, s)))
    }

    pub fn with(mut save: SaveFn) -> Writer {
        let shared: Shared = Arc::default();
        let (done_tx, done) = oneshot::channel();
        let theirs = shared.clone();
        let thread = std::thread::Builder::new()
            .name("ytmfast-state".into())
            .spawn(move || {
                while let Some(saved) = take(&theirs) {
                    if let Err(e) = save(&saved) {
                        // The kind only: an io::Error's text can name the path (the home
                        // folder), and the engine carries on either way.
                        eprintln!("ytmfast: could not save the play state ({:?})", e.kind());
                    }
                }
                let _ = done_tx.send(());
            });
        let done = match thread {
            Ok(_) => Some(done),
            Err(_) => {
                eprintln!("ytmfast: could not start the state writer; nothing will be saved");
                None
            }
        };
        Writer { shared, done }
    }

    /// Queues `saved` for writing, replacing any snapshot still waiting.
    pub fn submit(&self, saved: Saved) {
        let (slot, wake) = &*self.shared;
        slot.lock().unwrap_or_else(PoisonError::into_inner).pending = Some(saved);
        wake.notify_one();
    }

    /// Writes `last`, then stops the thread. Waits at most `within` for that write (the disk
    /// may hang); true when it finished in time.
    pub async fn finish(mut self, last: Saved, within: Duration) -> bool {
        self.submit(last);
        self.close();
        match self.done.take() {
            Some(done) => matches!(tokio::time::timeout(within, done).await, Ok(Ok(()))),
            None => false,
        }
    }

    fn close(&self) {
        let (slot, wake) = &*self.shared;
        slot.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
        wake.notify_one();
    }
}

impl Drop for Writer {
    /// Without `finish`: the thread writes what is waiting, then ends.
    fn drop(&mut self) {
        self.close();
    }
}

/// The writer thread's next snapshot; `None` once closed with nothing left to write.
fn take(shared: &Shared) -> Option<Saved> {
    let (slot, wake) = &**shared;
    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some(s) = slot.pending.take() {
            return Some(s);
        }
        if slot.closed {
            return None;
        }
        slot = wake.wait(slot).unwrap_or_else(PoisonError::into_inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn song(v: &str) -> SongItem {
        SongItem {
            video_id: v.to_string(),
            title: format!("Title {v}"),
            artists: vec!["Artist".into()],
            album: Some("Album".into()),
            thumbnail: Some(format!("https://i.ytimg.com/vi/{v}/hq.jpg")),
            length_seconds: 200,
            playlist_id: Some("OLAK5uy_abc".into()),
        }
    }

    /// An 11-character id from one letter.
    fn vid(c: char) -> String {
        c.to_string().repeat(11)
    }

    fn saved_of(ids: &str, current: usize) -> Saved {
        Saved {
            queue: ids.chars().map(|c| song(&vid(c))).collect(),
            current_index: current,
            position: 42.5,
            volume: 0.4,
            source_playlist: Some("RDAMVMAAAAAAAAAAA".into()),
            source_kind: SourceKind::Radio,
            continuation: Some("CONT-token_1%3D".into()),
            saved_unix: 1_790_000_000,
            ..Saved::default()
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn save_is_atomic_and_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let first = saved_of("ABC", 1);
        save(dir.path(), &first).unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(load(dir.path()), Some(first.clone()));

        // A stale temp file from a crash, left wide open, must not leak its mode or bytes.
        let temp = dir.path().join(TEMP_NAME);
        std::fs::write(&temp, "junk").unwrap();
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Atomic: a reader holding the old file still reads all of it after the save,
        // because the new file is renamed over it rather than written into it.
        let mut old = File::open(&path).unwrap();
        let second = saved_of("DE", 0);
        save(dir.path(), &second).unwrap();
        let mut text = String::new();
        old.read_to_string(&mut text).unwrap();
        let was: Saved = serde_json::from_str(&text).unwrap();
        assert_eq!(
            was, first,
            "the old file was replaced, not rewritten in place"
        );

        assert_eq!(mode(&path), 0o600);
        assert!(!temp.exists(), "no temp file left behind");
        assert_eq!(load(dir.path()), Some(second));
        // Exactly the one file (and nothing else) in the folder.
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, [FILE_NAME]);
    }

    #[test]
    fn corrupt_file_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(FILE_NAME);
        let bad = dir.path().join(BAD_NAME);

        // Nothing saved: nothing loaded, nothing made.
        assert_eq!(load(dir.path()), None);
        assert!(!bad.exists());

        // Not JSON: ignored, and kept for the user as state.json.bad (never deleted).
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(load(dir.path()), None);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "{ not json");

        // A version this build doesn't know: the same.
        let mut future = serde_json::to_value(saved_of("AB", 0)).unwrap();
        future["version"] = 2.into();
        std::fs::write(&path, future.to_string()).unwrap();
        assert_eq!(load(dir.path()), None);
        assert!(!path.exists());
        assert!(
            std::fs::read_to_string(&bad)
                .unwrap()
                .contains("\"version\":2")
        );

        // Valid JSON of the wrong shape.
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        assert_eq!(load(dir.path()), None);
        assert_eq!(std::fs::read_to_string(&bad).unwrap(), "[1, 2, 3]");

        // A save after that works as usual.
        save(dir.path(), &saved_of("A", 0)).unwrap();
        assert!(load(dir.path()).is_some());
    }

    #[test]
    fn loaded_items_are_revalidated() {
        let dir = tempfile::tempdir().unwrap();
        // Songs A..F; B has an id that isn't one, D a thumbnail off the allowed hosts, F a
        // thumbnail over plain http. The current song is E (index 4).
        let mut s = saved_of("ABCDEF", 4);
        s.queue[1].video_id = "B&list=x/../".into();
        s.queue[3].thumbnail = Some("https://evil.example/t.jpg".into());
        s.queue[5].thumbnail = Some("http://i.ytimg.com/t.jpg".into());
        s.shuffle = true;
        // The order before shuffling: F, E, D, C, B, A.
        s.original_order = Some(vec![5, 4, 3, 2, 1, 0]);
        save(dir.path(), &s).unwrap();

        let got = load(dir.path()).unwrap();
        let ids: Vec<String> = got.queue.iter().map(|i| i.video_id.clone()).collect();
        assert_eq!(ids, [vid('A'), vid('C'), vid('E')]);
        assert_eq!(got.current_index, 2, "E is still current");
        assert_eq!(got.position, 42.5, "and keeps its second");
        // E, C, A in the old original order, as positions in the new queue.
        assert_eq!(got.original_order, Some(vec![2, 1, 0]));
        // Over-long text, or too many artists, drops the song; long but sane is kept.
        let mut s = saved_of("ABCD", 0);
        s.queue[1].title = "t".repeat(MAX_TEXT + 1);
        s.queue[2].artists = vec!["a".into(); MAX_ARTISTS + 1];
        s.queue[3].album = Some("b".repeat(MAX_TEXT + 1));
        s.queue[0].title = "t".repeat(MAX_TEXT);
        s.queue[0].artists = vec!["a".repeat(MAX_TEXT); MAX_ARTISTS];
        save(dir.path(), &s).unwrap();
        let ids: Vec<String> = load(dir.path())
            .unwrap()
            .queue
            .iter()
            .map(|i| i.video_id.clone())
            .collect();
        assert_eq!(ids, [vid('A')]);
        let mut s = saved_of("AB", 0);
        s.queue[1].artists[0] = "a".repeat(MAX_TEXT + 1);
        s.queue[0].playlist_id = Some("p".repeat(MAX_TEXT + 1));
        save(dir.path(), &s).unwrap();
        assert!(load(dir.path()).unwrap().queue.is_empty());
        // A song with no thumbnail at all is fine.
        let mut s = saved_of("A", 0);
        s.queue[0].thumbnail = None;
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap().queue.len(), 1);
    }

    #[test]
    fn a_dropped_current_song_moves_on_and_starts_at_zero() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = saved_of("ABC", 1);
        s.queue[1].video_id = "short".into();
        save(dir.path(), &s).unwrap();
        let got = load(dir.path()).unwrap();
        assert_eq!(got.queue[got.current_index].video_id, vid('C'));
        assert_eq!(got.position, 0.0, "the second was the dropped song's");

        // The last one dropped: the one before it.
        let mut s = saved_of("ABC", 2);
        s.queue[2].video_id = String::new();
        save(dir.path(), &s).unwrap();
        let got = load(dir.path()).unwrap();
        assert_eq!(got.queue[got.current_index].video_id, vid('B'));

        // All dropped: an empty queue (play then starts Liked songs), the rest kept.
        let mut s = saved_of("A", 0);
        s.queue[0].video_id = "x".into();
        save(dir.path(), &s).unwrap();
        let got = load(dir.path()).unwrap();
        assert!(got.queue.is_empty());
        assert_eq!(got.current_index, 0);
        assert_eq!(got.volume, 0.4);
    }

    #[test]
    fn loaded_values_are_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = saved_of("AB", 9);
        s.volume = 7.0;
        s.position = -3.0;
        s.source_playlist = Some("PL x\"y".into());
        s.continuation = Some("a".repeat(MAX_CONTINUATION + 1));
        // Shuffle on with a broken order: shuffle stays on, the order is the queue's.
        s.shuffle = true;
        s.original_order = Some(vec![0, 0]);
        save(dir.path(), &s).unwrap();
        let got = load(dir.path()).unwrap();
        assert_eq!(got.current_index, 0);
        assert_eq!(got.volume, 1.0);
        assert_eq!(got.position, 0.0);
        assert_eq!(got.source_playlist, None);
        assert_eq!(got.continuation, None);
        assert!(got.shuffle);
        assert_eq!(got.original_order, Some(vec![0, 1]));

        // Shuffle off: any order in the file is dropped.
        let mut s = saved_of("AB", 0);
        s.original_order = Some(vec![1, 0]);
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap().original_order, None);

        // An old or hand-written file over the cap: 500 songs around the current one, the
        // shuffled positions kept in step.
        let mut s = Saved {
            queue: (0..600).map(|i| song(&format!("{i:0>11}"))).collect(),
            current_index: 550,
            shuffle: true,
            original_order: Some((0..600).rev().collect()),
            ..saved_of("", 0)
        };
        s.position = 10.0;
        save(dir.path(), &s).unwrap();
        let got = load(dir.path()).unwrap();
        assert_eq!(got.queue.len(), MAX_ITEMS);
        assert_eq!(got.queue[0].video_id, format!("{:0>11}", 100));
        assert_eq!(got.current_index, 450);
        assert_eq!(
            got.queue[got.current_index].video_id,
            format!("{:0>11}", 550)
        );
        assert_eq!(got.position, 10.0);
        assert_eq!(got.original_order, Some((0..500).rev().collect()));

        // A song of unknown length: any second up to a day.
        let mut s = saved_of("A", 0);
        s.queue[0].length_seconds = 0;
        s.position = 1e9;
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap().position, 86_400.0);
        s.position = 5000.0;
        save(dir.path(), &s).unwrap();
        assert_eq!(load(dir.path()).unwrap().position, 5000.0);

        // NaN can't be written as JSON; a hand-edited huge number is clamped the same way.
        let mut v = serde_json::to_value(saved_of("A", 0)).unwrap();
        v["position"] = serde_json::json!(1e300);
        v["volume"] = serde_json::json!(-1.0);
        std::fs::write(dir.path().join(FILE_NAME), v.to_string()).unwrap();
        let got = load(dir.path()).unwrap();
        assert_eq!(got.position, 0.0, "past any song's end");
        assert_eq!(got.volume, 0.0);
    }

    #[test]
    fn window_keeps_500_around_the_current_song() {
        assert_eq!(window(3, 1, 500), 0..3);
        assert_eq!(window(500, 499, 500), 0..500);
        assert_eq!(window(1200, 600, 500), 350..850);
        assert_eq!(window(1200, 10, 500), 0..500);
        assert_eq!(window(1200, 1199, 500), 700..1200);
        assert_eq!(window(0, 0, 500), 0..0);
    }

    #[test]
    fn a_symlinked_state_file_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let target = elsewhere.path().join("target.json");
        std::fs::write(&target, serde_json::to_string(&saved_of("A", 0)).unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join(FILE_NAME)).unwrap();
        assert_eq!(load(dir.path()), None);
        // A save replaces the link itself, never writes through it.
        save(dir.path(), &saved_of("B", 0)).unwrap();
        let still: Saved =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        assert_eq!(still.queue[0].video_id, vid('A'));
        assert_eq!(load(dir.path()).unwrap().queue[0].video_id, vid('B'));
    }

    /// A recorder for `Writer::with`: every snapshot it was asked to save, optionally held up
    /// until the test lets it go.
    #[derive(Clone, Default)]
    struct Recorder {
        saves: Arc<Mutex<Vec<Saved>>>,
        gate: Arc<(Mutex<bool>, Condvar)>,
    }

    impl Recorder {
        fn gated() -> Recorder {
            let r = Recorder::default();
            *r.gate.0.lock().unwrap() = true;
            r
        }
        fn open(&self) {
            *self.gate.0.lock().unwrap() = false;
            self.gate.1.notify_all();
        }
        fn save_fn(&self) -> SaveFn {
            let r = self.clone();
            Box::new(move |s| {
                let mut shut = r.gate.0.lock().unwrap();
                while *shut {
                    shut = r.gate.1.wait(shut).unwrap();
                }
                r.saves.lock().unwrap().push(s.clone());
                Ok(())
            })
        }
        fn positions(&self) -> Vec<f64> {
            self.saves
                .lock()
                .unwrap()
                .iter()
                .map(|s| s.position)
                .collect()
        }
    }

    fn at(position: f64) -> Saved {
        Saved {
            position,
            ..Saved::default()
        }
    }

    #[tokio::test]
    async fn writer_coalesces_to_the_latest() {
        let rec = Recorder::gated();
        let w = Writer::with(rec.save_fn());
        w.submit(at(1.0));
        // Give the thread time to take the first one and block in it.
        std::thread::sleep(Duration::from_millis(50));
        w.submit(at(2.0));
        w.submit(at(3.0));
        w.submit(at(4.0));
        rec.open();
        assert!(w.finish(at(5.0), Duration::from_secs(5)).await);
        let got = rec.positions();
        // The first, then only the latest waiting one: never 2 or 3 or 4 on their own.
        assert_eq!(got.first(), Some(&1.0));
        assert_eq!(got.last(), Some(&5.0));
        assert!(got.len() <= 3, "{got:?}");
        assert!(!got.contains(&2.0) && !got.contains(&3.0), "{got:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn finish_is_bounded_when_the_disk_hangs() {
        // A save that never returns (a hung disk): quitting must not wait for it.
        let rec = Recorder::gated();
        let w = Writer::with(rec.save_fn());
        let t = tokio::time::Instant::now();
        assert!(!w.finish(at(1.0), Duration::from_secs(2)).await);
        assert_eq!(t.elapsed(), Duration::from_secs(2));
        rec.open();
    }

    #[tokio::test]
    async fn spawn_writes_into_the_folder() {
        let dir = tempfile::tempdir().unwrap();
        let w = Writer::spawn(dir.path().to_path_buf());
        w.submit(saved_of("AB", 1));
        assert!(w.finish(saved_of("ABC", 2), Duration::from_secs(5)).await);
        assert_eq!(load(dir.path()), Some(saved_of("ABC", 2)));
    }
}
