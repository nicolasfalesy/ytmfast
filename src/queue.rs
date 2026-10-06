//! The play queue: order, shuffle, repeat and stable queue ids. Pure: no I/O, no clock (except
//! the one seed `Queue::new` reads), so every rule here is unit-tested.
//!
//! The queue only says which item is current; the engine decides what to play. Items carry a
//! queue id (`u64`, from 1, never reused while the daemon runs) so the widgets can jump to,
//! remove or move a song by id whatever the order is, and a song queued twice is still two
//! different items.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::innertube::SongItem;

/// How many of the queue's last items a radio page is checked against. Consecutive radio
/// pages overlap by a few songs; a song further back than this may come round again.
const RADIO_OVERLAP_WINDOW: usize = 50;

/// Previous restarts the song once the position is more than this far in (global constraint).
const RESTART_AFTER_SECONDS: f64 = 3.0;

/// The most songs the live queue holds (ruling S15). The whole queue goes to every widget on
/// every change as one line, and the socket's lines stop at 1 MiB: 1,000 songs at about
/// 375 bytes each is about 375 KB. An add that would pass it is refused; a refill drops
/// played songs first and stops at it.
pub const MAX_ITEMS: usize = 1_000;

/// Played songs a refill keeps behind the current one when it makes room, so Previous and
/// the widget's list still reach back a while.
pub const KEEP_PLAYED: usize = 50;

/// One song in the queue, with the id the widgets use to jump to it, remove it or move it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: u64,
    pub song: SongItem,
}

/// Repeat mode; on the wire `"off" | "all" | "one"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// Where `add` puts new songs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AddAt {
    /// Right after the current item.
    Next,
    /// At the end.
    End,
}

/// What `previous` decided.
#[derive(Debug, PartialEq, Eq)]
pub enum Previous<'a> {
    /// Play the current song again from the start (the current item is unchanged).
    Restart,
    /// This item is now current.
    Item(&'a QueueItem),
}

/// The shuffle's random numbers: SplitMix64. Shuffle only has to look random to a listener,
/// not resist anyone guessing it, so a cryptographic generator (and a crate for one) would buy
/// nothing; a fixed seed also makes the tests' shuffles repeatable.
#[derive(Debug, Clone)]
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A number in `0..n` (n > 0), by multiply-shift: its bias is at most n / 2^64, which is
    /// nothing for a queue, and it needs no rejection loop.
    fn below(&mut self, n: usize) -> usize {
        ((u128::from(self.next_u64()) * n as u128) >> 64) as usize
    }

    /// Fisher-Yates.
    fn shuffle<T>(&mut self, slice: &mut [T]) {
        for i in (1..slice.len()).rev() {
            let j = self.below(i + 1);
            slice.swap(i, j);
        }
    }
}

/// The queue. `Clone` is for snapshots (events, saving): a clone keeps the id counter, so only
/// one copy may go on adding songs, or ids would repeat.
#[derive(Debug, Clone)]
pub struct Queue {
    /// The items in play order (the shuffled order while shuffle is on).
    items: Vec<QueueItem>,
    /// Index into `items`. `None` only when the queue is empty or no song was picked yet (songs
    /// added to an empty queue).
    current: Option<usize>,
    /// While shuffle is on: every item's id in the order from before shuffling, kept up to date
    /// by add, remove and radio, so turning shuffle off restores it. `None` while shuffle is
    /// off, when `items` itself is that order.
    original: Option<Vec<u64>>,
    repeat: Repeat,
    next_id: u64,
    rng: Rng,
}

impl Default for Queue {
    fn default() -> Self {
        Self::new()
    }
}

impl Queue {
    /// An empty queue whose shuffle is seeded from the clock.
    pub fn new() -> Self {
        // The clock's nanoseconds differ between daemon starts, which is all a shuffle seed needs
        // (see `Rng`). A clock before 1970 just gives seed 0, which SplitMix64 handles fine.
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self::with_seed(seed)
    }

    /// An empty queue with a fixed shuffle seed (tests).
    pub fn with_seed(seed: u64) -> Self {
        Queue {
            items: Vec::new(),
            current: None,
            original: None,
            repeat: Repeat::Off,
            next_id: 1,
            rng: Rng(seed),
        }
    }

    /// Replaces the whole queue with new items (new ids) and makes `songs[start_index]` current.
    /// A start past the end (a stale index) starts at the first song. With shuffle on, the new
    /// queue is shuffled with the start song first. A list over `MAX_ITEMS` keeps that many
    /// around the start: `KEEP_PLAYED` before it and the rest after, reaching further back
    /// when the list ends sooner.
    pub fn replace(&mut self, mut songs: Vec<SongItem>, start_index: usize) -> Option<&QueueItem> {
        let mut start_index = start_index;
        if songs.len() > MAX_ITEMS {
            let start = if start_index < songs.len() {
                start_index
            } else {
                0
            };
            let from = start
                .saturating_sub(KEEP_PLAYED)
                .min(songs.len() - MAX_ITEMS);
            songs.truncate(from + MAX_ITEMS);
            songs.drain(..from);
            start_index = start - from;
        }
        let items = self.new_items(songs);
        self.current = match items.len() {
            0 => None,
            n if start_index < n => Some(start_index),
            _ => Some(0),
        };
        if self.original.is_some() {
            self.original = Some(items.iter().map(|i| i.id).collect());
            self.items = items;
            self.shuffle_items();
        } else {
            self.items = items;
        }
        self.current()
    }

    /// A queue from a saved state (`crate::state`): `songs` in play order with new ids,
    /// `current` an index into them, and while shuffled `original` the order from before
    /// shuffling as indexes into `songs`. An index past the end means no current song yet;
    /// an `original` that isn't every index once falls back to the play order (shuffle stays
    /// on).
    pub fn restore(
        songs: Vec<SongItem>,
        current: Option<usize>,
        original: Option<Vec<usize>>,
        repeat: Repeat,
    ) -> Queue {
        let mut q = Queue::new();
        q.items = q.new_items(songs);
        q.current = current.filter(|&c| c < q.items.len());
        q.repeat = repeat;
        q.original = original.map(|order| {
            let mut seen = vec![false; q.items.len()];
            let valid = order.len() == q.items.len()
                && order
                    .iter()
                    .all(|&p| p < seen.len() && !std::mem::replace(&mut seen[p], true));
            if valid {
                order.iter().map(|&p| q.items[p].id).collect()
            } else {
                q.items.iter().map(|i| i.id).collect()
            }
        });
        q
    }

    /// While shuffled: the order from before shuffling, as indexes into `items()` (what
    /// `restore` takes back). `None` while shuffle is off.
    pub fn original_positions(&self) -> Option<Vec<usize>> {
        let original = self.original.as_ref()?;
        let at: HashMap<u64, usize> = self
            .items
            .iter()
            .enumerate()
            .map(|(p, i)| (i.id, p))
            .collect();
        Some(
            original
                .iter()
                .filter_map(|id| at.get(id).copied())
                .collect(),
        )
    }

    pub fn current(&self) -> Option<&QueueItem> {
        self.current.and_then(|c| self.items.get(c))
    }

    /// The current item's index in `items()`.
    pub fn current_index(&self) -> Option<usize> {
        self.current
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The items in play order (shuffled while shuffle is on).
    pub fn items(&self) -> &[QueueItem] {
        &self.items
    }

    pub fn shuffle(&self) -> bool {
        self.original.is_some()
    }

    pub fn repeat(&self) -> Repeat {
        self.repeat
    }

    /// Moves on and returns the new current item, or `None` at the end (Repeat::Off), leaving
    /// the current item as it was. `auto` means the song ended by itself: only then does
    /// Repeat::One play it again; a skip moves on as if One were Off. With no current item yet,
    /// the first item becomes current.
    pub fn next(&mut self, auto: bool) -> Option<&QueueItem> {
        let next = self.next_index(auto)?;
        self.current = Some(next);
        self.items.get(next)
    }

    /// The item `next(auto)` would make current, without moving: the engine prefetches its
    /// link.
    pub fn peek_next(&self, auto: bool) -> Option<&QueueItem> {
        self.next_index(auto).and_then(|n| self.items.get(n))
    }

    fn next_index(&self, auto: bool) -> Option<usize> {
        let len = self.items.len();
        if len == 0 {
            return None;
        }
        match self.current {
            None => Some(0),
            Some(c) if auto && self.repeat == Repeat::One => Some(c),
            Some(c) if c + 1 < len => Some(c + 1),
            Some(_) if self.repeat == Repeat::All => Some(0),
            Some(_) => None,
        }
    }

    /// Previous: restart when more than 3 s in; else the item before, if there is one
    /// (Repeat::All wraps from the first to the last); else restart.
    pub fn previous(&mut self, position: f64) -> Previous<'_> {
        if position > RESTART_AFTER_SECONDS {
            return Previous::Restart;
        }
        let Some(c) = self.current else {
            return Previous::Restart;
        };
        let prev = match c {
            0 if self.repeat == Repeat::All => self.items.len() - 1,
            0 => return Previous::Restart,
            c => c - 1,
        };
        // A one-item queue with Repeat::All wraps onto itself: that's a restart.
        if prev == c {
            return Previous::Restart;
        }
        self.current = Some(prev);
        Previous::Item(&self.items[prev])
    }

    /// Makes the item with this id current.
    pub fn jump(&mut self, id: u64) -> Option<&QueueItem> {
        let i = self.index_of(id)?;
        self.current = Some(i);
        self.items.get(i)
    }

    /// Removes the item with this id. If it was current, the item after it becomes current (or
    /// the one before, if it was last; nothing, if it was the only one); what to play then is
    /// the engine's call.
    pub fn remove(&mut self, id: u64) -> bool {
        let Some(p) = self.index_of(id) else {
            return false;
        };
        self.items.remove(p);
        if let Some(original) = &mut self.original {
            original.retain(|&x| x != id);
        }
        self.current = match self.current {
            Some(c) if p < c => Some(c - 1),
            Some(c) if p == c => match self.items.len() {
                0 => None,
                n if p < n => Some(p),
                _ => Some(p - 1),
            },
            other => other,
        };
        true
    }

    /// Adds songs as new items, in the given order. `Next` puts them right after the current
    /// item, in the play order and (while shuffled) in the original order too, so they still
    /// come next after shuffle is turned off. `End` appends them; while shuffled, only to the
    /// original order, and the play order gets them shuffled into the songs still to come. False, with nothing added, when they would take
    /// the queue past `MAX_ITEMS`: the user asked for these songs, so none are dropped
    /// quietly, and played songs are not dropped to make room either.
    pub fn add(&mut self, songs: Vec<SongItem>, at: AddAt) -> bool {
        if self.items.len() + songs.len() > MAX_ITEMS {
            return false;
        }
        self.insert(songs, at);
        true
    }

    fn insert(&mut self, songs: Vec<SongItem>, at: AddAt) {
        let items = self.new_items(songs);
        let ids = items.iter().map(|i| i.id);
        match at {
            AddAt::End => match &mut self.original {
                // Shuffled: the original order appends them; the play order shuffles them
                // into the songs still to come (ruling S10, the user's pick), so a later page
                // of Liked songs or a radio refill isn't heard as one unshuffled run at the end.
                Some(original) => {
                    original.extend(ids);
                    self.shuffle_in(items);
                }
                None => self.items.extend(items),
            },
            AddAt::Next => {
                let current_id = self.current().map(|i| i.id);
                if let Some(original) = &mut self.original {
                    let at = current_id
                        .and_then(|cid| original.iter().position(|&x| x == cid))
                        .map_or(0, |p| p + 1);
                    original.splice(at..at, ids);
                }
                let at = self.current.map_or(0, |c| c + 1);
                self.items.splice(at..at, items);
            }
        }
    }

    /// Moves the item with this id to `index` in the play order (past the end means last).
    /// The current item stays current. While shuffled this reorders only the shuffled order:
    /// turning shuffle off goes back to the original order.
    pub fn move_to(&mut self, id: u64, index: usize) -> bool {
        let Some(p) = self.index_of(id) else {
            return false;
        };
        let current_id = self.current().map(|i| i.id);
        let item = self.items.remove(p);
        let to = index.min(self.items.len());
        self.items.insert(to, item);
        self.current = current_id.and_then(|cid| self.index_of(cid));
        true
    }

    /// On: the items are shuffled, the current one first. Off: back to the original order, at
    /// the same current item. Setting the mode it already has changes nothing.
    pub fn set_shuffle(&mut self, on: bool) {
        match (on, self.original.take()) {
            (true, None) => {
                self.original = Some(self.items.iter().map(|i| i.id).collect());
                self.shuffle_items();
            }
            (false, Some(original)) => {
                let current_id = self.current().map(|i| i.id);
                let rank: HashMap<u64, usize> = original
                    .iter()
                    .enumerate()
                    .map(|(r, &id)| (id, r))
                    .collect();
                // Every item is in `original` (add, remove and radio keep them in step); the
                // fallback only keeps a broken invariant from panicking.
                self.items
                    .sort_by_key(|i| rank.get(&i.id).copied().unwrap_or(usize::MAX));
                self.current = current_id.and_then(|cid| self.index_of(cid));
            }
            (_, kept) => self.original = kept,
        }
    }

    pub fn set_repeat(&mut self, repeat: Repeat) {
        self.repeat = repeat;
    }

    /// True when the queue is about to run out: Repeat::Off and 2 or fewer items after the
    /// current one. The engine then fetches more radio songs. Never with no current item
    /// (an empty queue, or songs added to one): nothing plays, so nothing is about to run
    /// out, and a radio fetched then would be for a queue the user may never play.
    pub fn needs_more(&self) -> bool {
        let Some(c) = self.current else {
            return false;
        };
        self.repeat == Repeat::Off && self.items.len() - c - 1 <= 2
    }

    /// Appends a radio page (or a list's next page) at the end of the original order; while
    /// shuffled, the play order gets it shuffled into the songs still to come (ruling S10).
    /// Songs already among the last 50 items appended are skipped, because consecutive radio
    /// pages overlap. Returns how many were added.
    ///
    /// At `MAX_ITEMS`: first the played songs (before the current one in play order) are
    /// dropped from the front, keeping `KEEP_PLAYED`; then only what fits is appended. A refill
    /// comes only when 2 or fewer songs are left, so in practice there is always room.
    pub fn append_radio(&mut self, songs: Vec<SongItem>) -> usize {
        // The last items in the order songs were appended in: the original order while shuffled.
        let mut seen: HashSet<String> = match &self.original {
            Some(original) => {
                let tail: HashSet<u64> = original
                    [original.len().saturating_sub(RADIO_OVERLAP_WINDOW)..]
                    .iter()
                    .copied()
                    .collect();
                self.items
                    .iter()
                    .filter(|i| tail.contains(&i.id))
                    .map(|i| i.song.video_id.clone())
                    .collect()
            }
            None => self.items[self.items.len().saturating_sub(RADIO_OVERLAP_WINDOW)..]
                .iter()
                .map(|i| i.song.video_id.clone())
                .collect(),
        };
        // `seen` also takes each new song, so a page that repeats a song adds it once.
        let mut fresh: Vec<SongItem> = songs
            .into_iter()
            .filter(|s| seen.insert(s.video_id.clone()))
            .collect();
        if self.items.len() + fresh.len() > MAX_ITEMS {
            self.drop_played();
        }
        fresh.truncate(MAX_ITEMS.saturating_sub(self.items.len()));
        let added = fresh.len();
        self.insert(fresh, AddAt::End);
        added
    }

    /// Drops the played songs (those before the current one in play order) but the last
    /// `KEEP_PLAYED`, from both orders.
    fn drop_played(&mut self) {
        let Some(c) = self.current else {
            return;
        };
        let drop = c.saturating_sub(KEEP_PLAYED);
        if drop == 0 {
            return;
        }
        let gone: HashSet<u64> = self.items.drain(..drop).map(|i| i.id).collect();
        if let Some(original) = &mut self.original {
            original.retain(|id| !gone.contains(id));
        }
        self.current = Some(c - drop);
    }

    fn index_of(&self, id: u64) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    fn new_items(&mut self, songs: Vec<SongItem>) -> Vec<QueueItem> {
        songs
            .into_iter()
            .map(|song| {
                let id = self.next_id;
                self.next_id += 1;
                QueueItem { id, song }
            })
            .collect()
    }

    /// Shuffles `new` into the items after the current one (all items, with no current one):
    /// `new` is shuffled, then merged at random with the items to come, which keep their order
    /// among themselves. Each step takes from either side in proportion to what is left on it,
    /// which makes every interleaving equally likely. Keeping the order of the songs to come
    /// keeps the next song (and its preload) in place unless a new song lands before it.
    fn shuffle_in(&mut self, mut new: Vec<QueueItem>) {
        self.rng.shuffle(&mut new);
        let start = self.current.map_or(0, |c| c + 1);
        let to_come: Vec<QueueItem> = self.items.drain(start..).collect();
        let mut to_come = to_come.into_iter().peekable();
        let mut new = new.into_iter().peekable();
        let (mut a, mut b) = (to_come.len(), new.len());
        self.items.reserve(a + b);
        while a + b > 0 {
            let item = if self.rng.below(a + b) < a {
                a -= 1;
                to_come.next()
            } else {
                b -= 1;
                new.next()
            };
            self.items.extend(item);
        }
    }

    /// Shuffles `items` with the current item first, and makes index 0 current. With no current
    /// item, everything is shuffled.
    fn shuffle_items(&mut self) {
        let rest = match self.current {
            Some(c) => {
                // The swap disturbs the order of the rest, which is shuffled next anyway.
                self.items.swap(0, c);
                self.current = Some(0);
                1
            }
            None => 0,
        };
        self.rng.shuffle(&mut self.items[rest..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(v: &str) -> SongItem {
        SongItem {
            video_id: v.to_string(),
            title: format!("title {v}"),
            ..Default::default()
        }
    }

    /// Songs "s0", "s1", ... "s{n-1}".
    fn songs(n: usize) -> Vec<SongItem> {
        (0..n).map(|i| song(&format!("s{i}"))).collect()
    }

    fn vids(q: &Queue) -> Vec<String> {
        q.items().iter().map(|i| i.song.video_id.clone()).collect()
    }

    fn vid_list(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn cur(q: &Queue) -> Option<String> {
        q.current().map(|i| i.song.video_id.clone())
    }

    fn id_of(q: &Queue, v: &str) -> u64 {
        q.items().iter().find(|i| i.song.video_id == v).unwrap().id
    }

    fn queue(n: usize, start: usize) -> Queue {
        let mut q = Queue::with_seed(7);
        q.replace(songs(n), start);
        q
    }

    #[test]
    fn ids_are_stable_and_unique() {
        let mut q = Queue::with_seed(1);
        let first = q.replace(songs(3), 0).map(|i| i.id);
        assert_eq!(first, Some(1), "ids start at 1");
        let ids: Vec<u64> = q.items().iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);

        // A new queue never reuses an id from the old one.
        q.replace(songs(2), 0);
        let ids: Vec<u64> = q.items().iter().map(|i| i.id).collect();
        assert_eq!(ids, vec![4, 5]);
        q.add(vec![song("x")], AddAt::End);
        q.add(vec![song("y")], AddAt::Next);
        q.append_radio(vec![song("r")]);
        assert_eq!(id_of(&q, "x"), 6);
        assert_eq!(id_of(&q, "y"), 7);
        assert_eq!(id_of(&q, "r"), 8);

        // Shuffle, unshuffle and move keep every song's id.
        let before: Vec<(u64, String)> = q
            .items()
            .iter()
            .map(|i| (i.id, i.song.video_id.clone()))
            .collect();
        q.set_shuffle(true);
        q.move_to(id_of(&q, "s1"), 0);
        q.set_shuffle(false);
        for (id, v) in &before {
            assert_eq!(id_of(&q, v), *id, "{v} kept its id");
        }
        // A removed id is gone for good: removing it again fails, and new songs get new ids.
        assert!(q.remove(6));
        assert!(!q.remove(6));
        q.add(vec![song("z")], AddAt::End);
        assert_eq!(id_of(&q, "z"), 9);
    }

    #[test]
    fn restore_keeps_the_shuffled_and_the_original_order() {
        let mut q = queue(8, 3);
        q.set_shuffle(true);
        q.set_repeat(Repeat::All);
        q.next(false);
        let played: Vec<SongItem> = q.items().iter().map(|i| i.song.clone()).collect();
        let original = q.original_positions();
        let mut back = Queue::restore(played, q.current_index(), original, q.repeat());
        assert_eq!(vids(&back), vids(&q), "the shuffled order");
        assert_eq!(cur(&back), cur(&q));
        assert!(back.shuffle());
        assert_eq!(back.repeat(), Repeat::All);
        back.set_shuffle(false);
        q.set_shuffle(false);
        assert_eq!(vids(&back), vids(&q), "the original order");
        assert_eq!(vids(&back), vids(&queue(8, 0)));
        assert_eq!(cur(&back), cur(&q));
        // New ids from 1, so later songs never collide with them.
        let ids: Vec<u64> = back.items().iter().map(|i| i.id).collect();
        assert_eq!(ids.iter().min(), Some(&1));
        back.add(vec![song("x")], AddAt::End);
        assert_eq!(id_of(&back, "x"), 9);

        // A broken order keeps shuffle on, in the play order; a bad index means no current.
        let b = Queue::restore(songs(3), Some(7), Some(vec![0, 0, 1]), Repeat::Off);
        assert!(b.shuffle());
        assert_eq!(b.original_positions(), Some(vec![0, 1, 2]));
        assert_eq!(b.current(), None);
        let off = Queue::restore(songs(2), Some(1), None, Repeat::One);
        assert!(!off.shuffle());
        assert_eq!(off.original_positions(), None);
        assert_eq!(cur(&off).as_deref(), Some("s1"));
    }

    #[test]
    fn remove_by_id_after_shuffle() {
        let mut q = queue(10, 0);
        q.set_shuffle(true);
        assert_ne!(vids(&q), vids(&queue(10, 0)), "the seed shuffles");

        let target = id_of(&q, "s5");
        assert!(q.remove(target));
        assert_eq!(q.len(), 9);
        assert!(!vids(&q).contains(&"s5".to_string()));
        assert_eq!(cur(&q).as_deref(), Some("s0"), "current untouched");

        // Jump by id lands on the right song whatever the order.
        let j = id_of(&q, "s7");
        assert_eq!(
            q.jump(j).map(|i| i.song.video_id.clone()).as_deref(),
            Some("s7")
        );
        assert_eq!(cur(&q).as_deref(), Some("s7"));
        assert!(q.jump(target).is_none(), "a removed id can't be jumped to");

        // The removal also holds in the original order.
        q.set_shuffle(false);
        assert_eq!(
            vids(&q),
            vid_list(&["s0", "s1", "s2", "s3", "s4", "s6", "s7", "s8", "s9"])
        );
        assert_eq!(cur(&q).as_deref(), Some("s7"));
    }

    #[test]
    fn shuffle_keeps_current_first_and_unshuffle_restores_order() {
        let mut q = queue(10, 4);
        q.set_shuffle(true);
        assert!(q.shuffle());
        assert_eq!(q.current_index(), Some(0));
        assert_eq!(cur(&q).as_deref(), Some("s4"));
        let mut sorted = vids(&q);
        sorted.sort();
        assert_eq!(sorted, {
            let mut o = vids(&queue(10, 4));
            o.sort();
            o
        });
        assert_ne!(vids(&q), vids(&queue(10, 4)));

        // The same seed gives the same order (the tests rely on it).
        let mut again = queue(10, 4);
        again.set_shuffle(true);
        assert_eq!(vids(&q), vids(&again));

        // Turning it on again doesn't reshuffle.
        let shuffled = vids(&q);
        q.set_shuffle(true);
        assert_eq!(vids(&q), shuffled);

        // Play on two songs, then turn shuffle off: the original order, at the song now playing.
        q.next(false);
        q.next(false);
        let playing = cur(&q).unwrap();
        q.set_shuffle(false);
        assert!(!q.shuffle());
        assert_eq!(vids(&q), vids(&queue(10, 4)));
        assert_eq!(cur(&q).as_deref(), Some(playing.as_str()));
        let idx: usize = playing[1..].parse().unwrap();
        assert_eq!(q.current_index(), Some(idx));
    }

    #[test]
    fn repeat_all_wraps() {
        let mut q = queue(3, 2);
        q.set_repeat(Repeat::All);
        assert_eq!(q.repeat(), Repeat::All);
        assert_eq!(
            q.next(true).map(|i| i.song.video_id.clone()).as_deref(),
            Some("s0")
        );
        q.jump(id_of(&q, "s2"));
        assert_eq!(
            q.next(false).map(|i| i.song.video_id.clone()).as_deref(),
            Some("s0")
        );
        // Previous at the first item wraps to the last.
        match q.previous(0.5) {
            Previous::Item(i) => assert_eq!(i.song.video_id, "s2"),
            other => panic!("expected the last item, got {other:?}"),
        }
        assert_eq!(cur(&q).as_deref(), Some("s2"));

        // Off: the end is the end, and the current song stays.
        q.set_repeat(Repeat::Off);
        assert!(q.next(true).is_none());
        assert!(q.next(false).is_none());
        assert_eq!(cur(&q).as_deref(), Some("s2"));
    }

    #[test]
    fn repeat_one_repeats_on_auto_only() {
        let mut q = queue(3, 1);
        q.set_repeat(Repeat::One);
        let id = q.current().unwrap().id;
        assert_eq!(
            q.next(true).map(|i| i.id),
            Some(id),
            "the song ended: again"
        );
        assert_eq!(q.next(true).map(|i| i.id), Some(id));
        // A skip moves on.
        assert_eq!(
            q.next(false).map(|i| i.song.video_id.clone()).as_deref(),
            Some("s2")
        );
        // A skip at the end with One stops, like Off.
        assert!(q.next(false).is_none());
        assert_eq!(cur(&q).as_deref(), Some("s2"));
        // The song ending still repeats it.
        assert_eq!(
            q.next(true).map(|i| i.song.video_id.clone()).as_deref(),
            Some("s2")
        );
    }

    #[test]
    fn previous_restarts_after_3_s() {
        let mut q = queue(3, 2);
        assert_eq!(q.previous(3.5), Previous::Restart);
        assert_eq!(cur(&q).as_deref(), Some("s2"), "a restart doesn't move");
        // Exactly 3 s is not "more than 3 s in".
        match q.previous(3.0) {
            Previous::Item(i) => assert_eq!(i.song.video_id, "s1"),
            other => panic!("expected s1, got {other:?}"),
        }
        match q.previous(0.0) {
            Previous::Item(i) => assert_eq!(i.song.video_id, "s0"),
            other => panic!("expected s0, got {other:?}"),
        }
        // At the first item with Repeat::Off there is nothing before: restart.
        assert_eq!(q.previous(1.0), Previous::Restart);
        assert_eq!(cur(&q).as_deref(), Some("s0"));
        // A one-song queue with Repeat::All wraps onto itself: that's a restart too.
        let mut one = queue(1, 0);
        one.set_repeat(Repeat::All);
        assert_eq!(one.previous(1.0), Previous::Restart);
        // An empty queue: nothing to go back to.
        assert_eq!(Queue::with_seed(1).previous(1.0), Previous::Restart);
    }

    #[test]
    fn add_next_and_end() {
        let mut q = queue(4, 1);
        q.add(vec![song("x"), song("y")], AddAt::Next);
        assert_eq!(vids(&q), vid_list(&["s0", "s1", "x", "y", "s2", "s3"]));
        q.add(vec![song("z")], AddAt::End);
        assert_eq!(vids(&q), vid_list(&["s0", "s1", "x", "y", "s2", "s3", "z"]));
        assert_eq!(
            cur(&q).as_deref(),
            Some("s1"),
            "adding doesn't change the song"
        );
        assert_eq!(
            q.next(false).map(|i| i.song.video_id.clone()).as_deref(),
            Some("x")
        );

        // While shuffled: next in the shuffled order, and right after the current song's
        // original place once shuffle is off.
        let mut q = queue(6, 2);
        q.set_shuffle(true);
        q.next(false);
        let playing = cur(&q).unwrap();
        q.add(vec![song("n")], AddAt::Next);
        q.add(vec![song("e")], AddAt::End);
        let i = q.current_index().unwrap();
        assert_eq!(q.items()[i + 1].song.video_id, "n");
        // `End` while shuffled: somewhere after the current song (shuffled in, ruling S10).
        let e = vids(&q).iter().position(|v| v == "e").unwrap();
        assert!(e > i + 1, "{:?}", vids(&q));
        q.set_shuffle(false);
        let order = vids(&q);
        let p = order.iter().position(|v| *v == playing).unwrap();
        assert_eq!(order[p + 1], "n");
        assert_eq!(order.last().unwrap(), "e");
        assert_eq!(q.len(), 8);

        // Adding to an empty queue doesn't pick a song; next() starts at the top.
        let mut q = Queue::with_seed(1);
        q.add(vec![song("a"), song("b")], AddAt::End);
        assert!(q.current().is_none());
        assert_eq!(
            q.next(false).map(|i| i.song.video_id.clone()).as_deref(),
            Some("a")
        );
    }

    #[test]
    fn move_to_reorders() {
        let mut q = queue(4, 1);
        assert!(q.move_to(id_of(&q, "s0"), 3));
        assert_eq!(vids(&q), vid_list(&["s1", "s2", "s3", "s0"]));
        assert_eq!(cur(&q).as_deref(), Some("s1"), "current follows its song");
        assert_eq!(q.current_index(), Some(0));

        // Moving the current song itself.
        assert!(q.move_to(id_of(&q, "s1"), 2));
        assert_eq!(vids(&q), vid_list(&["s2", "s3", "s1", "s0"]));
        assert_eq!(q.current_index(), Some(2));

        // Past the end clamps to the last place.
        assert!(q.move_to(id_of(&q, "s2"), 99));
        assert_eq!(vids(&q), vid_list(&["s3", "s1", "s0", "s2"]));
        assert_eq!(q.current_index(), Some(1));

        // Moving a song in front of the current one shifts the current index.
        assert!(q.move_to(id_of(&q, "s2"), 0));
        assert_eq!(vids(&q), vid_list(&["s2", "s3", "s1", "s0"]));
        assert_eq!(q.current_index(), Some(2));

        assert!(!q.move_to(999, 0), "an unknown id");
    }

    #[test]
    fn needs_more_near_end() {
        assert!(!queue(5, 0).needs_more());
        assert!(!queue(5, 1).needs_more(), "3 left");
        assert!(queue(5, 2).needs_more(), "2 left");
        assert!(queue(5, 4).needs_more(), "none left");
        assert!(!Queue::with_seed(1).needs_more(), "empty: nothing plays");
        // Songs added to an empty queue: none is current until a play, and nothing is
        // fetched for a queue that isn't playing.
        let mut q = Queue::with_seed(1);
        assert!(q.add(vec![song("a")], AddAt::End));
        assert_eq!(q.current(), None);
        assert!(!q.needs_more(), "no current song");

        let mut q = queue(5, 4);
        q.set_repeat(Repeat::All);
        assert!(!q.needs_more(), "repeat all never runs out");
        q.set_repeat(Repeat::One);
        assert!(!q.needs_more(), "only Off asks for radio");
    }

    #[test]
    fn remove_current_moves_to_next() {
        let mut q = queue(3, 1);
        assert!(q.remove(id_of(&q, "s1")));
        assert_eq!(cur(&q).as_deref(), Some("s2"));
        assert_eq!(q.current_index(), Some(1));

        // The last item: the one before it becomes current.
        assert!(q.remove(id_of(&q, "s2")));
        assert_eq!(cur(&q).as_deref(), Some("s0"));

        // The only item: nothing is current.
        assert!(q.remove(id_of(&q, "s0")));
        assert!(q.current().is_none());
        assert!(q.is_empty());
        assert!(q.next(false).is_none());

        // Removing a song before the current one keeps the current song.
        let mut q = queue(4, 2);
        assert!(q.remove(id_of(&q, "s0")));
        assert_eq!(cur(&q).as_deref(), Some("s2"));
        assert_eq!(q.current_index(), Some(1));
    }

    #[test]
    fn replace_starts_at_the_index_and_resets_shuffle_order() {
        let mut q = Queue::with_seed(3);
        assert!(q.replace(Vec::new(), 0).is_none());
        assert!(q.current().is_none());
        assert_eq!(
            q.replace(songs(3), 2)
                .map(|i| i.song.video_id.clone())
                .as_deref(),
            Some("s2")
        );
        // A start past the end (a stale index) starts at the top.
        assert_eq!(
            q.replace(songs(3), 9)
                .map(|i| i.song.video_id.clone())
                .as_deref(),
            Some("s0")
        );
        // With shuffle on, a new queue comes shuffled with the start song first, and
        // turning shuffle off gives its own original order.
        q.set_shuffle(true);
        let first = q.replace(songs(8), 5).map(|i| i.song.video_id.clone());
        assert_eq!(first.as_deref(), Some("s5"));
        assert_eq!(q.current_index(), Some(0));
        assert!(q.shuffle());
        q.set_shuffle(false);
        assert_eq!(vids(&q), vids(&queue(8, 0)));
        assert_eq!(q.current_index(), Some(5));
    }

    #[test]
    fn append_radio_skips_overlap_and_goes_last_in_the_original_order() {
        let mut q = queue(3, 0);
        let n = q.append_radio(vec![song("r1"), song("r2")]);
        assert_eq!(n, 2);
        // The next page overlaps the last one; songs already near the end are skipped, and a
        // page that repeats a song within itself adds it once.
        let n = q.append_radio(vec![song("r2"), song("r3"), song("s2"), song("r3")]);
        assert_eq!(n, 1);
        assert_eq!(vids(&q), vid_list(&["s0", "s1", "s2", "r1", "r2", "r3"]));
        assert_eq!(q.append_radio(Vec::new()), 0);

        // Shuffled: radio songs go after the current song in the play order (shuffled in,
        // `later_pages_shuffle_into_the_songs_to_come`) and at the end of the original order.
        let mut q = queue(6, 0);
        q.set_shuffle(true);
        q.append_radio(vec![song("r1"), song("r2")]);
        let v = vids(&q);
        assert_eq!(v[0], "s0");
        assert!(v[1..].contains(&"r1".to_string()) && v[1..].contains(&"r2".to_string()));
        q.set_shuffle(false);
        assert_eq!(
            vids(&q),
            vid_list(&["s0", "s1", "s2", "s3", "s4", "s5", "r1", "r2"])
        );
    }

    #[test]
    fn later_pages_shuffle_into_the_songs_to_come() {
        // Shuffle on Liked songs: page 1 (20 songs) is shuffled, five songs are played, then
        // page 2 comes (ruling S10).
        let mut q = Queue::with_seed(11);
        q.set_shuffle(true);
        q.replace(songs_named("a", 20), 0);
        for _ in 0..5 {
            q.next(false);
        }
        let played: Vec<u64> = q.items()[..=5].iter().map(|i| i.id).collect();
        let current = q.current().unwrap().clone();
        let to_come: Vec<u64> = q.items()[6..].iter().map(|i| i.id).collect();
        assert_eq!(q.append_radio(songs_named("b", 20)), 20);

        // Played songs and the current one are untouched; page 2 went only after the current.
        assert_eq!(q.current(), Some(&current));
        assert_eq!(q.current_index(), Some(5));
        let ids: Vec<u64> = q.items().iter().map(|i| i.id).collect();
        assert_eq!(&ids[..=5], &played[..]);
        // The page-1 songs to come keep their (shuffled) order among themselves...
        let page1_after: Vec<u64> = ids[6..]
            .iter()
            .copied()
            .filter(|id| to_come.contains(id))
            .collect();
        assert_eq!(page1_after, to_come);
        // ...with page 2 interleaved, not tacked on behind them.
        let upcoming = &vids(&q)[6..];
        let first_b = upcoming.iter().position(|v| v.starts_with('b')).unwrap();
        let last_a = upcoming.iter().rposition(|v| v.starts_with('a')).unwrap();
        assert!(first_b < last_a, "{upcoming:?}");
        // Page 2 itself is shuffled too.
        let b_order: Vec<&String> = upcoming.iter().filter(|v| v.starts_with('b')).collect();
        let b_given: Vec<String> = (0..20).map(|i| format!("b{i}")).collect();
        assert_ne!(b_order, b_given.iter().collect::<Vec<_>>());
        assert_orders_agree(&q);

        // The original order still appends page 2 after page 1.
        q.set_shuffle(false);
        let mut want: Vec<String> = (0..20).map(|i| format!("a{i}")).collect();
        want.extend(b_given);
        assert_eq!(vids(&q), want);
        assert_eq!(q.current(), Some(&current));

        // The same for songs the user adds at the end, and with no current song yet.
        let mut q = Queue::with_seed(5);
        q.add(songs_named("a", 10), AddAt::End);
        q.set_shuffle(true);
        q.add(songs_named("e", 10), AddAt::End);
        assert!(q.current().is_none());
        let v = vids(&q);
        let first_e = v.iter().position(|v| v.starts_with('e')).unwrap();
        let last_a = v.iter().rposition(|v| v.starts_with('a')).unwrap();
        assert!(first_e < last_a, "{v:?}");
        assert_orders_agree(&q);
    }

    #[test]
    fn append_radio_only_checks_the_last_50() {
        // A song that was in the queue long ago (more than 50 items back) can come round again.
        let mut q = queue(60, 0);
        assert_eq!(q.append_radio(vec![song("s0"), song("s59")]), 1);
        assert_eq!(q.items().last().unwrap().song.video_id, "s0");
        assert_eq!(q.len(), 61);
    }

    #[test]
    fn repeat_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Repeat::Off).unwrap(), "\"off\"");
        assert_eq!(serde_json::to_string(&Repeat::All).unwrap(), "\"all\"");
        assert_eq!(serde_json::to_string(&Repeat::One).unwrap(), "\"one\"");
        assert_eq!(
            serde_json::from_str::<Repeat>("\"one\"").unwrap(),
            Repeat::One
        );
        assert_eq!(
            serde_json::from_str::<AddAt>("\"next\"").unwrap(),
            AddAt::Next
        );
    }

    #[test]
    fn peek_next_matches_next_without_moving() {
        let mut q = queue(3, 1);
        for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
            q.set_repeat(repeat);
            for auto in [false, true] {
                for at in 0..3 {
                    q.jump(id_of(&q, &format!("s{at}")));
                    let peeked = q.peek_next(auto).map(|i| i.id);
                    assert_eq!(cur(&q), Some(format!("s{at}")), "peek doesn't move");
                    let moved = q.next(auto).map(|i| i.id);
                    assert_eq!(peeked, moved, "{repeat:?} auto={auto} at={at}");
                }
            }
        }
        assert!(Queue::with_seed(1).peek_next(true).is_none());
    }

    /// The ids in play order, and (while shuffled) the original order as ids: what a test
    /// compares to see both orders stay in step.
    fn orders(q: &Queue) -> (Vec<u64>, Option<Vec<u64>>) {
        let ids: Vec<u64> = q.items().iter().map(|i| i.id).collect();
        let original = q
            .original_positions()
            .map(|o| o.iter().map(|&p| ids[p]).collect());
        (ids, original)
    }

    /// Both orders hold exactly the same ids, each once.
    fn assert_orders_agree(q: &Queue) {
        let (ids, original) = orders(q);
        let mut a = ids.clone();
        a.sort_unstable();
        a.dedup();
        assert_eq!(a.len(), ids.len(), "an id twice in the play order");
        if let Some(o) = original {
            let mut b = o.clone();
            b.sort_unstable();
            assert_eq!(a, b, "the original order lost or gained items");
        }
    }

    #[test]
    fn add_past_the_cap_is_refused_whole() {
        let mut q = queue(500, 0);
        assert!(q.add(songs(499), AddAt::End));
        assert_eq!(q.len(), MAX_ITEMS - 1);
        // Exactly to the cap is fine.
        assert!(q.add(vec![song("last")], AddAt::Next));
        assert_eq!(q.len(), MAX_ITEMS);
        let before = orders(&q);
        let current = q.current().map(|i| i.id);
        // One more: refused, and nothing changed (no item, no id spent).
        assert!(!q.add(vec![song("over")], AddAt::End));
        assert!(!q.add(vec![song("over")], AddAt::Next));
        assert_eq!(orders(&q), before);
        assert_eq!(q.current().map(|i| i.id), current);
        // A refused add that would have fitted partly is still refused whole.
        q.remove(id_of(&q, "last"));
        assert!(!q.add(songs(2), AddAt::End));
        assert_eq!(q.len(), MAX_ITEMS - 1);
        // The next id is the one after the last song added: refused adds spent none.
        assert!(q.add(vec![song("fits")], AddAt::End));
        assert_eq!(id_of(&q, "fits"), 1001);
    }

    #[test]
    fn refill_at_the_cap_drops_played_songs_keeping_50() {
        let mut q = queue(MAX_ITEMS, 997);
        let current = q.current().unwrap().clone();
        let kept_first = q.items()[997 - KEEP_PLAYED].id;
        let radio: Vec<SongItem> = (0..100).map(|i| song(&format!("r{i}"))).collect();
        assert_eq!(q.append_radio(radio), 100);
        // 50 played, the current one, the 2 to come, then the new 100.
        assert_eq!(q.len(), KEEP_PLAYED + 1 + 2 + 100);
        assert_eq!(q.current(), Some(&current));
        assert_eq!(q.current_index(), Some(KEEP_PLAYED));
        assert_eq!(q.items()[0].id, kept_first);
        assert_eq!(cur(&q), Some("s997".into()));
        assert_eq!(q.items().last().unwrap().song.video_id, "r99");
        // Playing on still works through the trimmed queue.
        assert_eq!(
            q.next(false).map(|i| i.song.video_id.clone()),
            Some("s998".into())
        );
        assert_orders_agree(&q);
    }

    #[test]
    fn refill_below_the_cap_trims_nothing() {
        let mut q = queue(500, 498);
        assert_eq!(q.append_radio(songs_named("r", 100)), 100);
        assert_eq!(q.len(), 600);
        assert_eq!(q.current_index(), Some(498));
    }

    fn songs_named(prefix: &str, n: usize) -> Vec<SongItem> {
        (0..n).map(|i| song(&format!("{prefix}{i}"))).collect()
    }

    #[test]
    fn refill_stops_appending_at_the_cap() {
        // Few songs played (under 50 to drop): only what fits is appended.
        let mut q = queue(960, 5);
        assert_eq!(q.append_radio(songs_named("r", 100)), 40);
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(q.current_index(), Some(5));
        assert_eq!(q.items().last().unwrap().song.video_id, "r39");
        // Full with nothing to drop: nothing is added.
        assert_eq!(q.append_radio(songs_named("x", 10)), 0);
        assert_eq!(q.len(), MAX_ITEMS);
    }

    #[test]
    fn refill_trim_keeps_both_orders_in_step_while_shuffled() {
        let mut q = queue(MAX_ITEMS, 0);
        q.set_shuffle(true);
        // Play to near the end of the shuffled order.
        for _ in 0..997 {
            q.next(false);
        }
        assert!(q.needs_more());
        let current = q.current().unwrap().clone();
        let played: Vec<u64> = q.items()[..997].iter().map(|i| i.id).collect();
        assert_eq!(q.append_radio(songs_named("r", 100)), 100);
        assert_eq!(q.current(), Some(&current));
        assert_eq!(q.len(), KEEP_PLAYED + 1 + 2 + 100);
        // The songs dropped are the earliest played in play order.
        let dropped = &played[..997 - KEEP_PLAYED];
        assert!(q.items().iter().all(|i| !dropped.contains(&i.id)));
        assert_orders_agree(&q);
        // Shuffle off: the survivors in their original order, at the same current song, with
        // the radio songs last.
        q.set_shuffle(false);
        assert_eq!(q.current(), Some(&current));
        let originals: Vec<usize> = q
            .items()
            .iter()
            .filter_map(|i| i.song.video_id.strip_prefix('s'))
            .map(|n| n.parse().unwrap())
            .collect();
        assert!(originals.windows(2).all(|w| w[0] < w[1]), "{originals:?}");
        assert_eq!(q.items().last().unwrap().song.video_id, "r99");
    }

    #[test]
    fn replace_keeps_at_most_the_cap_around_the_start() {
        let mut q = Queue::with_seed(3);
        q.replace(songs(3000), 700);
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(cur(&q), Some("s700".into()));
        assert_eq!(q.current_index(), Some(KEEP_PLAYED));
        assert_eq!(q.items()[0].song.video_id, "s650");
        // Fewer than MAX_ITEMS - KEEP_PLAYED after the start: the window reaches further back.
        q.replace(songs(1500), 700);
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(cur(&q), Some("s700".into()));
        assert_eq!(q.items()[0].song.video_id, "s500");
        // Near the end: the last MAX_ITEMS songs, the start among them.
        q.replace(songs(1500), 1490);
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(cur(&q), Some("s1490".into()));
        assert_eq!(q.items().last().unwrap().song.video_id, "s1499");
        // From the first song, or a start past the end: the first MAX_ITEMS.
        q.replace(songs(1500), 5000);
        assert_eq!(q.len(), MAX_ITEMS);
        assert_eq!(cur(&q), Some("s0".into()));
        assert_eq!(q.items().last().unwrap().song.video_id, "s999");
    }
}
