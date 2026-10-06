//! The `next` request: YouTube Music's queue. A song, a playlist or an album in, the queue's
//! songs out, plus the token for the next page of an endless (radio) queue.
//!
//! It goes out as the music web client (`clients::WEB_REMIX`) to music.youtube.com, which is
//! the client whose answers carry the music details (artists, album, length). As with
//! `player`, the answer becomes our own types at once.

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::player::{RawText, RawThumbnails, widest_thumbnail};
use super::{Innertube, clients};
use crate::browse::{self, LikeStatus};
use crate::error::Error;
use crate::streams::is_video_id;

/// What to ask `next` for. Every field is optional and only the ones set are sent.
///
/// - A song's radio: `video_id` plus `playlist_id` `"RDAMVM" + video_id`.
/// - An album or playlist: `playlist_id` alone (with a `video_id` too, YouTube answers with
///   just that song and an automix preview).
/// - The next page of a radio: `continuation` alone, from the previous page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NextRequest {
    pub video_id: Option<String>,
    pub playlist_id: Option<String>,
    /// Where in the playlist to start.
    pub index: Option<u32>,
    /// The opaque `params` of a watch endpoint (YouTube uses it to pick the queue's flavour).
    pub params: Option<String>,
    pub continuation: Option<String>,
}

/// One page of a queue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NextPage {
    /// The songs, in queue order.
    pub items: Vec<SongItem>,
    /// The token for the next page; `None` when the queue is finite (an album, a playlist).
    pub continuation: Option<String>,
    /// The queue's own playlist id (`playlistPanelRenderer.playlistId`).
    pub playlist_id: Option<String>,
    /// The like status of the song the request named (`NextRequest::video_id`), from the
    /// answer's like button: the queue fetch already makes this request, so the engine needs
    /// no second one for that song (ruling P1). `None` when the request named no song (a
    /// playlist or a continuation, whose answer may be about some other song) or the answer
    /// has no button for it.
    pub like: Option<LikeStatus>,
}

/// One song in a queue. Serializable because the queue is saved across restarts and sent to
/// the bar widgets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SongItem {
    pub video_id: String,
    pub title: String,
    /// Each artist once, in the byline's order, with any `" - Topic"` suffix removed.
    pub artists: Vec<String>,
    pub album: Option<String>,
    /// The widest thumbnail on an allowed host.
    pub thumbnail: Option<String>,
    /// 0 when the answer gives no length, which is how an unplayable item shows.
    pub length_seconds: u32,
    /// The playlist this item was queued from (its watch endpoint's `playlistId`).
    pub playlist_id: Option<String>,
}

/// An artist name without the `" - Topic"` suffix of YouTube's auto-generated artist channels.
/// Only one trailing suffix is removed.
pub fn clean_artist(name: &str) -> String {
    name.strip_suffix(" - Topic").unwrap_or(name).to_string()
}

impl Innertube {
    /// One page of a queue (see `NextRequest` for what to ask).
    ///
    /// Errors: `SignedOut` for no session or a 401, `Unavailable` when the answer holds no
    /// queue, `Network` for transport trouble or an answer over 32 MiB, `Internal` for an
    /// answer that is not JSON.
    pub async fn next(&self, req: NextRequest) -> Result<NextPage, Error> {
        let client = &clients::WEB_REMIX;
        let body = request_body(client, &req);
        let answer = self.post(client, "next", &body).await?;
        parse(&answer, req.video_id.as_deref())
    }
}

/// The request body: the music web client's context, the audio-only flag, and the fields
/// of `req` that are set.
pub(super) fn request_body(client: &clients::ClientInfo, req: &NextRequest) -> serde_json::Value {
    let mut body = json!({
        "context": context(client),
        // As the music app's audio mode asks: the queue then prefers the song over its
        // music video.
        "isAudioOnly": true,
    });
    let fields = [
        ("videoId", req.video_id.as_ref().map(|v| json!(v))),
        ("playlistId", req.playlist_id.as_ref().map(|v| json!(v))),
        ("index", req.index.map(|v| json!(v))),
        ("params", req.params.as_ref().map(|v| json!(v))),
        ("continuation", req.continuation.as_ref().map(|v| json!(v))),
    ];
    if let Some(o) = body.as_object_mut() {
        for (key, value) in fields {
            if let Some(value) = value {
                o.insert(key.into(), value);
            }
        }
    }
    body
}

/// The `context` every music web request carries (`next`, and the browsing requests in
/// `browse.rs`): the client, its version and the language. One builder, so a browse can
/// never go out as a different client version than the queue.
pub(super) fn context(client: &clients::ClientInfo) -> serde_json::Value {
    json!({
        "client": {
            "clientName": client.name,
            "clientVersion": client.version,
            "hl": "en",
        }
    })
}

// The answer, only the parts we read. Everything is optional, as in `player.rs`.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    contents: Option<RawContents>,
    /// Where a continuation page puts its queue.
    continuation_contents: Option<RawContinuationContents>,
    /// The player's buttons, the like button among them. Kept raw (it is small) and read by
    /// `browse::parse_like_for`, the one reader of a like button.
    player_overlays: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContents {
    single_column_music_watch_next_results_renderer: Option<RawWatchNext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWatchNext {
    tabbed_renderer: Option<RawTabbed>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTabbed {
    watch_next_tabbed_results_renderer: Option<RawTabs>,
}

#[derive(Deserialize)]
struct RawTabs {
    #[serde(default)]
    tabs: Vec<RawTab>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTab {
    tab_renderer: Option<RawTabRenderer>,
}

#[derive(Deserialize)]
struct RawTabRenderer {
    content: Option<RawTabContent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTabContent {
    music_queue_renderer: Option<RawQueue>,
}

#[derive(Deserialize)]
struct RawQueue {
    content: Option<RawQueueContent>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawQueueContent {
    playlist_panel_renderer: Option<RawPanel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContinuationContents {
    playlist_panel_continuation: Option<RawPanel>,
}

/// `playlistPanelRenderer` (a first page) or `playlistPanelContinuation` (a later one): the
/// two have the same shape.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPanel {
    playlist_id: Option<String>,
    /// Kept as raw values and parsed one by one, so one odd item is skipped instead of
    /// failing the whole page.
    #[serde(default)]
    contents: Vec<serde_json::Value>,
    #[serde(default)]
    continuations: Vec<RawContinuation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawContinuation {
    /// A radio's next page.
    next_radio_continuation_data: Option<RawContinuationData>,
    /// A long playlist's next page.
    next_continuation_data: Option<RawContinuationData>,
}

#[derive(Deserialize)]
struct RawContinuationData {
    continuation: Option<String>,
}

/// One queue entry. Anything else (an `automixPreviewVideoRenderer`, which is a suggestion,
/// not a song) has neither field and is skipped.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawItem {
    playlist_panel_video_renderer: Option<RawVideo>,
    /// A song with a music-video twin: the song is `primaryRenderer`; the twin
    /// (`counterpart`) is not read.
    playlist_panel_video_wrapper_renderer: Option<RawWrapper>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWrapper {
    primary_renderer: Option<RawPrimary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPrimary {
    playlist_panel_video_renderer: Option<RawVideo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawVideo {
    video_id: Option<String>,
    title: Option<RawText>,
    long_byline_text: Option<RawByline>,
    thumbnail: Option<RawThumbnails>,
    length_text: Option<RawText>,
    navigation_endpoint: Option<RawNavigation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawNavigation {
    watch_endpoint: Option<RawWatchEndpoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawWatchEndpoint {
    video_id: Option<String>,
    playlist_id: Option<String>,
}

#[derive(Deserialize)]
struct RawByline {
    #[serde(default)]
    runs: Vec<RawBylineRun>,
}

/// One piece of the byline: a name (maybe linked to its page) or a separator.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBylineRun {
    text: Option<String>,
    navigation_endpoint: Option<RawBrowseNavigation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBrowseNavigation {
    browse_endpoint: Option<RawBrowseEndpoint>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBrowseEndpoint {
    browse_endpoint_context_supported_configs: Option<RawBrowseConfigs>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBrowseConfigs {
    browse_endpoint_context_music_config: Option<RawMusicConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMusicConfig {
    page_type: Option<String>,
}

const PAGE_ARTIST: &str = "MUSIC_PAGE_TYPE_ARTIST";
const PAGE_ALBUM: &str = "MUSIC_PAGE_TYPE_ALBUM";

impl RawBylineRun {
    /// The linked page's type (`MUSIC_PAGE_TYPE_…`), when the run is a link.
    fn page_type(&self) -> Option<&str> {
        self.navigation_endpoint
            .as_ref()?
            .browse_endpoint
            .as_ref()?
            .browse_endpoint_context_supported_configs
            .as_ref()?
            .browse_endpoint_context_music_config
            .as_ref()?
            .page_type
            .as_deref()
    }
}

/// Parses a `next` answer: a first page or a continuation page.
/// `video_id` is the song the request named, if any: the answer's like button is read for it.
fn parse(answer: &[u8], video_id: Option<&str>) -> Result<NextPage, Error> {
    // Fixed text: serde_json's message can quote part of the answer.
    let raw: Raw = serde_json::from_slice(answer)
        .map_err(|_| Error::Internal("the next answer could not be read".into()))?;

    let like = match (video_id, raw.player_overlays) {
        (Some(id), Some(overlays)) => {
            browse::parse_like_for(&json!({ "playerOverlays": overlays }), id)
        }
        _ => None,
    };
    // A continuation answer can also carry `contents` (the tab headers), so its own queue
    // is looked at first.
    let panel = raw
        .continuation_contents
        .and_then(|c| c.playlist_panel_continuation)
        .or_else(|| {
            raw.contents?
                .single_column_music_watch_next_results_renderer?
                .tabbed_renderer?
                .watch_next_tabbed_results_renderer?
                .tabs
                .into_iter()
                .find_map(|t| {
                    t.tab_renderer?
                        .content?
                        .music_queue_renderer?
                        .content?
                        .playlist_panel_renderer
                })
        });
    let Some(panel) = panel else {
        return Err(Error::Unavailable("YouTube sent no queue".into()));
    };

    let continuation = panel.continuations.into_iter().find_map(|c| {
        c.next_radio_continuation_data
            .or(c.next_continuation_data)
            .and_then(|d| d.continuation)
            .filter(|t| !t.is_empty())
    });
    let items = panel
        .contents
        .into_iter()
        .filter_map(|v| serde_json::from_value::<RawItem>(v).ok())
        .filter_map(|item| {
            item.playlist_panel_video_renderer.or_else(|| {
                item.playlist_panel_video_wrapper_renderer?
                    .primary_renderer?
                    .playlist_panel_video_renderer
            })
        })
        .filter_map(song)
        .collect();

    Ok(NextPage {
        items,
        continuation,
        playlist_id: panel.playlist_id,
        like,
    })
}

/// One song, or `None` without a valid video id. An unavailable song (no length) is kept:
/// the queue shows it, and playing it reports why it can't play.
fn song(v: RawVideo) -> Option<SongItem> {
    let watch = v.navigation_endpoint.and_then(|n| n.watch_endpoint);
    let (watch_id, playlist_id) = match watch {
        Some(w) => (w.video_id, w.playlist_id),
        None => (None, None),
    };
    // The id ends up in a URL and a yt-dlp argument, where `&` or `/` would change what is
    // asked for; the control socket applies the same check to the ids it is given.
    let video_id = v.video_id.or(watch_id).filter(|id| is_video_id(id))?;
    let (artists, album) = v.long_byline_text.map(byline).unwrap_or_default();
    Some(SongItem {
        video_id,
        title: v.title.map(RawText::text).unwrap_or_default(),
        artists,
        album,
        thumbnail: v.thumbnail.and_then(widest_thumbnail),
        length_seconds: v.length_text.map(|t| parse_length(&t.text())).unwrap_or(0),
        playlist_id,
    })
}

/// True for the runs between names: `" • "` between sections, `" & "` and `", "` between
/// artists.
fn is_separator(text: &str) -> bool {
    matches!(text.trim(), "" | "•" | "&" | ",")
}

/// The artists and the album from a byline such as `Artist & Artist • Album • 2024`.
///
/// The artists are the names before the first `" • "` (linked or not: a featured artist
/// often has no link, and a user upload's channel links to a channel page, not an artist),
/// plus any artist link further on. The album is the run linked to an album page.
fn byline(b: RawByline) -> (Vec<String>, Option<String>) {
    let album = b
        .runs
        .iter()
        .find(|r| r.page_type() == Some(PAGE_ALBUM))
        .and_then(|r| r.text.clone());

    let first_section = b
        .runs
        .iter()
        .take_while(|r| r.text.as_deref().is_none_or(|t| t.trim() != "•"))
        .count();
    let mut artists: Vec<String> = Vec::new();
    for (i, run) in b.runs.iter().enumerate() {
        let Some(text) = run.text.as_deref() else {
            continue;
        };
        let page = run.page_type();
        let is_artist = if i < first_section {
            !is_separator(text) && page != Some(PAGE_ALBUM)
        } else {
            page == Some(PAGE_ARTIST)
        };
        if is_artist {
            let name = clean_artist(text.trim());
            if !name.is_empty() && !artists.contains(&name) {
                artists.push(name);
            }
        }
    }
    (artists, album)
}

/// `"m:ss"` or `"h:mm:ss"` in seconds; 0 for anything else.
fn parse_length(text: &str) -> u32 {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return 0;
    }
    parts
        .iter()
        .try_fold(0u32, |total, part| {
            // Digits only: `u32::from_str` would also take a leading `+`.
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            total.checked_mul(60)?.checked_add(part.parse().ok()?)
        })
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lengths() {
        assert_eq!(parse_length("3:05"), 185);
        assert_eq!(parse_length("13:42"), 822);
        assert_eq!(parse_length("1:02:03"), 3723);
        assert_eq!(parse_length(" 0:07 "), 7);
        for bad in [
            "",
            "45",
            "1:2:3:4",
            "a:bc",
            "1::2",
            "+1:05",
            "-1:05",
            "99999999999:00",
        ] {
            assert_eq!(parse_length(bad), 0, "{bad:?}");
        }
    }

    #[test]
    fn body_sends_only_what_is_set() {
        let body = request_body(&clients::WEB_REMIX, &NextRequest::default());
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["context", "isAudioOnly"]);
    }

    #[test]
    fn queue_item_thumbnail_is_sent_in_its_checked_form() {
        let v: RawVideo = serde_json::from_value(json!({
            "videoId": "abcdefghijk",
            "thumbnail": {"thumbnails": [
                {"url": "//lh3.googleusercontent.com\\@evil.example/a\t=w120", "width": 120}
            ]}
        }))
        .unwrap();
        assert_eq!(
            song(v).unwrap().thumbnail.as_deref(),
            Some("https://lh3.googleusercontent.com/@evil.example/a=w120")
        );
    }

    #[test]
    fn repeated_artist_listed_once() {
        let b: RawByline = serde_json::from_value(json!({"runs": [
            {"text": "Same"}, {"text": " & "}, {"text": "Same - Topic"}, {"text": " • "},
            {"text": "2024"}
        ]}))
        .unwrap();
        assert_eq!(byline(b), (vec!["Same".to_string()], None));
    }
}
