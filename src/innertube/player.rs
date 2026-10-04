//! The `player` request: a video id in, the song's details, audio formats, loudness and
//! play-history URLs out.
//!
//! It goes out as the TV client (`clients::TV`) with the session, because that is the client
//! that gives the Premium formats. The answer is parsed into our own types right away, so the
//! rest of the engine never sees YouTube's JSON.

use std::fmt;

use serde::Deserialize;
use serde_json::json;
use url::Url;

use super::{Innertube, clients};
use crate::error::Error;
use crate::net;

/// What the `player` answer gives for one song.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerResponse {
    pub video_id: String,
    pub title: String,
    pub author: String,
    pub length_seconds: u32,
    /// The widest thumbnail.
    pub thumbnail: Option<String>,
    /// `playerConfig.audioConfig.loudnessDb`: how far the song is above YouTube's loudness
    /// target, for the volume normalisation gain.
    pub loudness_db: Option<f32>,
    /// Audio-only formats, in the answer's order.
    pub formats: Vec<AudioFormat>,
    pub tracking: Tracking,
}

/// One audio-only stream. Exactly one of `url` (ready, apart from the `n` challenge) and
/// `signature_cipher` (needs the signature solved first) is set.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioFormat {
    pub itag: u32,
    pub mime: String,
    pub bitrate: u32,
    pub content_length: Option<u64>,
    pub url: Option<String>,
    pub signature_cipher: Option<String>,
}

/// The play-history URLs the report module pings so plays count on the account.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Tracking {
    pub playback_url: Option<String>,
    pub watchtime_url: Option<String>,
}

/// Shows whether a URL is there, never the URL: stream links carry access tokens, and these
/// types end up in logs, panics and test failures.
fn redacted(v: &Option<String>) -> &'static str {
    if v.is_some() { "<redacted>" } else { "None" }
}

impl fmt::Debug for AudioFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioFormat")
            .field("itag", &self.itag)
            .field("mime", &self.mime)
            .field("bitrate", &self.bitrate)
            .field("content_length", &self.content_length)
            .field("url", &redacted(&self.url))
            .field("signature_cipher", &redacted(&self.signature_cipher))
            .finish()
    }
}

impl fmt::Debug for Tracking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tracking")
            .field("playback_url", &redacted(&self.playback_url))
            .field("watchtime_url", &redacted(&self.watchtime_url))
            .finish()
    }
}

impl Innertube {
    /// The `player` answer for `video_id`. `sts` is the signature timestamp of the current
    /// player script; YouTube only hands out ciphers that script can solve.
    ///
    /// Errors: `SignedOut` for no session or LOGIN_REQUIRED, `Unavailable(reason)` when
    /// YouTube won't play it, `Network` for transport trouble or an answer over 32 MiB.
    pub async fn player(&self, video_id: &str, sts: u32) -> Result<PlayerResponse, Error> {
        let client = &clients::TV;
        let body = request_body(client, video_id, sts);
        let answer = self.post(client, "player", &body).await?;
        parse(&answer, video_id)
    }
}

/// The request body, as yt-dlp 2026.08.19 sends it for this client.
fn request_body(client: &clients::ClientInfo, video_id: &str, sts: u32) -> serde_json::Value {
    json!({
        "context": {
            "client": {
                "clientName": client.name,
                "clientVersion": client.version,
                "userAgent": client.user_agent,
                "hl": "en",
                "timeZone": "UTC",
                "utcOffsetMinutes": 0,
            }
        },
        "videoId": video_id,
        "playbackContext": {
            "contentPlaybackContext": {
                "html5Preference": "HTML5_PREF_WANTS",
                "signatureTimestamp": sts,
            }
        },
        // Skip the "this may be inappropriate" interstitials; the user picked the song.
        "contentCheckOk": true,
        "racyCheckOk": true,
    })
}

// The answer, only the parts we read. Everything is optional so one odd field doesn't sink
// the whole answer; what is truly needed is checked in `parse`.

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Raw {
    playability_status: Option<RawPlayability>,
    video_details: Option<RawDetails>,
    streaming_data: Option<RawStreaming>,
    player_config: Option<RawPlayerConfig>,
    playback_tracking: Option<RawTracking>,
}

#[derive(Deserialize)]
struct RawPlayability {
    status: Option<String>,
    reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDetails {
    video_id: Option<String>,
    title: Option<String>,
    author: Option<String>,
    /// A number in a string, as YouTube sends it.
    length_seconds: Option<String>,
    thumbnail: Option<RawThumbnails>,
}

#[derive(Deserialize)]
struct RawThumbnails {
    #[serde(default)]
    thumbnails: Vec<RawThumbnail>,
}

#[derive(Deserialize)]
struct RawThumbnail {
    url: Option<String>,
    width: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawStreaming {
    #[serde(default)]
    adaptive_formats: Vec<RawFormat>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawFormat {
    itag: Option<u32>,
    mime_type: Option<String>,
    bitrate: Option<u32>,
    /// A number in a string.
    content_length: Option<String>,
    url: Option<String>,
    signature_cipher: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPlayerConfig {
    audio_config: Option<RawAudioConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawAudioConfig {
    loudness_db: Option<f32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTracking {
    videostats_playback_url: Option<RawBaseUrl>,
    videostats_watchtime_url: Option<RawBaseUrl>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBaseUrl {
    base_url: Option<String>,
}

/// Parses a `player` answer for `video_id`.
fn parse(answer: &[u8], video_id: &str) -> Result<PlayerResponse, Error> {
    // Fixed text: serde_json's message can quote part of the answer, which holds stream links.
    let raw: Raw = serde_json::from_slice(answer)
        .map_err(|_| Error::Internal("the player answer could not be read".into()))?;

    let playability = raw
        .playability_status
        .ok_or_else(|| Error::Internal("the player answer has no playability status".into()))?;
    let status = playability.status.as_deref().unwrap_or("");
    match status {
        "OK" => {}
        "LOGIN_REQUIRED" => return Err(Error::SignedOut),
        _ => return Err(Error::Unavailable(reason(playability.reason, status))),
    }

    let details = raw.video_details;
    let answered_for = details.as_ref().and_then(|d| d.video_id.as_deref());
    // YouTube sometimes answers with another video's data; yt-dlp skips such answers too.
    if answered_for.is_some_and(|id| id != video_id) {
        return Err(Error::Unavailable(
            "YouTube answered for a different video".into(),
        ));
    }
    let (title, author, length_seconds, thumbnail) = match details {
        Some(d) => (
            d.title.unwrap_or_default(),
            d.author.unwrap_or_default(),
            d.length_seconds
                .and_then(|s| s.parse().ok())
                .unwrap_or_default(),
            d.thumbnail.and_then(widest_thumbnail),
        ),
        None => Default::default(),
    };

    let formats = raw
        .streaming_data
        .map(|s| s.adaptive_formats)
        .unwrap_or_default()
        .into_iter()
        .filter_map(audio_format)
        .collect();

    let loudness_db = raw
        .player_config
        .and_then(|c| c.audio_config)
        .and_then(|a| a.loudness_db);

    let tracking = raw
        .playback_tracking
        .map(|t| Tracking {
            playback_url: t.videostats_playback_url.and_then(allowed_base_url),
            watchtime_url: t.videostats_watchtime_url.and_then(allowed_base_url),
        })
        .unwrap_or_default();

    Ok(PlayerResponse {
        video_id: video_id.to_string(),
        title,
        author,
        length_seconds,
        thumbnail,
        loudness_db,
        formats,
        tracking,
    })
}

/// YouTube's reason text for a refusal, else the status itself. It is shown to the user, so
/// it is kept short, and a reason holding a URL is swapped for the status: no error may carry
/// a URL (ruling R6).
fn reason(reason: Option<String>, status: &str) -> String {
    let fallback = if status.is_empty() { "unknown" } else { status };
    let text = reason.as_deref().map(str::trim).unwrap_or("");
    if text.is_empty() || text.contains("://") {
        return fallback.to_string();
    }
    text.chars().take(200).collect()
}

/// An audio-only format (`audio/…` mime), or `None` for video, incomplete entries, and links
/// to hosts off the allowlist (ruling R7: every URL received is checked). A `signatureCipher`
/// holds its URL inside; the streams module checks that one once it has decoded it.
fn audio_format(f: RawFormat) -> Option<AudioFormat> {
    let mime = f.mime_type?;
    if !mime.starts_with("audio/") {
        return None;
    }
    let url = match f.url {
        Some(u) if !url_allowed(&u) => return None,
        other => other,
    };
    if url.is_none() && f.signature_cipher.is_none() {
        return None;
    }
    Some(AudioFormat {
        itag: f.itag?,
        mime,
        bitrate: f.bitrate.unwrap_or(0),
        content_length: f.content_length.and_then(|s| s.parse().ok()),
        url,
        signature_cipher: f.signature_cipher,
    })
}

/// The widest thumbnail with an allowed URL. Some answers give protocol-relative links
/// (`//i.ytimg.com/…`); those are made https.
fn widest_thumbnail(t: RawThumbnails) -> Option<String> {
    t.thumbnails
        .into_iter()
        .filter_map(|t| {
            let url = t.url?;
            let url = match url.strip_prefix("//") {
                Some(rest) => format!("https://{rest}"),
                None => url,
            };
            url_allowed(&url).then_some((t.width.unwrap_or(0), url))
        })
        // `max_by_key` keeps the last of equal widths; the order of equals doesn't matter.
        .max_by_key(|(width, _)| *width)
        .map(|(_, url)| url)
}

fn allowed_base_url(b: RawBaseUrl) -> Option<String> {
    b.base_url.filter(|u| url_allowed(u))
}

fn url_allowed(u: &str) -> bool {
    Url::parse(u).is_ok_and(|u| net::allowed_host(&u))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(v: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn other_statuses_are_unavailable() {
        for status in ["ERROR", "AGE_CHECK_REQUIRED", "LIVE_STREAM_OFFLINE"] {
            let a = answer(json!({"playabilityStatus": {"status": status}}));
            assert_eq!(parse(&a, "x"), Err(Error::Unavailable(status.into())));
        }
        let a = answer(
            json!({"playabilityStatus": {"status": "ERROR", "reason": "Video unavailable"}}),
        );
        assert_eq!(
            parse(&a, "x"),
            Err(Error::Unavailable("Video unavailable".into()))
        );
    }

    #[test]
    fn reason_never_carries_a_url() {
        let a = answer(json!({"playabilityStatus": {
            "status": "UNPLAYABLE",
            "reason": "see https://rr1---sn-test.googlevideo.com/videoplayback?sig=FAKE"
        }}));
        assert_eq!(parse(&a, "x"), Err(Error::Unavailable("UNPLAYABLE".into())));
        assert_eq!(reason(Some("y".repeat(500)), "ERROR").len(), 200);
        assert_eq!(reason(None, ""), "unknown");
    }

    #[test]
    fn missing_status_is_an_error() {
        let a = answer(json!({"videoDetails": {"videoId": "x"}}));
        assert_eq!(parse(&a, "x").unwrap_err().code(), "internal");
    }

    #[test]
    fn ok_with_little_in_it_still_parses() {
        // No details, no streaming data, no loudness: the caller decides what that means.
        let a = answer(json!({"playabilityStatus": {"status": "OK"}}));
        let p = parse(&a, "abc").unwrap();
        assert_eq!(p.video_id, "abc");
        assert!(p.formats.is_empty());
        assert_eq!(p.loudness_db, None);
        assert_eq!(p.thumbnail, None);
        assert_eq!(p.tracking, Tracking::default());
    }

    #[test]
    fn odd_formats_are_skipped_not_fatal() {
        let a = answer(json!({
            "playabilityStatus": {"status": "OK"},
            "streamingData": {"adaptiveFormats": [
                {"itag": 1, "url": "https://rr1---sn-test.googlevideo.com/v"},
                {"mimeType": "audio/mp4", "url": "https://rr1---sn-test.googlevideo.com/v"},
                {"itag": 3, "mimeType": "audio/mp4"},
                {"itag": 4, "mimeType": "audio/mp4", "url": "http://rr1---sn-test.googlevideo.com/v"},
                {"itag": 5, "mimeType": "audio/mp4", "contentLength": "nope",
                 "url": "https://rr1---sn-test.googlevideo.com/v"}
            ]}
        }));
        let p = parse(&a, "x").unwrap();
        // Only itag 5 is whole and on an allowed https host; its bad length is just None.
        assert_eq!(p.formats.len(), 1);
        assert_eq!(p.formats[0].itag, 5);
        assert_eq!(p.formats[0].content_length, None);
        assert_eq!(p.formats[0].bitrate, 0);
    }

    #[test]
    fn protocol_relative_thumbnail_is_made_https() {
        let a = answer(json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "x", "thumbnail": {"thumbnails": [
                {"url": "//i.ytimg.com/vi/x/hq.jpg", "width": 480},
                {"url": "https://evil.example/big.jpg", "width": 4000}
            ]}}
        }));
        assert_eq!(
            parse(&a, "x").unwrap().thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/x/hq.jpg")
        );
    }

    #[test]
    fn off_allowlist_tracking_urls_are_dropped() {
        let a = answer(json!({
            "playabilityStatus": {"status": "OK"},
            "playbackTracking": {
                "videostatsPlaybackUrl": {"baseUrl": "https://evil.example/stats"},
                "videostatsWatchtimeUrl": {"baseUrl": "https://s.youtube.com/api/stats/watchtime"}
            }
        }));
        let t = parse(&a, "x").unwrap().tracking;
        assert_eq!(t.playback_url, None);
        assert_eq!(
            t.watchtime_url.as_deref(),
            Some("https://s.youtube.com/api/stats/watchtime")
        );
    }
}
