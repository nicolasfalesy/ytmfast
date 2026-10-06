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
    /// The answer's `responseContext.visitorData`: the pings send it back as
    /// `X-Goog-Visitor-Id`. It identifies the visitor, so it is treated like a session value
    /// (never logged). Only the music web client's answer (`play_tracking`) fills it.
    pub visitor_data: Option<String>,
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
            .field("visitor_data", &redacted(&self.visitor_data))
            .finish()
    }
}

impl Innertube {
    /// The `player` answer for `video_id`. `sts` is the signature timestamp of the current
    /// player script; YouTube only hands out ciphers that script can solve.
    ///
    /// Errors: `SignedOut` for no session or a plain LOGIN_REQUIRED (not the bot check, a
    /// private video or an age check, which are `StreamFailed`),
    /// `StreamFailed(reason)` when the TV client won't play it or answers for another video
    /// (yt-dlp may still get it), `Network` for transport trouble or an answer over 32 MiB.
    pub async fn player(&self, video_id: &str, sts: u32) -> Result<PlayerResponse, Error> {
        let client = &clients::TV;
        let body = request_body(client, video_id, sts);
        let answer = self.post(client, "player", &body).await?;
        let parsed = parse(&answer, video_id)?;
        if parsed.title.is_empty() {
            // Once per run: which shape came back, so a missing title can be traced (keys
            // only; the resolver then asks oEmbed for the details).
            static NOTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if !NOTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                eprintln!(
                    "ytmfast: the player answer has no title ({})",
                    answer_shape(&answer)
                );
            }
        }
        Ok(parsed)
    }
}

impl Innertube {
    /// The song's play-history links, from the music web client's `player` answer (sent to
    /// music.youtube.com), and the visitor id the pings must carry.
    ///
    /// Why a second `player` request: the TV answer's links answer 204 but never reach the
    /// YouTube Music history; this one, with exactly this body and the pings' headers in
    /// `Innertube::ping`, appeared in the history within 10 s in the step-2 spike (ledger,
    /// "T7 spike result", variant 2). `sts` is the current player script's timestamp, as for
    /// the TV request; the spike's working variant sent it and the one without it did not
    /// count, so it stays.
    ///
    /// Errors: as `player`; `Unavailable` when the answer has no playback link (or only links
    /// off the allowlist).
    pub async fn play_tracking(&self, video_id: &str, sts: u32) -> Result<Tracking, Error> {
        let client = &clients::WEB_REMIX;
        let body = tracking_body(client, video_id, sts);
        let answer = self.post(client, "player", &body).await?;
        parse_tracking(&answer, |u| self.target_allows(u))
    }
}

/// The spike's variant 2: the client, the song and the signature timestamp, nothing else.
fn tracking_body(client: &clients::ClientInfo, video_id: &str, sts: u32) -> serde_json::Value {
    json!({
        "context": {
            "client": {
                "clientName": client.name,
                "clientVersion": client.version,
                "hl": "en",
            }
        },
        "videoId": video_id,
        "playbackContext": {
            "contentPlaybackContext": {
                "signatureTimestamp": sts,
            }
        },
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTrackingAnswer {
    playback_tracking: Option<RawTracking>,
    response_context: Option<RawResponseContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawResponseContext {
    visitor_data: Option<String>,
}

/// The longest visitor id kept. Real ones are about 30-80 characters; the cap only stops an
/// odd answer from putting a huge value into every ping's headers.
const MAX_VISITOR_DATA: usize = 512;

/// The tracking links of a music web `player` answer. `allowed` checks every link (ruling
/// R7): the allowlist in production, the test server in tests.
fn parse_tracking(answer: &[u8], allowed: impl Fn(&Url) -> bool) -> Result<Tracking, Error> {
    // Fixed text: serde_json's message can quote part of the answer.
    let raw: RawTrackingAnswer = serde_json::from_slice(answer)
        .map_err(|_| Error::Internal("the player answer could not be read".into()))?;
    let ok = |b: Option<RawBaseUrl>| {
        b.and_then(|b| b.base_url)
            .filter(|u| Url::parse(u).is_ok_and(|u| allowed(&u)))
    };
    let (playback_url, watchtime_url) = match raw.playback_tracking {
        Some(t) => (
            ok(t.videostats_playback_url),
            ok(t.videostats_watchtime_url),
        ),
        None => (None, None),
    };
    if playback_url.is_none() {
        return Err(Error::Unavailable("no play-history link".into()));
    }
    // Only what can go into a header as it is: visible ASCII (it is base64 and %-escapes).
    let visitor_data = raw
        .response_context
        .and_then(|c| c.visitor_data)
        .filter(|v| {
            !v.is_empty() && v.len() <= MAX_VISITOR_DATA && v.bytes().all(|b| b.is_ascii_graphic())
        });
    Ok(Tracking {
        playback_url,
        watchtime_url,
        visitor_data,
    })
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
    microformat: Option<RawMicroformat>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMicroformat {
    player_microformat_renderer: Option<RawMicroformatRenderer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMicroformatRenderer {
    title: Option<RawText>,
    owner_channel_name: Option<String>,
    length_seconds: Option<String>,
}

/// YouTube's text object: `{"simpleText": …}` or `{"runs": [{"text": …}, …]}`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RawText {
    simple_text: Option<String>,
    #[serde(default)]
    runs: Vec<RawRun>,
}

#[derive(Deserialize)]
struct RawRun {
    text: Option<String>,
}

impl RawText {
    pub(super) fn text(self) -> String {
        match self.simple_text {
            Some(t) => t,
            None => self.runs.into_iter().filter_map(|r| r.text).collect(),
        }
    }
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
pub(super) struct RawThumbnails {
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

/// The answer's shape for a log line: its top-level keys and `videoDetails`' keys, sorted.
/// Keys only, never values (the answer holds signed links).
pub(crate) fn answer_shape(answer: &[u8]) -> String {
    let keys = |v: Option<&serde_json::Value>| -> String {
        match v.and_then(|v| v.as_object()) {
            Some(o) => {
                let mut k: Vec<&str> = o.keys().map(String::as_str).collect();
                k.sort_unstable();
                k.join(", ")
            }
            None => "none".into(),
        }
    };
    let v: Option<serde_json::Value> = serde_json::from_slice(answer).ok();
    format!(
        "keys: {}; videoDetails: {}",
        keys(v.as_ref()),
        keys(v.as_ref().and_then(|v| v.get("videoDetails")))
    )
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
    // Only a plain sign-in refusal means the session is bad. Every other refusal is
    // `StreamFailed`, so the resolver asks yt-dlp (ruling S4, revising step 1's R26): this is
    // the TV client's answer alone, and yt-dlp asks other clients, which may play what the TV
    // client won't. The bot check, a private video and an age check come as LOGIN_REQUIRED
    // too, but they are about this client or this song, not the session: calling them
    // `signed_out` would stop the queue and send the user to re-import a session that is fine.
    match status {
        "OK" => {}
        "LOGIN_REQUIRED" if !not_about_the_session(playability.reason.as_deref()) => {
            return Err(Error::SignedOut);
        }
        _ => return Err(Error::StreamFailed(reason(playability.reason, status))),
    }

    let details = raw.video_details;
    let answered_for = details.as_ref().and_then(|d| d.video_id.as_deref());
    // YouTube sometimes answers with another video's data; yt-dlp skips such answers too. A
    // rare transient, so `StreamFailed`: yt-dlp's own request will most likely be answered
    // right.
    if answered_for.is_some_and(|id| id != video_id) {
        return Err(Error::StreamFailed(
            "YouTube answered for a different video".into(),
        ));
    }
    let (title, author, length_seconds, thumbnail) = match details {
        Some(d) => (
            d.title,
            d.author,
            d.length_seconds.and_then(|s| s.parse().ok()),
            d.thumbnail.and_then(widest_thumbnail),
        ),
        None => (None, None, None, None),
    };
    // Each detail from `videoDetails` first, else from the microformat (some clients send
    // only one of the two).
    let micro = raw.microformat.and_then(|m| m.player_microformat_renderer);
    let (micro_title, micro_author, micro_length) = match micro {
        Some(m) => (
            m.title.map(RawText::text),
            m.owner_channel_name,
            m.length_seconds.and_then(|s| s.parse().ok()),
        ),
        None => (None, None, None),
    };
    let present = |s: Option<String>| s.filter(|s| !s.is_empty());
    let title = present(title).or(present(micro_title)).unwrap_or_default();
    let author = present(author)
        .or(present(micro_author))
        .unwrap_or_default();
    let length_seconds = length_seconds.or(micro_length).unwrap_or_default();

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
            visitor_data: None,
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

/// Whether a LOGIN_REQUIRED reason is about something other than the session: the "Sign in to
/// confirm you're not a bot" check, a private video ("This is a private video…"), or an age
/// check ("Sign in to confirm your age", "age-restricted", "inappropriate for some users").
/// Matched on fragments in any case, so either apostrophe YouTube uses (and any text around
/// them) fits. A bare LOGIN_REQUIRED, or a plain "sign in" reason, is the session.
fn not_about_the_session(reason: Option<&str>) -> bool {
    const FRAGMENTS: [&str; 6] = [
        "not a bot",
        "private video",
        "confirm your age",
        "age-restricted",
        "age restricted",
        "inappropriate for some users",
    ];
    reason.is_some_and(|r| {
        let r = r.to_lowercase();
        FRAGMENTS.iter().any(|f| r.contains(f))
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
pub(super) fn widest_thumbnail(t: RawThumbnails) -> Option<String> {
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
    fn other_statuses_are_tv_refusals() {
        // The TV client is the one client asked: its refusal may not be another client's, so
        // these are stream_failed and yt-dlp gets a try.
        for status in ["ERROR", "AGE_CHECK_REQUIRED", "LIVE_STREAM_OFFLINE"] {
            let a = answer(json!({"playabilityStatus": {"status": status}}));
            assert_eq!(parse(&a, "x"), Err(Error::StreamFailed(status.into())));
        }
        let a = answer(
            json!({"playabilityStatus": {"status": "ERROR", "reason": "Video unavailable"}}),
        );
        assert_eq!(
            parse(&a, "x"),
            Err(Error::StreamFailed("Video unavailable".into()))
        );
    }

    #[test]
    fn login_required_is_signed_out_unless_a_bot_check() {
        let a = answer(json!({"playabilityStatus": {"status": "LOGIN_REQUIRED"}}));
        assert_eq!(parse(&a, "x"), Err(Error::SignedOut));
        // Both apostrophes YouTube uses, any case.
        for reason in [
            "Sign in to confirm you're not a bot",
            "Sign in to confirm you\u{2019}re not a bot",
            "SIGN IN TO CONFIRM YOU'RE NOT A BOT. This helps protect our community.",
        ] {
            let a = answer(json!({"playabilityStatus": {
                "status": "LOGIN_REQUIRED", "reason": reason}}));
            assert_eq!(
                parse(&a, "x").unwrap_err().code(),
                "stream_failed",
                "{reason}"
            );
        }
    }

    #[test]
    fn login_required_for_a_private_or_age_checked_song_is_that_songs_failure() {
        // These refusals are about the song, not the session: the song is skipped, and the
        // user is not sent to import a session that works.
        for reason in [
            "This is a private video. Please sign in to verify that you may see it.",
            "PRIVATE VIDEO",
            "Sign in to confirm your age",
            "Sign in to confirm your age. This video may be inappropriate for some users.",
            "This video may be inappropriate for some users.",
            "Age-restricted video (based on Community Guidelines)",
            "This video is age restricted",
        ] {
            let a = answer(json!({"playabilityStatus": {
                "status": "LOGIN_REQUIRED", "reason": reason}}));
            assert_eq!(
                parse(&a, "x").unwrap_err().code(),
                "stream_failed",
                "{reason}"
            );
        }
        // A plain sign-in refusal (or none at all) is still the session.
        for reason in ["", "Sign in to continue", "Please sign in"] {
            let a = answer(json!({"playabilityStatus": {
                "status": "LOGIN_REQUIRED", "reason": reason}}));
            assert_eq!(parse(&a, "x"), Err(Error::SignedOut), "{reason:?}");
        }
    }

    #[test]
    fn reason_never_carries_a_url() {
        let a = answer(json!({"playabilityStatus": {
            "status": "UNPLAYABLE",
            "reason": "see https://rr1---sn-test.googlevideo.com/videoplayback?sig=FAKE"
        }}));
        assert_eq!(
            parse(&a, "x"),
            Err(Error::StreamFailed("UNPLAYABLE".into()))
        );
        assert_eq!(reason(Some("y".repeat(500)), "ERROR").len(), 200);
        assert_eq!(reason(None, ""), "unknown");
    }

    #[test]
    fn details_fall_back_to_the_microformat() {
        // Some clients answer with the microformat and no `videoDetails`.
        let a = answer(json!({
            "playabilityStatus": {"status": "OK"},
            "microformat": {"playerMicroformatRenderer": {
                "title": {"runs": [{"text": "Micro "}, {"text": "Song"}]},
                "ownerChannelName": "Micro Artist",
                "lengthSeconds": "187"
            }}
        }));
        let p = parse(&a, "x").unwrap();
        assert_eq!(
            (p.title.as_str(), p.author.as_str(), p.length_seconds),
            ("Micro Song", "Micro Artist", 187)
        );
        let a = answer(json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "x", "title": "Details Song", "lengthSeconds": "10"},
            "microformat": {"playerMicroformatRenderer": {
                "title": {"simpleText": "Micro Song"}, "ownerChannelName": "Micro Artist"
            }}
        }));
        let p = parse(&a, "x").unwrap();
        // Each field from the details first, the microformat only for what they lack.
        assert_eq!(
            (p.title.as_str(), p.author.as_str(), p.length_seconds),
            ("Details Song", "Micro Artist", 10)
        );
    }

    #[test]
    fn shape_names_keys_never_values() {
        let a = json!({
            "playabilityStatus": {"status": "OK"},
            "streamingData": {"adaptiveFormats": [{"url": "https://x/?sig=SECRET"}]},
            "videoDetails": {"videoId": "x", "isPrivate": false}
        });
        let shape = answer_shape(&serde_json::to_vec(&a).unwrap());
        assert_eq!(
            shape,
            "keys: playabilityStatus, streamingData, videoDetails; videoDetails: isPrivate, videoId"
        );
        assert!(!shape.contains("SECRET"));
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

    #[test]
    fn tracking_answer_links_are_checked() {
        let a = answer(json!({
            "responseContext": {"visitorData": "CgtWaXNpdG9y%3D%3D"},
            "playbackTracking": {
                "videostatsPlaybackUrl": {"baseUrl": "https://s.youtube.com/api/stats/playback?docid=x"},
                "videostatsWatchtimeUrl": {"baseUrl": "https://evil.example/api/stats/watchtime"}
            }
        }));
        let t = parse_tracking(&a, net::allowed_host).unwrap();
        assert_eq!(
            t.playback_url.as_deref(),
            Some("https://s.youtube.com/api/stats/playback?docid=x")
        );
        assert_eq!(t.watchtime_url, None);
        assert_eq!(t.visitor_data.as_deref(), Some("CgtWaXNpdG9y%3D%3D"));
        // The visitor id never shows in Debug output.
        assert!(!format!("{t:?}").contains("CgtW"));
    }

    #[test]
    fn no_playback_link_is_unavailable() {
        let a = answer(json!({"playbackTracking": {
            "videostatsPlaybackUrl": {"baseUrl": "http://s.youtube.com/api/stats/playback"}
        }}));
        assert_eq!(
            parse_tracking(&a, net::allowed_host),
            Err(Error::Unavailable("no play-history link".into()))
        );
        let a = answer(json!({"playabilityStatus": {"status": "OK"}}));
        assert!(parse_tracking(&a, net::allowed_host).is_err());
    }

    #[test]
    fn odd_visitor_ids_are_dropped() {
        for bad in [
            "",
            "has space",
            "line\nbreak",
            &"x".repeat(MAX_VISITOR_DATA + 1),
        ] {
            let a = answer(json!({
                "responseContext": {"visitorData": bad},
                "playbackTracking": {"videostatsPlaybackUrl":
                    {"baseUrl": "https://s.youtube.com/api/stats/playback"}}
            }));
            assert_eq!(
                parse_tracking(&a, net::allowed_host).unwrap().visitor_data,
                None,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn tracking_body_is_the_spike_recipe() {
        let b = tracking_body(&clients::WEB_REMIX, "abc", 20725);
        assert_eq!(
            b,
            json!({
                "context": {"client": {"clientName": "WEB_REMIX",
                    "clientVersion": clients::WEB_REMIX.version, "hl": "en"}},
                "videoId": "abc",
                "playbackContext": {"contentPlaybackContext": {"signatureTimestamp": 20725}}
            })
        );
    }
}
