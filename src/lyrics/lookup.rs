//! The two lookups (`kugouLookup`, `lrclibLookup`), with the widget's links and choices.

use serde_json::Value;

use super::js::{encode_uri_component as enc, number, round, string, truthy};
use super::krc::{krc_text, parse_krc};
use super::lrc::{parse_lrc, plain_lines};
use super::names::{clean_title, first_artist, same_name};
use super::web::{Fetched, LyricsWeb};
use super::{Found, Line, SongFacts};

/// What a lookup found, and whether any of its requests failed (see `Outcome::failed`).
pub struct Looked<T> {
    pub value: Option<T>,
    pub failed: bool,
}

/// One request: its JSON (`None` for "not found"), noting a failure in `failed`.
async fn get(web: &dyn LyricsWeb, url: &str, failed: &mut bool) -> Option<Value> {
    match web.get_json(url).await {
        Fetched::Json(v) => Some(v),
        Fetched::NotFound => None,
        Fetched::Failed => {
            *failed = true;
            None
        }
    }
}

/// The song's length as the lookups use it: whole seconds.
fn seconds(song: &SongFacts) -> f64 {
    round(f64::from(song.length_seconds))
}

/// KuGou's word timing: a search by "first artist - title" and the length, then the lyrics of
/// the best candidate. Only a candidate with the same title and artist (`same_name`) and a
/// length within 3 s: a wrong song's words are worse than LRCLIB's lines. `None` also for a
/// KRC that can't be read or isn't really timed (`parse_krc`), which is not a failure: asked
/// again, it would say the same.
pub async fn kugou(web: &dyn LyricsWeb, song: &SongFacts) -> Looked<Vec<Line>> {
    let mut failed = false;
    let value = kugou_inner(web, song, &mut failed).await;
    Looked { value, failed }
}

async fn kugou_inner(
    web: &dyn LyricsWeb,
    song: &SongFacts,
    failed: &mut bool,
) -> Option<Vec<Line>> {
    let clean = clean_title(&song.title);
    let first = first_artist(&song.artist);
    let dur = seconds(song);
    if clean.is_empty() || first.is_empty() {
        return None;
    }
    let search = format!(
        "https://krcs.kugou.com/search?ver=1&man=yes&client=mobi&keyword={}&duration={}&hash=",
        enc(&format!("{first} - {clean}")),
        dur * 1000.0
    );
    let d = get(web, &search, failed).await;
    let mut best: Option<(f64, &Value)> = None;
    let candidates = d
        .as_ref()
        .filter(|d| truthy(d.get("candidates")))
        .and_then(|d| d["candidates"].as_array());
    for c in candidates.into_iter().flatten() {
        let off = if dur > 0.0 {
            (number(c.get("duration")) / 1000.0 - dur).abs()
        } else {
            0.0
        };
        // `off > 3` is false for NaN, as in JavaScript: a candidate with an unreadable length
        // passes this check. It never beats a best already chosen (`NaN < b` is false), but
        // checked first it becomes the best, and then nothing replaces it (`off < NaN` is
        // false too). Kept as the widget had it; real KuGou lengths are always numbers.
        if off > 3.0 || !truthy(c.get("id")) || !truthy(c.get("accesskey")) {
            continue;
        }
        if !same_name(&string(c.get("song")), &clean) || !same_name(&string(c.get("singer")), first)
        {
            continue;
        }
        if best.is_none_or(|(b, _)| off < b) {
            best = Some((off, c));
        }
    }
    let (_, c) = best?;
    let download = format!(
        "https://lyrics.kugou.com/download?ver=1&client=pc&fmt=krc&charset=utf8&id={}&accesskey={}",
        enc(&string(c.get("id"))),
        enc(&string(c.get("accesskey")))
    );
    let x = get(web, &download, failed).await?;
    if !truthy(x.get("content")) {
        return None;
    }
    // Unreadable (a broken stream, or one past the inflate cap): no word timing.
    let text = krc_text(&string(x.get("content"))).ok()?;
    parse_krc(&text, &clean, first)
}

/// LRCLIB: the exact song (`/api/get`: artist, title, album, length); when that has no timed
/// lines, a search by the cleaned title and first artist, taking the closest length within
/// 3 s that has timed lines. Falls back to the exact song's plain text.
pub async fn lrclib(web: &dyn LyricsWeb, song: &SongFacts) -> Looked<Found> {
    let mut failed = false;
    let value = lrclib_inner(web, song, &mut failed).await;
    Looked { value, failed }
}

fn timed(text: &str) -> Found {
    Found {
        source: "LRCLIB".into(),
        synced: true,
        words: false,
        lines: parse_lrc(text),
    }
}

async fn lrclib_inner(web: &dyn LyricsWeb, song: &SongFacts, failed: &mut bool) -> Option<Found> {
    let q = |k: &str, v: &str| format!("{k}={}", enc(v));
    let dur = seconds(song);
    let mut url = format!(
        "https://lrclib.net/api/get?{}&{}",
        q("artist_name", &song.artist),
        q("track_name", &song.title)
    );
    if let Some(album) = song.album.as_deref().filter(|a| !a.is_empty()) {
        url.push('&');
        url.push_str(&q("album_name", album));
    }
    if dur > 0.0 {
        url.push('&');
        url.push_str(&q("duration", &dur.to_string()));
    }
    let d = get(web, &url, failed).await;
    let d = d.as_ref();
    let plain = d.filter(|d| truthy(d.get("plainLyrics"))).map(|d| Found {
        source: "LRCLIB".into(),
        synced: false,
        words: false,
        lines: plain_lines(&string(d.get("plainLyrics"))),
    });
    if let Some(d) = d.filter(|d| truthy(d.get("syncedLyrics"))) {
        return Some(timed(&string(d.get("syncedLyrics"))));
    }
    // No exact match: search by a cleaned title and the first artist.
    let search = format!(
        "https://lrclib.net/api/search?{}&{}",
        q("track_name", &clean_title(&song.title)),
        q("artist_name", first_artist(&song.artist))
    );
    let list = get(web, &search, failed).await;
    let mut best: Option<(f64, &Value)> = None;
    for e in list
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if !truthy(e.get("syncedLyrics")) {
            continue;
        }
        let off = if dur > 0.0 {
            (number(e.get("duration")) - dur).abs()
        } else {
            0.0
        };
        if off <= 3.0 && best.is_none_or(|(b, _)| off < b) {
            best = Some((off, e));
        }
    }
    match best {
        Some((_, e)) => Some(timed(&string(e.get("syncedLyrics")))),
        None => plain,
    }
}
