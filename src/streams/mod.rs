//! Video id in, playable audio link out.
//!
//! The own-code path: find the current player version (and its signature timestamp), ask
//! InnerTube's `player` for the song as the TV client with the session, pick the format
//! (itag 774 Opus, else 141 AAC, else the highest-bitrate other audio), solve the link's
//! challenges (the `n` parameter, and the signature of a `signatureCipher`) in one solver
//! call, and assemble the link. If any step fails, yt-dlp is asked instead (`ytdlp`); if that
//! fails too, the own-code error is reported, since it says why (signed out, unavailable).
//!
//! Links are cached per video until 30 minutes before their `expire` time.

pub mod ytdlp;

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use url::Url;

use crate::auth::Session;
use crate::error::Error;
use crate::innertube::{AudioFormat, Innertube, PlayerResponse, Tracking, clients};
use crate::net;
use crate::solver::{ChallengeKind, ChallengeSolver, player_js};
use ytdlp::YtDlp;

/// A link is used until this long before its `expire` time, so a song started on it has
/// time to finish downloading.
const EXPIRY_MARGIN_SECS: u64 = 30 * 60;

/// How long the current player version is trusted before `iframe_api` is asked again.
/// YouTube moves to a new version every few days and keeps serving links the previous one can
/// solve, so an hour saves a request per song at no real risk; `resolve_fresh` (used when a
/// link stops working) always asks again.
const PLAYER_ID_TTL: Duration = Duration::from_secs(3600);

/// How long a player version the solver failed on is skipped (straight to yt-dlp) before the
/// own-code path tries it again. The frozen solver scripts may simply not handle a newer
/// player, and each try costs a cold solve (seconds of CPU and a ~190 MiB spike); but the
/// failure may also have been a deadline hit on a throttled CPU, so it isn't forever.
const FAILED_PLAYER_RETRY: Duration = Duration::from_secs(6 * 3600);

/// The marker file `players/{id}.failed`: its mtime is when the solver failed on that
/// version, so a restarted engine doesn't pay for the failure again.
const FAILED_SUFFIX: &str = "failed";

/// oEmbed answers are a few hundred bytes.
const OEMBED_CAP: usize = 64 << 10;

/// Details are cosmetic: don't hold a song's start for long waiting on them.
const OEMBED_TIMEOUT: Duration = Duration::from_secs(3);

/// The parts of an oEmbed answer we read.
#[derive(Deserialize)]
struct OEmbed {
    title: Option<String>,
    author_name: Option<String>,
}

/// The most links kept. A queue rarely holds more songs than this; expired links go first.
const MAX_CACHED_LINKS: usize = 64;

/// A playable link for one song, with what the player and the reports need.
#[derive(Clone, PartialEq)]
pub struct Stream {
    pub video_id: String,
    /// The signed googlevideo link. Carries an access token: never logged.
    pub url: String,
    pub itag: u32,
    pub mime: String,
    pub content_length: Option<u64>,
    /// When to stop using the link: its `expire` value minus 30 minutes (Unix seconds).
    pub expires_unix: u64,
    pub loudness_db: Option<f32>,
    pub meta: TrackMeta,
    pub tracking: Tracking,
}

/// Shows that there is a link, never the link (it carries an access token).
impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Stream")
            .field("video_id", &self.video_id)
            .field("url", &"<redacted>")
            .field("itag", &self.itag)
            .field("mime", &self.mime)
            .field("content_length", &self.content_length)
            .field("expires_unix", &self.expires_unix)
            .field("loudness_db", &self.loudness_db)
            .field("meta", &self.meta)
            .field("tracking", &self.tracking)
            .finish()
    }
}

/// What the bar shows for a song.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackMeta {
    pub title: String,
    pub artist: String,
    pub length_seconds: u32,
    pub thumbnail: Option<String>,
}

/// Turns a video id into a `Stream`. Used as `Arc<dyn Resolver>` (ruling R1).
#[async_trait]
pub trait Resolver: Send + Sync {
    /// A link for `video_id`, from the cache while it is good.
    async fn resolve(&self, video_id: &str) -> Result<Stream, Error>;
    /// A new link, skipping the cache: for when a link stopped working mid-song (ruling R2).
    async fn resolve_fresh(&self, video_id: &str) -> Result<Stream, Error>;
}

/// itag 774 (Opus 256k, Premium), else 141 (AAC 256k, Premium), else the highest-bitrate
/// other audio format.
pub fn pick_format(formats: &[AudioFormat]) -> Option<&AudioFormat> {
    [774, 141]
        .iter()
        .find_map(|itag| formats.iter().find(|f| f.itag == *itag))
        .or_else(|| formats.iter().max_by_key(|f| f.bitrate))
}

/// The resolver: own code first, yt-dlp as the fallback, with a link cache.
pub struct Streams {
    api: Arc<Innertube>,
    session: Arc<Mutex<Session>>,
    solver: Arc<dyn ChallengeSolver>,
    ytdlp: Arc<dyn YtDlp>,
    /// For `iframe_api` and the player script: no session goes on these.
    http: reqwest::Client,
    web_base: Url,
    players_dir: PathBuf,
    /// The current player version and its signature timestamp, and when they were read.
    player: Mutex<Option<(String, u32, Instant)>>,
    links: Mutex<HashMap<String, Stream>>,
    /// Player versions the solver failed on, and when (mirrors the marker files).
    failed_players: Mutex<HashMap<String, SystemTime>>,
}

impl Streams {
    /// `cache_dir` is `paths::cache_dir()` in production: player scripts are kept in its
    /// `players/` folder.
    pub fn new(
        api: Arc<Innertube>,
        session: Arc<Mutex<Session>>,
        solver: Arc<dyn ChallengeSolver>,
        ytdlp: Arc<dyn YtDlp>,
        cache_dir: PathBuf,
    ) -> Streams {
        Streams {
            api,
            session,
            solver,
            ytdlp,
            http: net::client(clients::WEB_REMIX.user_agent),
            web_base: Url::parse(player_js::WEB_BASE).expect("WEB_BASE is a valid URL"),
            players_dir: cache_dir.join("players"),
            player: Mutex::new(None),
            links: Mutex::new(HashMap::new()),
            failed_players: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches the player version and script from `base` instead of `WEB_BASE`. For tests
    /// only: this is the one way past the https allowlist for these requests (ruling R7).
    pub fn with_web_base(mut self, base: Url) -> Streams {
        self.web_base = base;
        self
    }

    /// The own-code path, then yt-dlp.
    async fn fetch(&self, video_id: &str, fresh: bool) -> Result<Stream, Error> {
        let own = match self.own(video_id, fresh).await {
            Ok(stream) => return Ok(stream),
            Err(e) => e,
        };
        // Forget the player version: the next resolve asks for the current one instead of
        // reusing a version that may be why this failed, for up to `PLAYER_ID_TTL`. If it is
        // the same version and the solver failed on it, `current_player` skips it at once.
        *self.player.lock().unwrap_or_else(|e| e.into_inner()) = None;
        // The code only: messages are short and fixed, but the code is all a log needs.
        eprintln!(
            "ytmfast: own stream link failed ({}); trying yt-dlp",
            own.code()
        );
        let session = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        match self.ytdlp.info_json(video_id, &session).await {
            Ok(json) => match from_ytdlp(&json, video_id) {
                Ok(stream) => Ok(stream),
                Err(e) => {
                    eprintln!("ytmfast: yt-dlp's answer was refused ({})", e.code());
                    Err(own)
                }
            },
            Err(e) => {
                eprintln!("ytmfast: yt-dlp failed too ({})", e.code());
                Err(own)
            }
        }
    }

    async fn own(&self, video_id: &str, fresh: bool) -> Result<Stream, Error> {
        let (player_id, sts, mut code) = self.current_player(fresh).await?;
        let answer = self.api.player(video_id, sts).await?;
        let format = pick_format(&answer.formats)
            .ok_or_else(|| Error::Unavailable("no audio format".into()))?;
        let link = Link::of(format)?;

        // Both kinds in one solver call: one `jsc` run, one pass over the player.
        let mut requests = Vec::new();
        if let Some(n) = &link.n {
            requests.push((ChallengeKind::N, vec![n.clone()]));
        }
        if let Some(cipher) = &link.cipher {
            requests.push((ChallengeKind::Sig, vec![cipher.s.clone()]));
        }
        let mut solved_n = None;
        let mut solved_sig = None;
        if !requests.is_empty() {
            if code.is_none() && !self.solver.has_player(&player_id) {
                code = Some(self.player_code(&player_id).await?);
            }
            let asked: Vec<(ChallengeKind, String)> = requests
                .iter()
                .map(|(kind, challenges)| (*kind, challenges[0].clone()))
                .collect();
            let answers = match self.solver.solve_batch(&player_id, code, requests).await {
                Ok(answers) => {
                    self.clear_player_failed(&player_id);
                    answers
                }
                Err(e) => {
                    self.mark_player_failed(&player_id);
                    return Err(e);
                }
            };
            for ((kind, challenge), answer) in asked.into_iter().zip(answers) {
                let value = answer.get(&challenge).cloned();
                match kind {
                    ChallengeKind::N => solved_n = value,
                    ChallengeKind::Sig => solved_sig = value,
                }
            }
        }
        let url = link.assemble(solved_sig.as_deref(), solved_n.as_deref())?;
        let mut stream = stream_from_answer(format, url, &answer);
        if stream.meta.title.is_empty() {
            // The TV answer can come back without the song's details (seen live): ask oEmbed.
            // Best effort: a song without a title still plays.
            if let Some((title, artist)) = self.oembed(video_id).await {
                stream.meta.title = title;
                if stream.meta.artist.is_empty() {
                    stream.meta.artist = artist;
                }
            }
        }
        Ok(stream)
    }

    /// The song's title and channel from YouTube's oEmbed answer (public: no session goes
    /// with it), or `None` if that fails.
    async fn oembed(&self, video_id: &str) -> Option<(String, String)> {
        let mut url = self.web_base.clone();
        url.set_path("/oembed");
        url.query_pairs_mut()
            .append_pair(
                "url",
                &format!("https://www.youtube.com/watch?v={video_id}"),
            )
            .append_pair("format", "json");
        let ask = async {
            let resp = self.http.get(url).send().await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            net::read_capped(resp, OEMBED_CAP).await.ok()
        };
        let body = tokio::time::timeout(OEMBED_TIMEOUT, ask).await.ok()??;
        let o: OEmbed = serde_json::from_slice(&body).ok()?;
        Some((
            o.title.unwrap_or_default(),
            o.author_name.unwrap_or_default(),
        ))
    }

    /// The current player id and signature timestamp, and the player script when it had to be
    /// read for them (so a cold solve doesn't read it twice).
    async fn current_player(&self, fresh: bool) -> Result<(String, u32, Option<String>), Error> {
        if !fresh {
            let memo = self
                .player
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if let Some((id, sts, at)) = memo
                && at.elapsed() < PLAYER_ID_TTL
            {
                self.check_player_not_failed(&id)?;
                return Ok((id, sts, None));
            }
        }
        let id = player_js::current_player_id_at(&self.http, &self.web_base).await?;
        // Before reading the 3 MB script: a version the solver failed on needs none of it.
        self.check_player_not_failed(&id)?;
        let code = self.player_code(&id).await?;
        let sts = player_js::sts(&code).ok_or_else(|| {
            Error::StreamFailed("the player script has no signature timestamp".into())
        })?;
        *self.player.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((id.clone(), sts, Instant::now()));
        Ok((id, sts, Some(code)))
    }

    /// `StreamFailed` when the solver failed on player `id` less than `FAILED_PLAYER_RETRY`
    /// ago, in this process or (from the marker file) an earlier one.
    fn check_player_not_failed(&self, id: &str) -> Result<(), Error> {
        let mut failed = self
            .failed_players
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let at = match failed.get(id) {
            Some(at) => Some(*at),
            None => {
                let at = player_js::modified(&self.players_dir, id, FAILED_SUFFIX);
                if let Some(at) = at {
                    failed.insert(id.to_string(), at);
                }
                at
            }
        };
        // A mark from the future (the clock was set back) counts as stale: better one retry
        // than a version skipped for good.
        let recent = at.is_some_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age < FAILED_PLAYER_RETRY)
        });
        if recent {
            return Err(Error::StreamFailed(
                "the challenge solver failed on this player version".into(),
            ));
        }
        Ok(())
    }

    fn mark_player_failed(&self, id: &str) {
        self.failed_players
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), SystemTime::now());
        // A failed write only means a restarted engine tries this version once more.
        if let Err(e) = player_js::store_cached(&self.players_dir, id, FAILED_SUFFIX, "") {
            eprintln!("ytmfast: could not note the failed player version: {e}");
        }
    }

    /// Clears a mark after the solver worked on `id` (a stale mark that was retried).
    fn clear_player_failed(&self, id: &str) {
        let was_marked = self
            .failed_players
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
            .is_some();
        // Only then: an unmarked version (the usual case) costs no file system call.
        if was_marked {
            player_js::remove_cached(&self.players_dir, id, FAILED_SUFFIX);
        }
    }

    /// Player `id`'s script, from the cache folder or downloaded (and then cached).
    async fn player_code(&self, id: &str) -> Result<String, Error> {
        let dir = &self.players_dir;
        if let Some(code) = player_js::load_cached(dir, id, player_js::BASE_SUFFIX) {
            return Ok(code);
        }
        let code = player_js::fetch_player(&self.http, &self.web_base, id).await?;
        // A failed write only means downloading it again next time.
        if let Err(e) = player_js::store_cached(dir, id, player_js::BASE_SUFFIX, &code) {
            eprintln!("ytmfast: could not cache the player script: {e}");
        }
        Ok(code)
    }

    fn cached(&self, video_id: &str) -> Option<Stream> {
        let links = self.links.lock().unwrap_or_else(|e| e.into_inner());
        links
            .get(video_id)
            .filter(|s| s.expires_unix > now_unix())
            .cloned()
    }

    fn remember(&self, stream: &Stream) {
        let now = now_unix();
        let mut links = self.links.lock().unwrap_or_else(|e| e.into_inner());
        links.retain(|_, s| s.expires_unix > now);
        if links.len() >= MAX_CACHED_LINKS {
            // Drop the one closest to expiring.
            if let Some(oldest) = links
                .iter()
                .min_by_key(|(_, s)| s.expires_unix)
                .map(|(id, _)| id.clone())
            {
                links.remove(&oldest);
            }
        }
        if stream.expires_unix > now {
            links.insert(stream.video_id.clone(), stream.clone());
        }
    }
}

#[async_trait]
impl Resolver for Streams {
    async fn resolve(&self, video_id: &str) -> Result<Stream, Error> {
        check_video_id(video_id)?;
        if let Some(stream) = self.cached(video_id) {
            return Ok(stream);
        }
        let stream = self.fetch(video_id, false).await?;
        self.remember(&stream);
        Ok(stream)
    }

    async fn resolve_fresh(&self, video_id: &str) -> Result<Stream, Error> {
        check_video_id(video_id)?;
        let stream = self.fetch(video_id, true).await?;
        self.remember(&stream);
        Ok(stream)
    }
}

/// A video id is 11 characters of `A-Z a-z 0-9 _ -`. Checked first: the id goes into a
/// yt-dlp argument and a URL, where `&` or `/` would change what is asked for.
fn check_video_id(id: &str) -> Result<(), Error> {
    let ok = id.len() == 11
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Unavailable("not a video id".into()))
    }
}

/// A format's link before its challenges are solved.
struct Link {
    /// The URL the challenges go into: the format's `url`, or the cipher's `url`.
    base: String,
    /// The `n` value in `base`, if any.
    n: Option<String>,
    cipher: Option<Cipher>,
}

/// A `signatureCipher`: `s` (the scrambled signature), `sp` (the query name the solved
/// signature goes under) and `url`.
struct Cipher {
    s: String,
    sp: String,
}

impl Link {
    fn of(format: &AudioFormat) -> Result<Link, Error> {
        let (base, cipher) = match (&format.url, &format.signature_cipher) {
            (Some(url), _) => (url.clone(), None),
            (None, Some(c)) => {
                let mut s = None;
                let mut sp = None;
                let mut url = None;
                for (k, v) in url::form_urlencoded::parse(c.as_bytes()) {
                    match &*k {
                        "s" => s = Some(v.into_owned()),
                        "sp" => sp = Some(v.into_owned()),
                        "url" => url = Some(v.into_owned()),
                        _ => {}
                    }
                }
                let (Some(s), Some(url)) = (s, url) else {
                    return Err(Error::StreamFailed(
                        "the signature cipher is incomplete".into(),
                    ));
                };
                // yt-dlp's default when `sp` is missing.
                let sp = sp.unwrap_or_else(|| "signature".into());
                // It becomes a query name as is; anything but a plain name could add
                // parameters of its own.
                if sp.is_empty() || !sp.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                    return Err(Error::StreamFailed(
                        "the signature cipher is malformed".into(),
                    ));
                }
                (url, Some(Cipher { s, sp }))
            }
            (None, None) => return Err(Error::StreamFailed("the format has no link".into())),
        };
        let parsed = Url::parse(&base)
            .map_err(|_| Error::StreamFailed("the stream link is not a URL".into()))?;
        // Checked before anything is sent to the solver (carried from Task 4, ruling R7).
        if !net::allowed_host(&parsed) {
            return Err(Error::StreamFailed(
                "the stream link is not on an allowed host".into(),
            ));
        }
        let n = parsed
            .query_pairs()
            .find(|(k, _)| k == "n")
            .map(|(_, v)| v.into_owned());
        Ok(Link { base, n, cipher })
    }

    /// The final link, as yt-dlp builds it: for a cipher, `url + "&{sp}=" + encoded(sig)`;
    /// then the `n` value swapped for the solved one. The rest of the query is kept byte for
    /// byte (only the `n` pair is rewritten), so nothing the signature covers changes.
    fn assemble(&self, sig: Option<&str>, n: Option<&str>) -> Result<String, Error> {
        let mut url = self.base.clone();
        if let Some(cipher) = &self.cipher {
            let sig =
                sig.ok_or_else(|| Error::StreamFailed("the signature was not solved".into()))?;
            let sig: String = url::form_urlencoded::byte_serialize(sig.as_bytes()).collect();
            url = format!("{url}&{}={sig}", cipher.sp);
        }
        if self.n.is_some() {
            let n =
                n.ok_or_else(|| Error::StreamFailed("the n challenge was not solved".into()))?;
            url = replace_query_value(&url, "n", n);
        }
        let parsed = Url::parse(&url)
            .map_err(|_| Error::StreamFailed("the stream link is not a URL".into()))?;
        if !net::allowed_host(&parsed) {
            return Err(Error::StreamFailed(
                "the stream link is not on an allowed host".into(),
            ));
        }
        Ok(url)
    }
}

/// `url` with every `key=…` pair's value set to `value` (form-encoded), leaving every other
/// byte as it was.
fn replace_query_value(url: &str, key: &str, value: &str) -> String {
    let Some((head, rest)) = url.split_once('?') else {
        return url.to_string();
    };
    let (query, fragment) = match rest.split_once('#') {
        Some((q, f)) => (q, Some(f)),
        None => (rest, None),
    };
    let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
    let query: Vec<String> = query
        .split('&')
        .map(|pair| {
            let name = pair.split_once('=').map_or(pair, |(k, _)| k);
            if name == key {
                format!("{key}={encoded}")
            } else {
                pair.to_string()
            }
        })
        .collect();
    let mut out = format!("{head}?{}", query.join("&"));
    if let Some(f) = fragment {
        out.push('#');
        out.push_str(f);
    }
    out
}

/// When to stop using `url`: its `expire` query value minus the margin, or now (never cached)
/// when it has none.
fn expires_unix(url: &str) -> u64 {
    Url::parse(url)
        .ok()
        .and_then(|u| {
            u.query_pairs()
                .find(|(k, _)| k == "expire")
                .and_then(|(_, v)| v.parse::<u64>().ok())
        })
        .map(|e| e.saturating_sub(EXPIRY_MARGIN_SECS))
        .unwrap_or_else(now_unix)
}

fn stream_from_answer(format: &AudioFormat, url: String, answer: &PlayerResponse) -> Stream {
    Stream {
        video_id: answer.video_id.clone(),
        expires_unix: expires_unix(&url),
        url,
        itag: format.itag,
        mime: format.mime.clone(),
        content_length: format.content_length,
        loudness_db: answer.loudness_db,
        meta: TrackMeta {
            title: answer.title.clone(),
            artist: answer.author.clone(),
            length_seconds: answer.length_seconds,
            thumbnail: answer.thumbnail.clone(),
        },
        tracking: answer.tracking.clone(),
    }
}

/// The parts of yt-dlp's `-j` answer we read.
#[derive(Deserialize)]
struct YtDlpInfo {
    id: Option<String>,
    url: Option<String>,
    format_id: Option<String>,
    filesize: Option<f64>,
    filesize_approx: Option<f64>,
    ext: Option<String>,
    acodec: Option<String>,
    title: Option<String>,
    artist: Option<String>,
    uploader: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
}

/// A `Stream` from yt-dlp's `-j` answer. No loudness or play-history URLs: yt-dlp doesn't
/// give them.
fn from_ytdlp(json: &[u8], video_id: &str) -> Result<Stream, Error> {
    // Fixed text: serde_json's message can quote the answer, which holds the link.
    let info: YtDlpInfo = serde_json::from_slice(json)
        .map_err(|_| Error::StreamFailed("yt-dlp's answer could not be read".into()))?;
    if info.id.as_deref().is_some_and(|id| id != video_id) {
        return Err(Error::StreamFailed(
            "yt-dlp answered for another video".into(),
        ));
    }
    let url = info
        .url
        .ok_or_else(|| Error::StreamFailed("yt-dlp gave no link".into()))?;
    if !Url::parse(&url).is_ok_and(|u| net::allowed_host(&u)) {
        return Err(Error::StreamFailed(
            "yt-dlp's link is not on an allowed host".into(),
        ));
    }
    // `format_id` is the itag, sometimes with a suffix (`251-drc`).
    let itag = info
        .format_id
        .as_deref()
        .map(|f| f.split(|c: char| !c.is_ascii_digit()).next().unwrap_or(""))
        .and_then(|digits| digits.parse().ok())
        .ok_or_else(|| Error::StreamFailed("yt-dlp gave no itag".into()))?;
    let mime = ytdlp_mime(info.ext.as_deref(), info.acodec.as_deref());
    let thumbnail = info
        .thumbnail
        .filter(|t| Url::parse(t).is_ok_and(|u| net::allowed_host(&u)));
    Ok(Stream {
        video_id: video_id.to_string(),
        expires_unix: expires_unix(&url),
        url,
        itag,
        mime,
        content_length: info
            .filesize
            .or(info.filesize_approx)
            .filter(|n| *n > 0.0)
            .map(|n| n as u64),
        loudness_db: None,
        meta: TrackMeta {
            title: info.title.unwrap_or_default(),
            artist: info.artist.or(info.uploader).unwrap_or_default(),
            length_seconds: info.duration.map_or(0, |d| d.round() as u32),
            thumbnail,
        },
        tracking: Tracking::default(),
    })
}

/// A mime type in InnerTube's form (`audio/webm; codecs="opus"`) from yt-dlp's `ext` and
/// `acodec`.
fn ytdlp_mime(ext: Option<&str>, acodec: Option<&str>) -> String {
    let container = match ext {
        Some("webm") => "audio/webm",
        Some("m4a" | "mp4") => "audio/mp4",
        Some(other) => return format!("audio/{other}"),
        None => "audio/unknown",
    };
    match acodec.filter(|c| !c.is_empty() && *c != "none") {
        Some(codec) => format!("{container}; codecs=\"{codec}\""),
        None => container.to_string(),
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_query_value_keeps_the_rest() {
        assert_eq!(
            replace_query_value("https://h/v?a=1&n=abc&b=%2C", "n", "x/y"),
            "https://h/v?a=1&n=x%2Fy&b=%2C"
        );
        assert_eq!(replace_query_value("https://h/v", "n", "x"), "https://h/v");
        assert_eq!(
            replace_query_value("https://h/v?n=1#f", "n", "2"),
            "https://h/v?n=2#f"
        );
        // `nn=` is a different key.
        assert_eq!(
            replace_query_value("https://h/v?nn=1&n=1", "n", "2"),
            "https://h/v?nn=1&n=2"
        );
    }

    #[test]
    fn expiry_without_expire_is_now() {
        let before = now_unix();
        assert!(expires_unix("https://h/v?x=1") >= before);
        assert_eq!(expires_unix("https://h/v?expire=100"), 0);
    }

    #[test]
    fn ytdlp_mimes() {
        assert_eq!(
            ytdlp_mime(Some("webm"), Some("opus")),
            "audio/webm; codecs=\"opus\""
        );
        assert_eq!(
            ytdlp_mime(Some("m4a"), Some("mp4a.40.2")),
            "audio/mp4; codecs=\"mp4a.40.2\""
        );
        assert_eq!(ytdlp_mime(Some("m4a"), Some("none")), "audio/mp4");
        assert_eq!(ytdlp_mime(None, None), "audio/unknown");
    }

    #[test]
    fn stream_debug_hides_the_link() {
        let s = Stream {
            video_id: "x".into(),
            url: "https://rr1---sn-test.googlevideo.com/v?sig=SECRET".into(),
            itag: 774,
            mime: String::new(),
            content_length: None,
            expires_unix: 0,
            loudness_db: None,
            meta: TrackMeta::default(),
            tracking: Tracking::default(),
        };
        assert!(!format!("{s:?}").contains("SECRET"));
    }
}
