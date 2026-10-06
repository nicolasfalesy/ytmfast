//! One row from one YouTube renderer: `Page.js`'s `item`, `thumbOf`, `playOf`, `buttonPlay` and
//! `kindOf`.

use serde_json::Value;

use super::{
    Endpoint, Kind, Row, SEP, WatchEndpoint, clean_id, clean_token, clean_video_id, get, text,
    truthy,
};

/// The renderers that are one row each.
pub(crate) const ITEM_KEYS: [&str; 5] = [
    "musicResponsiveListItemRenderer",
    "musicTwoRowItemRenderer",
    "musicCardShelfRenderer",
    "playlistPanelVideoRenderer",
    "musicMultiRowListItemRenderer",
];

/// A renderer's picture: one near 120 px, as an https link on an allowed host, or "".
pub(crate) fn thumb_of(r: &Value) -> String {
    let holders = [
        r.get("thumbnail"),
        r.get("thumbnailRenderer"),
        r.get("thumbnail")
            .and_then(|t| t.get("croppedSquareThumbnailRenderer")),
    ];
    // The first holder with a picture list wins, even an empty one (as in Page.js).
    let list = holders
        .into_iter()
        .flatten()
        .filter(|h| truthy(h))
        .find_map(|h| {
            let m = get(h, "musicThumbnailRenderer").unwrap_or(h);
            get(m, "thumbnail")
                .and_then(|t| get(t, "thumbnails"))
                .or_else(|| get(m, "thumbnails"))
        });
    let Some(list) = list.and_then(Value::as_array).filter(|l| !l.is_empty()) else {
        return String::new();
    };
    // Pick one near 120 px: plenty for a 40-50 px row on a 2x screen.
    let mut best = &list[0];
    for t in list {
        if t.get("width")
            .and_then(Value::as_f64)
            .is_some_and(|w| w <= 226.0)
        {
            best = t;
        }
    }
    let mut u = best
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if u.starts_with("//") {
        u.insert_str(0, "https:");
    }
    // Queue and Up next videos come as plain hqdefault.jpg: a 4:3 picture with black bars above and
    // below the 16:9 frame, which the widget's square crop kept. mqdefault.jpg is the same frame at
    // 16:9 with no bars. (Search results carry YouTube's "sqp=" crop and are already bar-free.)
    if u.contains("//i.ytimg.com/vi/")
        && !u.contains("sqp=")
        && let Some(cut) = default_jpg_at(&u)
    {
        u.truncate(cut);
        u.push_str("/mqdefault.jpg");
    }
    // A picture link goes to the widget, which loads it: only https on YouTube's own hosts, and sent
    // in the parsed form that was checked (see `allowed_link`).
    crate::net::allowed_link(&u).unwrap_or_default()
}

/// Where Page.js's `/\/(hq|sd)?default\.jpg.*$/` first matches: the leftmost `/` followed by
/// `hqdefault.jpg`, `sddefault.jpg` or `default.jpg` (so `mqdefault.jpg` and `maxresdefault.jpg`
/// are left alone).
fn default_jpg_at(u: &str) -> Option<usize> {
    u.match_indices('/').map(|(i, _)| i).find(|&i| {
        let rest = &u[i + 1..];
        ["hqdefault.jpg", "sddefault.jpg", "default.jpg"]
            .iter()
            .any(|p| rest.starts_with(p))
    })
}

/// The play button over a tile's picture.
fn play_of(overlay: Option<&Value>) -> Option<Endpoint> {
    let ep = overlay?.pointer(
        "/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint",
    )?;
    Endpoint::from_endpoint(ep)
}

/// The first play-type action among a card's or page header's buttons (artist: Shuffle, then Mix;
/// radio card: Play; album or playlist: the round play button). Save, Share and Subscribe buttons
/// carry other commands and are skipped. (Page.js returned the first watch-type endpoint even if it
/// was malformed; here one that fails the checks is skipped too, so a later good button still plays.)
pub(crate) fn button_play<'a>(list: impl IntoIterator<Item = &'a Value>) -> Option<Endpoint> {
    list.into_iter().find_map(|b| {
        let ep = if let Some(m) = get(b, "musicPlayButtonRenderer") {
            m.get("playNavigationEndpoint")
        } else if let Some(br) = get(b, "buttonRenderer") {
            get(br, "command").or_else(|| br.get("navigationEndpoint"))
        } else {
            None
        }?;
        Endpoint::from_endpoint(ep)
    })
}

fn kind_of(it: &Row) -> Kind {
    let b = it.browse_id.as_str();
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| b.starts_with(p));
    if starts(&["MPREb"]) {
        Kind::Album
    } else if starts(&["UC", "MPLA"]) {
        Kind::Artist
    } else if starts(&["MPSP"]) {
        Kind::Podcast
    } else if starts(&["VL", "RD", "PL", "OLAK"]) {
        Kind::Playlist
    } else if !it.video_id.is_empty() {
        // A song tile can carry a radio playlistId too; the videoId wins.
        Kind::Song
    } else if !it.playlist_id.is_empty() {
        Kind::Playlist
    } else if !b.is_empty() {
        Kind::Page
    } else {
        Kind::None
    }
}

/// One row from the renderer `r` found under `key`, or `None` for a tile that does nothing or has no
/// title.
pub(crate) fn item(key: &str, r: &Value) -> Option<Row> {
    let mut it = Row {
        thumb: thumb_of(r),
        ..Row::default()
    };
    // Where the row leads. Most renderers carry it as navigationEndpoint; two kinds look elsewhere.
    let mut nav = r.get("navigationEndpoint");
    match key {
        "musicResponsiveListItemRenderer" => {
            let column = |c: &Value, inner: &str| text(c.get(inner).and_then(|x| x.get("text")));
            let cols: Vec<String> = r
                .get("flexColumns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|c| column(c, "musicResponsiveListItemFlexColumnRenderer"))
                .collect();
            it.title = cols.first().cloned().unwrap_or_default();
            it.subtitle = join(cols.iter().skip(1).map(String::as_str));
            it.duration = r
                .get("fixedColumns")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|c| column(c, "musicResponsiveListItemFixedColumnRenderer"))
                .collect();
            let data = r.get("playlistItemData");
            it.video_id = clean_video_id(data.and_then(|d| d.get("videoId")));
            // A playlist can hold the same song twice (a 901-song "Library Songs" had 11 such rows).
            // Each entry has its own set id; keying on it keeps them all, like the app does, instead
            // of dropping the repeats.
            it.set_id = clean_id(data.and_then(|d| d.get("playlistSetVideoId")));
            it.play = play_of(r.get("overlay"));
        }
        "musicMultiRowListItemRenderer" => {
            // Podcast episodes (show pages, "Episodes for Later"). Without this a podcast page came
            // back with no rows at all. Tapping one plays it (onTap); its title links to an episode
            // page with nothing to list. The length goes in the subtitle, not the time column: it
            // reads "36 min" or "1 hr 12 min", wider than any song time the column is sized for.
            it.title = text(r.get("title"));
            let len = r
                .get("playbackProgress")
                .and_then(|p| get(p, "musicPlaybackProgressRenderer"))
                .map(|pp| {
                    text(pp.get("durationText"))
                        .trim_start_matches(|c: char| {
                            c.is_whitespace() || c == '•' || c == '\u{feff}'
                        })
                        .to_owned()
                })
                .unwrap_or_default();
            let sub = text(r.get("subtitle"));
            it.subtitle = join([sub.as_str(), len.as_str()]);
            it.play = play_of(r.get("overlay"));
            nav = r.get("onTap");
        }
        "musicTwoRowItemRenderer" => {
            it.title = text(r.get("title"));
            it.subtitle = text(r.get("subtitle"));
            it.play = play_of(r.get("thumbnailOverlay"));
        }
        "musicCardShelfRenderer" => {
            it.title = text(r.get("title"));
            it.subtitle = text(r.get("subtitle"));
            // Search's top-result card has no thumbnail overlay: its Shuffle / Play button is the
            // action (an artist card played nothing before).
            it.play = play_of(r.get("thumbnailOverlay")).or_else(|| {
                button_play(
                    r.get("buttons")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            });
            // The card leads where its title leads (the artist, album or song it is about).
            if let Some(tn) = r
                .pointer("/title/runs/0/navigationEndpoint")
                .filter(|v| truthy(v))
            {
                nav = Some(tn);
            }
        }
        "playlistPanelVideoRenderer" => {
            it.title = text(r.get("title"));
            it.subtitle = text(r.get("shortBylineText"));
            if it.subtitle.is_empty() {
                it.subtitle = text(r.get("longBylineText"));
            }
            it.duration = text(r.get("lengthText"));
            it.video_id = clean_video_id(r.get("videoId"));
        }
        _ => {}
    }
    let nav = nav.filter(|n| truthy(n));
    if let Some(be) = nav.and_then(|n| get(n, "browseEndpoint")) {
        it.browse_id = clean_id(be.get("browseId"));
        it.params = clean_token(be.get("params"));
    }
    if let Some(we) = nav.and_then(|n| get(n, "watchEndpoint")) {
        if it.video_id.is_empty() {
            it.video_id = clean_video_id(we.get("videoId"));
        }
        it.playlist_id = clean_id(we.get("playlistId"));
        if it.play.is_none() {
            it.play = Endpoint::watch(we);
        }
    }
    // A library artist page's "Shuffle all" row links straight to a playlist.
    if it.play.is_none()
        && let Some(wpe) = nav.and_then(|n| get(n, "watchPlaylistEndpoint"))
    {
        it.play = Endpoint::watch_playlist(wpe);
    }
    if let Some(Endpoint::WatchPlaylist(w)) = &it.play
        && it.playlist_id.is_empty()
    {
        it.playlist_id.clone_from(&w.playlist_id);
    }
    if it.play.is_none() && !it.video_id.is_empty() {
        it.play = Some(Endpoint::Watch(WatchEndpoint {
            video_id: Some(it.video_id.clone()),
            ..WatchEndpoint::default()
        }));
    }
    it.kind = kind_of(&it);
    // Drop tiles that do nothing from here, like "New playlist".
    if it.video_id.is_empty() && it.browse_id.is_empty() && it.play.is_none() {
        return None;
    }
    (!it.title.is_empty()).then_some(it)
}

/// The non-empty parts, joined with YouTube's separator.
fn join<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    parts
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(SEP)
}
