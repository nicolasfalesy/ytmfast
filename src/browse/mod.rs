//! YouTube Music browse and search answers, turned into the small shapes the bar widget lists.
//!
//! This is a rule-for-rule port of the widget's `Page.js` (the code that ran inside the YouTube Music
//! app page before ytmfast existed). Its output shapes are the contract the widget relies on: the
//! same field names, empty strings rather than missing values, and rows in page order. The golden
//! test (`tests/browse_parse.rs`) runs every scrubbed fixture through both and compares them.
//!
//! What ytmfast adds on top of `Page.js`, because its answers leave the app's page and go to other
//! programs over a socket:
//! - every id, `params` and token is checked for shape before it is used or sent back out
//!   ([`video_id_ok`], [`id_ok`], [`token_ok`]); one that fails becomes `""`, and a row that can then
//!   do nothing is dropped;
//! - a play endpoint keeps only the fields that play something ([`Endpoint`]);
//! - every picture link passes [`crate::net::allowed_host`], or becomes `""`.
//!
//! Key order matters here: `collect` lists rows in the order YouTube's JSON holds them, like `Page.js`'s
//! `for (var k in o)` walk. That is why serde_json's `preserve_order` feature is on (Cargo.toml): with
//! the default sorted map, a search's top result could land below rows that come after it on the page.
//! (JavaScript lists integer-like keys first; YouTube's keys are never integer-like, so the two walks
//! agree.)

mod collect;
mod row;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use collect::{collect, header_of, next_of};

/// YouTube's own separator, so the columns joined here match the text inside them (a column already
/// reads "Song • Artist"; a different dot between columns looked mixed).
pub(crate) const SEP: &str = " • ";

/// What a row opens or plays, from `Page.js`'s `kindOf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Song,
    Album,
    Artist,
    Playlist,
    Podcast,
    /// Some other browse page.
    Page,
    /// Nothing to open. `Page.js` sent `""`.
    #[default]
    #[serde(rename = "")]
    None,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Song => "song",
            Kind::Album => "album",
            Kind::Artist => "artist",
            Kind::Playlist => "playlist",
            Kind::Podcast => "podcast",
            Kind::Page => "page",
            Kind::None => "",
        }
    }
}

/// One list row. Fields serialize in `Page.js`'s order, `kind` last (it is added last there), so the
/// output diffs cleanly against the widget's old answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub title: String,
    pub subtitle: String,
    pub thumb: String,
    pub video_id: String,
    /// A playlist entry's own id, so a playlist holding a song twice keeps both rows.
    pub set_id: String,
    pub playlist_id: String,
    pub browse_id: String,
    pub params: String,
    pub play: Option<Endpoint>,
    /// "m:ss", or "" (podcast lengths go in the subtitle).
    pub duration: String,
    pub kind: Kind,
}

/// A section's own "Show all" / "More" link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MoreLink {
    pub browse_id: String,
    pub params: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub title: String,
    pub items: Vec<Row>,
    /// The token for this list's next page, "" at the end.
    pub cont: String,
    pub more: Option<MoreLink>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageHeader {
    pub title: String,
    pub subtitle: String,
    pub thumb: String,
    /// What the page's big button plays: an artist's Shuffle (then Mix), an album's or playlist's Play.
    pub play: Option<Endpoint>,
}

/// A browse answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub header: PageHeader,
    pub sections: Vec<Section>,
    /// The section list's own next page (Home's further shelves), "" when there is none.
    pub cont: String,
}

/// A search filter (Songs, Albums, ...): its `params` go back with the same query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chip {
    pub label: String,
    pub params: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    pub sections: Vec<Section>,
    pub chips: Vec<Chip>,
}

/// A continuation: rows for the end of the list, or (Home) whole new sections.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MorePage {
    pub items: Vec<Row>,
    pub sections: Vec<Section>,
    pub cont: String,
}

/// A song's like status. On the socket: "like", "dislike", "none".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LikeStatus {
    Like,
    Dislike,
    #[serde(rename = "none")]
    Indifferent,
}

/// Something that plays: one of YouTube's two play endpoints, holding only the fields that matter for
/// playing. `Page.js` passed YouTube's whole endpoint object through; here everything else
/// (`playerParams`, logging blocks, unknown keys) is dropped, and every value is shape-checked, so a
/// client never sees raw YouTube JSON and never gets to send arbitrary JSON back.
///
/// Deserializing (an endpoint a client sends back) runs the same checks: anything that can't play
/// after cleaning is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "Value")]
pub enum Endpoint {
    #[serde(rename = "watchEndpoint")]
    Watch(WatchEndpoint),
    #[serde(rename = "watchPlaylistEndpoint")]
    WatchPlaylist(WatchPlaylistEndpoint),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchEndpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
    /// A position in the playlist (or YouTube's queue item id).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchPlaylistEndpoint {
    pub playlist_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<String>,
}

impl Endpoint {
    /// Cleans one YouTube endpoint object (`{watchEndpoint: …}` or `{watchPlaylistEndpoint: …}`).
    /// `None` when it is neither, or when nothing playable is left after the checks.
    pub fn from_endpoint(ep: &Value) -> Option<Endpoint> {
        get(ep, "watchEndpoint")
            .and_then(Endpoint::watch)
            .or_else(|| get(ep, "watchPlaylistEndpoint").and_then(Endpoint::watch_playlist))
    }

    /// From the inside of a `watchEndpoint`. A watch with neither a video nor a playlist plays nothing.
    pub fn watch(we: &Value) -> Option<Endpoint> {
        let w = WatchEndpoint {
            video_id: some(clean_video_id(we.get("videoId"))),
            playlist_id: some(clean_id(we.get("playlistId"))),
            // Positions and YouTube's queue item ids are small; a value past u32 (or negative, or not
            // a whole number) is no real position, so it is dropped rather than passed on.
            index: we
                .get("index")
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok()),
            params: some(clean_token(we.get("params"))),
        };
        (w.video_id.is_some() || w.playlist_id.is_some()).then_some(Endpoint::Watch(w))
    }

    /// From the inside of a `watchPlaylistEndpoint`, which needs its playlist id.
    pub fn watch_playlist(wpe: &Value) -> Option<Endpoint> {
        let playlist_id = some(clean_id(wpe.get("playlistId")))?;
        Some(Endpoint::WatchPlaylist(WatchPlaylistEndpoint {
            playlist_id,
            params: some(clean_token(wpe.get("params"))),
        }))
    }
}

impl TryFrom<Value> for Endpoint {
    type Error = &'static str;
    fn try_from(v: Value) -> Result<Self, Self::Error> {
        Endpoint::from_endpoint(&v).ok_or("not a playable endpoint")
    }
}

/// A browse answer: the page header, its sections in page order, and its own next page.
pub fn parse_browse(r: &Value) -> Page {
    let header = header_of(r);
    let mut sections = collect(r, 300);
    // Album tracks come without their own art: use the album cover.
    if !header.thumb.is_empty() {
        for row in sections.iter_mut().flat_map(|s| s.items.iter_mut()) {
            if row.thumb.is_empty() {
                row.thumb.clone_from(&header.thumb);
            }
        }
    }
    // Home sends more shelves as the user scrolls: the section list's own next page. Only the
    // one-column layout counts. On a playlist page the (two-column) section list's next page is
    // "Suggestions": songs that are NOT in the playlist, and it never ends.
    let sl = r.pointer(
        "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer",
    );
    Page {
        header,
        sections,
        cont: next_of(sl),
    }
}

/// A search answer. `filtered` is a search made with a filter chip's `params` (Songs, Albums, ...):
/// it pages 20 rows at a time and keeps up to 300 a section, while the mixed results have no next page
/// and keep 30 a section.
pub fn parse_search(r: &Value, filtered: bool) -> SearchPage {
    let mut chips = Vec::new();
    let list = r.pointer(
        "/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/header/chipCloudRenderer/chips",
    );
    for c in list.and_then(Value::as_array).into_iter().flatten() {
        let Some(x) = get(c, "chipCloudChipRenderer") else {
            continue;
        };
        // Only search filters: a chip that browses elsewhere (Library) has no searchEndpoint.
        let params = x
            .get("navigationEndpoint")
            .and_then(|n| get(n, "searchEndpoint"))
            .map(|se| clean_token(se.get("params")))
            .unwrap_or_default();
        if !params.is_empty() {
            chips.push(Chip {
                label: text(x.get("text")),
                params,
            });
        }
    }
    let mut sections = collect(r, if filtered { 300 } else { 30 });
    // Name untitled runs, or the list shows them under the previous header: an untitled first shelf is
    // the top result (podcasts get no card), and the loose rows after it are the rest ("More results"
    // only comes with a named shelf).
    if sections.len() > 1 {
        for (i, s) in sections.iter_mut().enumerate() {
            if s.title.is_empty() {
                s.title = if i == 0 { "Top result" } else { "More results" }.to_owned();
            }
        }
    }
    SearchPage { sections, chips }
}

/// A continuation answer (browse or search: the token alone is the request).
pub fn parse_more(r: &Value) -> MorePage {
    let cc = r.get("continuationContents");
    if let Some(slc) = cc.and_then(|c| get(c, "sectionListContinuation")) {
        return MorePage {
            items: Vec::new(),
            sections: collect(slc, 300),
            cont: next_of(Some(slc)),
        };
    }
    let mut shelf = cc.and_then(|c| {
        get(c, "musicShelfContinuation")
            .or_else(|| get(c, "musicPlaylistShelfContinuation"))
            .or_else(|| get(c, "gridContinuation"))
    });
    if shelf.is_none() {
        shelf = r
            .get("onResponseReceivedActions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find_map(|a| get(a, "appendContinuationItemsAction"));
    }
    let items = match shelf {
        Some(s) => collect(s, 1000).into_iter().flat_map(|s| s.items).collect(),
        None => Vec::new(),
    };
    MorePage {
        items,
        sections: Vec::new(),
        cont: next_of(shelf),
    }
}

/// The browse id of a song's lyrics page (`MPLYt…`), from the song's `next` answer: the watch-next
/// panel's Lyrics tab. `None` when the song has no lyrics tab.
pub fn parse_lyrics_tab(next: &Value) -> Option<String> {
    let tabs = next.pointer(
        "/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer/tabs",
    )?;
    // The last lyrics tab wins, as in Page.js (its forEach kept overwriting).
    let mut found = None;
    for t in tabs.as_array().into_iter().flatten() {
        let Some(b) = t.pointer("/tabRenderer/endpoint/browseEndpoint") else {
            continue;
        };
        let page_type = b
            .pointer(
                "/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType",
            )
            .and_then(Value::as_str);
        if page_type == Some("MUSIC_PAGE_TYPE_TRACK_LYRICS") {
            found = Some(b);
        }
    }
    some(clean_id(found?.get("browseId")))
}

/// YouTube Music's own lyrics (plain text, no timings) from the lyrics browse page: one
/// musicDescriptionShelfRenderer holding the text and a "Source: …" footer. `(text, source)`, or
/// `None` when there is no text.
pub fn parse_lyrics(b: &Value) -> Option<(String, String)> {
    let shelf = b
        .pointer("/contents/sectionListRenderer/contents")?
        .as_array()?
        .iter()
        .find_map(|c| get(c, "musicDescriptionShelfRenderer"))?;
    let t = text(shelf.get("description"));
    if t.is_empty() {
        return None;
    }
    Some((t, text(shelf.get("footer"))))
}

/// The like status of the song a `next` answer is for: the player overlay's like button.
pub fn parse_like_status(next: &Value) -> Option<LikeStatus> {
    let actions = next.pointer("/playerOverlays/playerOverlayRenderer/actions")?;
    actions.as_array()?.iter().find_map(|a| {
        match a.pointer("/likeButtonRenderer/likeStatus")?.as_str()? {
            "LIKE" => Some(LikeStatus::Like),
            "DISLIKE" => Some(LikeStatus::Dislike),
            "INDIFFERENT" => Some(LikeStatus::Indifferent),
            _ => None,
        }
    })
}

// ---- Shared helpers ----

/// JavaScript truthiness, which every `a || b` and `if (x)` in `Page.js` relies on.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// `o.key`, kept only when truthy (so `get(a, "x").or_else(|| get(a, "y"))` is JS's `a.x || a.y`).
pub(crate) fn get<'a>(o: &'a Value, key: &str) -> Option<&'a Value> {
    o.get(key).filter(|v| truthy(v))
}

/// YouTube's text object: `simpleText`, or its `runs` joined.
pub(crate) fn text(t: Option<&Value>) -> String {
    let Some(t) = t else {
        return String::new();
    };
    if let Some(s) = get(t, "simpleText").and_then(Value::as_str) {
        return s.to_owned();
    }
    t.get("runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("text").and_then(Value::as_str))
        .collect()
}

fn some(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

fn id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// A YouTube video id: 11 characters of base64url.
pub fn video_id_ok(s: &str) -> bool {
    s.len() == 11 && s.bytes().all(id_char)
}

/// A playlist, browse or playlist-entry id: 2 to 128 characters of base64url.
pub fn id_ok(s: &str) -> bool {
    (2..=128).contains(&s.len()) && s.bytes().all(id_char)
}

/// A `params` value or a continuation token: base64, either alphabet (YouTube's tokens and params can
/// hold `+` and `/` as well as `-` and `_`; blanking those would silently stop paging and filters,
/// ruling P3), with `=` padding or its `%3D` escape, at most 4 KiB. These go back to YouTube as JSON
/// strings; the charset keeps anything else (spaces, quotes, colons, markup, a smuggled URL) out.
pub fn token_ok(s: &str) -> bool {
    (1..=4096).contains(&s.len())
        && s.bytes()
            .all(|c| id_char(c) || matches!(c, b'%' | b'=' | b'+' | b'/'))
}

fn clean(v: Option<&Value>, ok: fn(&str) -> bool) -> String {
    v.and_then(Value::as_str)
        .filter(|s| ok(s))
        .map(str::to_owned)
        .unwrap_or_default()
}

pub(crate) fn clean_video_id(v: Option<&Value>) -> String {
    clean(v, video_id_ok)
}

pub(crate) fn clean_id(v: Option<&Value>) -> String {
    clean(v, id_ok)
}

pub(crate) fn clean_token(v: Option<&Value>) -> String {
    clean(v, token_ok)
}
