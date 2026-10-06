//! Walking a whole answer: `Page.js`'s `collect` (rows into titled sections, in page order),
//! `shelfTitle`, `nextOf`, `moreOf` and `headerOf`.

use std::collections::HashSet;

use serde_json::Value;

use super::row::{ITEM_KEYS, button_play, item, thumb_of};
use super::{MoreLink, PageHeader, SEP, Section, clean_id, clean_token, get, text, truthy};

/// The renderers that hold a list of rows.
const SHELF_KEYS: [&str; 5] = [
    "musicShelfRenderer",
    "musicCarouselShelfRenderer",
    "gridRenderer",
    "musicPlaylistShelfRenderer",
    "musicCardShelfRenderer",
];

/// Keys never walked into: bookkeeping, a row's own menu (its "Go to album" and the like are not rows
/// of this page), and headers (the page header is read by `header_of`; a shelf's header holds no rows).
const SKIP_KEYS: [&str; 4] = ["frameworkUpdates", "responseContext", "menu", "header"];

const CARD: &str = "musicCardShelfRenderer";

/// Page headers (album, playlist, artist). Only these renderer names count: a looser match picked up
/// carousel headers like "Other versions".
const HEADER_KEYS: [&str; 5] = [
    "musicResponsiveHeaderRenderer",
    "musicImmersiveHeaderRenderer",
    "musicVisualHeaderRenderer",
    "musicDetailHeaderRenderer",
    "musicHeaderRenderer",
];

fn shelf_title(s: &Value) -> String {
    if let Some(t) = get(s, "title") {
        return text(Some(t));
    }
    let hh = s.get("header").and_then(|h| {
        get(h, "musicCarouselShelfBasicHeaderRenderer")
            .or_else(|| get(h, "gridHeaderRenderer"))
            .or_else(|| get(h, "musicSideAlignedItemRenderer"))
    });
    text(hh.and_then(|hh| get(hh, "title").or_else(|| get(hh, "strapline"))))
}

/// Where the next page of a list is, if it has one ("" if not). YouTube Music sends long lists a page
/// at a time (checked live 2026-09-24: playlists and Liked songs 100 rows, library artists 25 then 50,
/// filtered search 20, Home 3 shelves). Two shapes:
///   - older: `shelf.continuations[0].nextContinuationData.continuation`
///     (library artists, the playlists grid, filtered search, Home)
///   - newer: a `continuationItemRenderer` as the list's last entry, holding
///     `continuationEndpoint.continuationCommand.token` (every playlist), or the same command inside a
///     `commandExecutorCommand`'s list
///
/// `reloadContinuationData` is not a next page (sort menus, filter chips, the offline tab), so it is
/// never followed.
pub(crate) fn next_of(s: Option<&Value>) -> String {
    let Some(s) = s.filter(|s| truthy(s)) else {
        return String::new();
    };
    if let Some(t) = s
        .pointer("/continuations/0/nextContinuationData/continuation")
        .filter(|t| truthy(t))
    {
        return clean_token(Some(t));
    }
    let list = get(s, "contents")
        .or_else(|| get(s, "items"))
        .or_else(|| get(s, "continuationItems"));
    let Some(ep) = list
        .and_then(Value::as_array)
        .and_then(|l| l.last())
        .and_then(|last| last.get("continuationItemRenderer"))
        .and_then(|c| get(c, "continuationEndpoint"))
    else {
        return String::new();
    };
    if let Some(cc) = get(ep, "continuationCommand") {
        return clean_token(cc.get("token"));
    }
    let cmds = ep.pointer("/commandExecutorCommand/commands");
    cmds.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|c| get(c, "continuationCommand"))
        .map(|cc| clean_token(cc.get("token")))
        .unwrap_or_default()
}

/// A shelf's own "Show all" or "More" button (artist pages: Top songs, Albums, Singles, Videos; Home's
/// Listen again). Only browse links count, so it opens in place. Title links are left out on purpose:
/// on Home a carousel titled with an artist's name links to that artist, which is not "all of this
/// shelf". Artist-tab links (UC… with params: "Live performances", "Playlists by") are kept: one once
/// came back empty, but rechecks gave full pages every time, and an empty one just says so.
fn more_of(s: &Value) -> Option<MoreLink> {
    let btn = s
        .get("header")
        .and_then(|h| get(h, "musicCarouselShelfBasicHeaderRenderer"))
        .and_then(|h| get(h, "moreContentButton"))
        .and_then(|b| get(b, "buttonRenderer"));
    let eps = [
        s.get("bottomEndpoint"),
        btn.and_then(|b| b.get("navigationEndpoint")),
    ];
    eps.into_iter().flatten().find_map(|ep| {
        let b = get(ep, "browseEndpoint")?;
        let browse_id = clean_id(b.get("browseId"));
        (!browse_id.is_empty()).then(|| MoreLink {
            browse_id,
            params: clean_token(b.get("params")),
        })
    })
}

/// Walks any answer and gathers rows into titled sections, in page order. `limit` caps the rows per
/// section.
pub(crate) fn collect(root: &Value, limit: usize) -> Vec<Section> {
    let mut c = Collector {
        limit,
        sections: Vec::new(),
        seen: HashSet::new(),
        loose: None,
        pending_title: String::new(),
    };
    // Depth needs no guard of its own only as long as answers are parsed with serde_json's default
    // recursion limit (it refuses input nested deeper than 128 levels). Parsing with
    // `disable_recursion_limit` would need a depth cap here.
    c.walk(root, None);
    c.sections.retain(|s| !s.items.is_empty());
    c.sections
}

struct Collector {
    limit: usize,
    sections: Vec<Section>,
    seen: HashSet<String>,
    /// Rows that sit outside any shelf keep their place on the page. Search (2026-09-24) sends its top
    /// result, an empty "More results" shelf and then its rows one by one; when those rows were
    /// gathered into one untitled section at the very top, they landed above the top result (a band's
    /// name searched listed the artist 30th, and Enter opened a compilation). This is the index of the
    /// current run of loose rows in `sections`.
    loose: Option<usize>,
    /// The title of an empty shelf, waiting for the loose rows that come after it.
    pending_title: String,
}

impl Collector {
    fn loose_section(&mut self) -> usize {
        if let Some(i) = self.loose {
            return i;
        }
        self.sections.push(Section {
            title: std::mem::take(&mut self.pending_title),
            ..Section::default()
        });
        let i = self.sections.len() - 1;
        self.loose = Some(i);
        i
    }

    /// `sec` is the shelf being filled, or `None` for a loose row.
    fn push(&mut self, sec: Option<&mut Section>, key: &str, r: &Value) {
        // Page.js made the loose section before building the row (`push(sec || looseSec(), k, v)`),
        // so a loose row that comes to nothing still fixes where that run of loose rows sits and takes
        // the waiting title. Kept in the same order so sections land where Page.js put them.
        let loose = if sec.is_none() {
            Some(self.loose_section())
        } else {
            None
        };
        let Some(it) = item(key, r) else {
            return;
        };
        let target = [&it.set_id, &it.video_id, &it.browse_id, &it.playlist_id]
            .into_iter()
            .find(|s| !s.is_empty())
            .map_or("", String::as_str);
        let id = format!("{}:{}:{}", it.kind.as_str(), target, it.title);
        if !self.seen.insert(id) {
            return;
        }
        let limit = self.limit;
        let sec = match (sec, loose) {
            (Some(s), _) => s,
            (None, Some(i)) => &mut self.sections[i],
            (None, None) => unreachable!("a loose row always has its section"),
        };
        if sec.items.len() < limit {
            sec.items.push(it);
        }
    }

    /// `sec` is `None` outside a shelf: a shelf found there starts a section, and a shelf inside a
    /// shelf does not (a card shelf inside one is just a row).
    fn walk(&mut self, o: &Value, mut sec: Option<&mut Section>) {
        match o {
            Value::Array(list) => {
                for v in list {
                    self.walk(v, sec.as_deref_mut());
                }
            }
            Value::Object(map) => {
                for (k, v) in map {
                    if !matches!(v, Value::Object(_) | Value::Array(_)) {
                        continue;
                    }
                    let k = k.as_str();
                    if SHELF_KEYS.contains(&k) && sec.is_none() {
                        let mut s = Section {
                            title: shelf_title(v),
                            items: Vec::new(),
                            cont: next_of(Some(v)),
                            more: more_of(v),
                        };
                        self.loose = None;
                        if k == CARD {
                            // The card's own title is the result's name, which read as a header above
                            // a row that says the same thing.
                            s.title = "Top result".to_owned();
                            self.push(Some(&mut s), k, v);
                        }
                        if let Some(inner) = get(v, "contents").or_else(|| get(v, "items")) {
                            self.walk(inner, Some(&mut s));
                        }
                        if !s.items.is_empty() {
                            self.sections.push(s);
                        } else if !s.title.is_empty() {
                            self.pending_title = s.title;
                        }
                    } else if ITEM_KEYS.contains(&k) {
                        self.push(sec.as_deref_mut(), k, v);
                    } else if !SKIP_KEYS.contains(&k) {
                        self.walk(v, sec.as_deref_mut());
                    }
                }
            }
            _ => {}
        }
    }
}

/// The page header: the first of `HEADER_KEYS` with a title, searched breadth first through the
/// answer's `header` and `contents` (at most 4,000 nodes, as in Page.js).
pub(crate) fn header_of(res: &Value) -> PageHeader {
    let mut queue: Vec<Option<&Value>> = vec![res.get("header"), res.get("contents")];
    let mut h = None;
    let mut n = 0;
    'search: while n < queue.len() && n < 4000 {
        let o = queue[n];
        n += 1;
        let children: Box<dyn Iterator<Item = (Option<&str>, &Value)>> = match o {
            Some(Value::Object(m)) => Box::new(m.iter().map(|(k, v)| (Some(k.as_str()), v))),
            Some(Value::Array(l)) => Box::new(l.iter().map(|v| (None, v))),
            _ => continue,
        };
        for (k, v) in children {
            if k.is_some_and(|k| HEADER_KEYS.contains(&k)) && get(v, "title").is_some() {
                h = Some(v);
                break 'search;
            }
            if matches!(v, Value::Object(_) | Value::Array(_)) {
                queue.push(Some(v));
            }
        }
    }
    let Some(h) = h else {
        return PageHeader::default();
    };
    let subtitle = [text(h.get("straplineTextOne")), text(h.get("subtitle"))]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(SEP);
    // What the page's big button plays: artist Shuffle (then Mix), album or playlist Play.
    let mut buttons: Vec<&Value> = [h.get("playButton"), h.get("startRadioButton")]
        .into_iter()
        .flatten()
        .collect();
    match h.get("buttons") {
        Some(Value::Array(list)) => buttons.extend(list),
        Some(other) => buttons.push(other),
        None => {}
    }
    PageHeader {
        title: text(h.get("title")),
        subtitle,
        thumb: thumb_of(h),
        play: button_play(buttons),
    }
}
