//! The queue's JSON text, kept per revision.
//!
//! A `queue.get` (a 40-byte request) or a `watch` turned back on answers with the whole queue:
//! up to about 600 KiB of JSON for a long radio queue, and building that text was 84 to 95% of
//! a core under a repeating client. The queue only changes with a new revision (`rev`, bumped on
//! every change and carried by every queue line), so the last text is kept and handed out again
//! until it does; the same text serves every connected client's `queue` event too.

use std::sync::{Arc, Mutex};

use crate::engine::QueueView;
use crate::queue::Repeat;

use super::protocol;

/// What the cached text was made from. The revision decides; the rest is a cheap guard, so a
/// view that did not come with a bump (none should) is never answered with stale text.
#[derive(PartialEq)]
struct Key {
    rev: u64,
    len: usize,
    current_id: Option<u64>,
    shuffle: bool,
    repeat: Repeat,
}

impl Key {
    fn of(view: &QueueView) -> Key {
        Key {
            rev: view.rev,
            len: view.items.len(),
            current_id: view.current_id,
            shuffle: view.shuffle,
            repeat: view.repeat,
        }
    }
}

struct Cached {
    key: Key,
    /// The `queue.get` reply's `data` object.
    data: Option<Arc<str>>,
    /// The whole `queue` event line, newline included.
    event: Option<Arc<str>>,
}

/// The last queue serialized, as a `queue.get` reply's data and as an event line.
#[derive(Default)]
pub struct QueueText {
    slot: Mutex<Option<Cached>>,
}

impl QueueText {
    /// The `data` object of a `queue.get` reply for `view`.
    pub fn data(&self, view: &QueueView) -> Arc<str> {
        self.get(view, |c| &mut c.data, || protocol::queue_data_text(view))
    }

    /// The `queue` event line (newline included) for `view`.
    pub fn event(&self, view: &QueueView) -> Arc<str> {
        self.get(view, |c| &mut c.event, || protocol::queue_event_text(view))
    }

    fn get(
        &self,
        view: &QueueView,
        field: impl Fn(&mut Cached) -> &mut Option<Arc<str>>,
        make: impl FnOnce() -> String,
    ) -> Arc<str> {
        let key = Key::of(view);
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        let cached = match &mut *slot {
            Some(c) if c.key == key => c,
            other => other.insert(Cached {
                key,
                data: None,
                event: None,
            }),
        };
        let text = field(cached);
        // Made while the lock is held: two clients asking for the same new revision at once
        // serialize it once, not twice.
        text.get_or_insert_with(|| make().into()).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::innertube::SongItem;
    use crate::queue::QueueItem;

    fn view(rev: u64, n: u64) -> QueueView {
        let items: Vec<QueueItem> = (1..=n)
            .map(|id| QueueItem {
                id,
                song: SongItem {
                    video_id: format!("{id:011}"),
                    title: format!("Song {id}"),
                    artists: vec!["Artist".into()],
                    album: None,
                    album_id: String::new(),
                    thumbnail: None,
                    length_seconds: 200,
                    playlist_id: None,
                    radio: false,
                },
            })
            .collect();
        QueueView {
            items: items.into(),
            current_id: Some(1),
            shuffle: false,
            repeat: Repeat::Off,
            rev,
        }
    }

    #[test]
    fn an_unchanged_queue_is_serialized_once() {
        let cache = QueueText::default();
        let v = view(7, 50);
        let a = cache.data(&v);
        let b = cache.data(&v);
        assert!(
            Arc::ptr_eq(&a, &b),
            "the second ask re-serialized the queue"
        );
        let e = cache.event(&v);
        assert!(Arc::ptr_eq(&e, &cache.event(&v)));
        // The text is what the protocol makes of it.
        assert_eq!(&*a, protocol::queue_data_text(&v));
        assert_eq!(&*e, protocol::queue_event_text(&v));
        assert!(e.ends_with('\n') && e.starts_with("{\"event\":\"queue\""));
    }

    #[test]
    fn a_new_revision_or_a_changed_view_is_serialized_again() {
        let cache = QueueText::default();
        let a = cache.data(&view(7, 50));
        let next = cache.data(&view(8, 50));
        assert!(!Arc::ptr_eq(&a, &next), "a new revision got the old text");
        // The same revision with other contents (a bump that was missed) is not answered
        // with the old text either.
        let other = cache.data(&view(8, 51));
        assert!(!Arc::ptr_eq(&next, &other));
        assert_ne!(&*next, &*other);
    }
}
