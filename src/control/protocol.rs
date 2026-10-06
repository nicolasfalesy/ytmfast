//! The control socket's wire format: newline-delimited JSON, one message per line.
//!
//! A request is `{"id": u64, "cmd": str, "args": {...}?}`. A reply carries the request's id
//! and either `"ok": true, "data": {...}` or `"ok": false, "error": {"code", "message"}`. An
//! event has no id: `{"event": "state" | "position" | "queue" | "error", ...}`. Field names
//! are camelCase. `docs/protocol.md` is the reader's version of this file.
//!
//! Parsing goes through `serde_json::Value` by hand rather than a derive, so each bad field
//! gets its own plain message in the `bad_request` reply.

use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::browse::{Endpoint, LikeStatus, id_ok, token_ok};
use crate::engine::{EngineEvent, PlayState, QueueView, Status};
use crate::innertube::{MoreKind, SongItem, check_query};
use crate::queue::{AddAt, QueueItem, Repeat};
use crate::state::{MAX_ARTISTS, MAX_TEXT, is_playlist_id};
use crate::streams::is_video_id;

/// The longest line the socket takes, newline not counted. A longer one closes the
/// connection: nothing legitimate comes close, and it bounds what one client can make the
/// engine hold.
pub const MAX_LINE: usize = 1024 * 1024;

/// The most songs one `queue.add` takes: the size of the queue the engine saves, so one
/// add can't push a whole saved queue out.
pub const MAX_ADD: usize = 500;

/// The error code for a request the engine can't take as written. The socket's own checks
/// answer with it directly; since step 3 a request the browsing calls refuse before sending
/// (`Error::BadRequest`, a bad id, token or search) carries the same code.
pub const BAD_REQUEST: &str = "bad_request";

#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Status,
    /// See `EngineCmd::Play`: no ids resumes, or plays the last song again. `index` only
    /// comes with `playlist_id`.
    Play {
        video_id: Option<String>,
        playlist_id: Option<String>,
        index: Option<usize>,
        start_seconds: f64,
    },
    /// `play {endpoint}`: a row's (or a page header's) play endpoint, sent back as the row
    /// gave it. Cleaned on the way in (see `parse_endpoint`); the engine turns it into a play
    /// (`EngineCmd::play_endpoint`).
    PlayEndpoint(Endpoint),
    /// The browsing commands. Answered from a task of their own, to the asking client only
    /// (`control::answer`); nothing is broadcast and the engine is not involved, except for
    /// `PlayPage`'s play.
    Browse {
        browse_id: String,
        params: Option<String>,
    },
    /// `query` is trimmed and checked (`innertube::check_query`).
    Search {
        query: String,
        params: Option<String>,
    },
    More {
        kind: MoreKind,
        token: String,
    },
    /// Browses the page, then plays its header's button or else its first playable row.
    PlayPage {
        browse_id: String,
        params: Option<String>,
    },
    Pause,
    Toggle,
    Seek {
        seconds: f64,
    },
    /// 0 to 100.
    Volume {
        percent: f64,
    },
    Next,
    Previous,
    QueueGet,
    /// 1 to `MAX_ADD` songs, each checked (see `parse_song`).
    QueueAdd {
        songs: Vec<SongItem>,
        at: AddAt,
    },
    QueueRemove {
        id: u64,
    },
    QueueJump {
        id: u64,
    },
    QueueMove {
        id: u64,
        index: usize,
    },
    Shuffle {
        on: bool,
    },
    Repeat {
        mode: Repeat,
    },
    /// Sets a song's like status: `video_id`'s, else the song playing (`EngineCmd::Like`).
    /// Answered once YouTube took it, from a task of its own like the browsing commands, to
    /// the asking client only.
    Like {
        status: LikeStatus,
        video_id: Option<String>,
    },
    /// Silences the output, keeping the volume for unmuting (`EngineCmd::Mute`).
    Mute {
        on: bool,
    },
    /// A song's plain lyrics (`control::lyrics`): answered like a browsing command, to the
    /// asking client only.
    Lyrics {
        video_id: String,
    },
    Quit,
}

/// A request that can't be taken. `id` is the request's, when it got that far.
#[derive(Debug, Clone, PartialEq)]
pub struct BadRequest {
    pub id: Option<u64>,
    pub message: String,
}

impl Request {
    /// The request as a line (with its newline): what a client sends.
    pub fn to_line(&self, id: u64) -> String {
        let (cmd, args) = match self {
            Request::Status => ("status", None),
            Request::Play {
                video_id,
                playlist_id,
                index,
                start_seconds,
            } => {
                let mut args = Map::new();
                if let Some(v) = video_id {
                    args.insert("videoId".into(), json!(v));
                }
                if let Some(p) = playlist_id {
                    args.insert("playlistId".into(), json!(p));
                }
                if let Some(i) = index {
                    args.insert("index".into(), json!(i));
                }
                args.insert("startSeconds".into(), json!(start_seconds));
                ("play", Some(Value::Object(args)))
            }
            Request::PlayEndpoint(endpoint) => ("play", Some(json!({ "endpoint": endpoint }))),
            Request::Browse { browse_id, params } => {
                ("browse", Some(page_args(browse_id, params.as_deref())))
            }
            Request::Search { query, params } => {
                let mut args = json!({ "query": query });
                if let Some(p) = params {
                    args["params"] = json!(p);
                }
                ("search", Some(args))
            }
            Request::More { kind, token } => {
                ("more", Some(json!({ "kind": kind, "token": token })))
            }
            Request::PlayPage { browse_id, params } => {
                ("playPage", Some(page_args(browse_id, params.as_deref())))
            }
            Request::Pause => ("pause", None),
            Request::Toggle => ("toggle", None),
            Request::Seek { seconds } => ("seek", Some(json!({ "seconds": seconds }))),
            Request::Volume { percent } => ("volume", Some(json!({ "percent": percent }))),
            Request::Next => ("next", None),
            Request::Previous => ("previous", None),
            Request::QueueGet => ("queue.get", None),
            Request::QueueAdd { songs, at } => {
                let songs: Vec<Value> = songs
                    .iter()
                    .map(|s| {
                        json!({
                            "videoId": s.video_id,
                            "title": s.title,
                            "artists": s.artists,
                            "album": s.album,
                            "thumbnail": s.thumbnail,
                            "lengthSeconds": s.length_seconds,
                        })
                    })
                    .collect();
                (
                    "queue.add",
                    Some(json!({ "songs": songs, "at": add_at_name(*at) })),
                )
            }
            Request::QueueRemove { id } => ("queue.remove", Some(json!({ "queueId": id }))),
            Request::QueueJump { id } => ("queue.jump", Some(json!({ "queueId": id }))),
            Request::QueueMove { id, index } => {
                ("queue.move", Some(json!({ "queueId": id, "index": index })))
            }
            Request::Shuffle { on } => ("shuffle", Some(json!({ "on": on }))),
            Request::Repeat { mode } => ("repeat", Some(json!({ "mode": repeat_name(*mode) }))),
            Request::Like { status, video_id } => {
                let mut args = json!({ "status": status });
                if let Some(v) = video_id {
                    args["videoId"] = json!(v);
                }
                ("like", Some(args))
            }
            Request::Mute { on } => ("mute", Some(json!({ "on": on }))),
            Request::Lyrics { video_id } => ("lyrics", Some(json!({ "videoId": video_id }))),
            Request::Quit => ("quit", None),
        };
        let mut msg = json!({ "id": id, "cmd": cmd });
        if let Some(args) = args {
            msg["args"] = args;
        }
        line(msg)
    }
}

/// `browse` and `playPage`'s arguments.
fn page_args(browse_id: &str, params: Option<&str>) -> Value {
    let mut args = json!({ "browseId": browse_id });
    if let Some(p) = params {
        args["params"] = json!(p);
    }
    args
}

const VIDEO_ID_RULE: &str = "videoId must be 11 characters of A-Z, a-z, 0-9, _ and -";
const PLAYLIST_ID_RULE: &str = "playlistId must be 1 to 256 characters of A-Z, a-z, 0-9, _ and -";
const BROWSE_ID_RULE: &str = "browseId must be 2 to 128 characters of A-Z, a-z, 0-9, _ and -";
const PARAMS_RULE: &str =
    "params must be up to 4096 characters of A-Z, a-z, 0-9, _, -, +, /, = and %";
const TOKEN_RULE: &str = "token must be 1 to 4096 characters of A-Z, a-z, 0-9, _, -, +, /, = and %";
const LIKE_STATUS_RULE: &str = "status must be \"like\", \"dislike\" or \"none\"";
const QUERY_RULE: &str =
    "query must be text of 1 to 200 characters with no control or invisible characters";
const ENDPOINT_RULE: &str = "endpoint must be a row's play: {\"watchEndpoint\": {videoId, playlistId, index, params}} or {\"watchPlaylistEndpoint\": {playlistId, params}}";
/// An endpoint's ids follow the browse rule (`browse::id_ok`, as rows are cleaned with), not
/// the plain `play`'s 1 to 256: an endpoint only ever comes from a row.
const ENDPOINT_PLAYLIST_ID_RULE: &str =
    "the endpoint's playlistId must be 2 to 128 characters of A-Z, a-z, 0-9, _ and -";
const ENDPOINT_INDEX_RULE: &str =
    "the endpoint's index must be a whole number from 0 to 4294967295";

/// Reads one request line (without its newline).
pub fn parse_request(text: &[u8]) -> Result<(u64, Request), BadRequest> {
    let bad = |id, message: &str| BadRequest {
        id,
        message: message.into(),
    };
    let value: Value = serde_json::from_slice(text).map_err(|_| bad(None, "not valid JSON"))?;
    let Value::Object(msg) = value else {
        return Err(bad(None, "a request must be a JSON object"));
    };
    let id = msg
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| bad(None, "id must be a whole number from 0 up"))?;
    let cmd = msg
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| bad(Some(id), "cmd must be a string"))?;
    let empty = Map::new();
    let args = match msg.get("args") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(a)) => a,
        Some(_) => return Err(bad(Some(id), "args must be an object")),
    };
    let request = parse_command(cmd, args).map_err(|m| bad(Some(id), m))?;
    Ok((id, request))
}

/// A field that may be missing or null.
fn field<'a>(args: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|v| !v.is_null())
}

fn queue_id(args: &Map<String, Value>) -> Result<u64, &'static str> {
    args.get("queueId")
        .and_then(Value::as_u64)
        .ok_or("queueId must be a whole number from 0 up")
}

/// A whole number from 0 up that fits an index.
fn index_of(v: &Value) -> Option<usize> {
    v.as_u64().and_then(|i| usize::try_from(i).ok())
}

fn parse_command(cmd: &str, args: &Map<String, Value>) -> Result<Request, &'static str> {
    Ok(match cmd {
        "status" => Request::Status,
        "play" if field(args, "endpoint").is_some() => {
            // One way to say what to play, not two: an endpoint already names its song and
            // list, and its play starts at the first second.
            if ["videoId", "playlistId", "index", "startSeconds"]
                .iter()
                .any(|k| field(args, k).is_some())
            {
                return Err(
                    "endpoint can't be mixed with videoId, playlistId, index or startSeconds",
                );
            }
            Request::PlayEndpoint(parse_endpoint(&args["endpoint"])?)
        }
        "browse" => Request::Browse {
            browse_id: browse_id(args)?,
            params: params(args)?,
        },
        "playPage" => Request::PlayPage {
            browse_id: browse_id(args)?,
            params: params(args)?,
        },
        // Checked here with the request's own rule, not left to the request: a bad search
        // must not first load the session, which can mean a keyring prompt.
        "search" => Request::Search {
            query: match field(args, "query") {
                Some(Value::String(q)) => check_query(q).map_err(|_| QUERY_RULE)?.to_owned(),
                _ => return Err(QUERY_RULE),
            },
            params: params(args)?,
        },
        "more" => Request::More {
            kind: match field(args, "kind").and_then(Value::as_str) {
                Some("browse") => MoreKind::Browse,
                Some("search") => MoreKind::Search,
                _ => return Err("kind must be \"browse\" or \"search\""),
            },
            token: match field(args, "token") {
                Some(Value::String(t)) if token_ok(t) => t.clone(),
                _ => return Err(TOKEN_RULE),
            },
        },
        "play" => {
            let video_id = match field(args, "videoId") {
                None => None,
                Some(Value::String(s)) if is_video_id(s) => Some(s.clone()),
                Some(_) => return Err(VIDEO_ID_RULE),
            };
            let playlist_id = match field(args, "playlistId") {
                None => None,
                Some(Value::String(s)) if is_playlist_id(s) => Some(s.clone()),
                Some(_) => return Err(PLAYLIST_ID_RULE),
            };
            let index = match field(args, "index") {
                None => None,
                // Without a list, an index would be quietly ignored: say so instead.
                Some(_) if playlist_id.is_none() => return Err("index needs a playlistId"),
                Some(v) => Some(index_of(v).ok_or("index must be a whole number from 0 up")?),
            };
            let start_seconds = match field(args, "startSeconds") {
                None => 0.0,
                Some(v) => v
                    .as_f64()
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .ok_or("startSeconds must be a number from 0 up")?,
            };
            Request::Play {
                video_id,
                playlist_id,
                index,
                start_seconds,
            }
        }
        "pause" => Request::Pause,
        "toggle" => Request::Toggle,
        // A negative seek is allowed: the engine clamps it to the start.
        "seek" => Request::Seek {
            seconds: args
                .get("seconds")
                .and_then(Value::as_f64)
                .filter(|s| s.is_finite())
                .ok_or("seconds must be a number")?,
        },
        "volume" => Request::Volume {
            percent: args
                .get("percent")
                .and_then(Value::as_f64)
                .filter(|p| (0.0..=100.0).contains(p))
                .ok_or("percent must be a number from 0 to 100")?,
        },
        "next" => Request::Next,
        "previous" => Request::Previous,
        "queue.get" => Request::QueueGet,
        "queue.add" => parse_add(args)?,
        "queue.remove" => Request::QueueRemove {
            id: queue_id(args)?,
        },
        "queue.jump" => Request::QueueJump {
            id: queue_id(args)?,
        },
        "queue.move" => Request::QueueMove {
            id: queue_id(args)?,
            index: args
                .get("index")
                .and_then(index_of)
                .ok_or("index must be a whole number from 0 up")?,
        },
        "shuffle" => Request::Shuffle {
            on: args
                .get("on")
                .and_then(Value::as_bool)
                .ok_or("on must be true or false")?,
        },
        "repeat" => Request::Repeat {
            mode: match args.get("mode").and_then(Value::as_str) {
                Some("off") => Repeat::Off,
                Some("all") => Repeat::All,
                Some("one") => Repeat::One,
                _ => return Err("mode must be \"off\", \"all\" or \"one\""),
            },
        },
        "like" => Request::Like {
            status: match field(args, "status").and_then(Value::as_str) {
                Some("like") => LikeStatus::Like,
                Some("dislike") => LikeStatus::Dislike,
                Some("none") => LikeStatus::Indifferent,
                _ => return Err(LIKE_STATUS_RULE),
            },
            video_id: match field(args, "videoId") {
                None => None,
                Some(Value::String(s)) if is_video_id(s) => Some(s.clone()),
                Some(_) => return Err(VIDEO_ID_RULE),
            },
        },
        "mute" => Request::Mute {
            on: args
                .get("on")
                .and_then(Value::as_bool)
                .ok_or("on must be true or false")?,
        },
        "lyrics" => Request::Lyrics {
            video_id: match field(args, "videoId") {
                Some(Value::String(s)) if is_video_id(s) => s.clone(),
                _ => return Err(VIDEO_ID_RULE),
            },
        },
        "quit" => Request::Quit,
        _ => return Err("unknown command"),
    })
}

/// `browse` and `playPage`'s page.
fn browse_id(args: &Map<String, Value>) -> Result<String, &'static str> {
    match field(args, "browseId") {
        Some(Value::String(s)) if id_ok(s) => Ok(s.clone()),
        _ => Err(BROWSE_ID_RULE),
    }
}

/// An optional `params`. `""` is none: rows and "more" links carry `""` for "no params" (the
/// `Page.js` shapes never use null), and that is how a client sends one back.
fn params(args: &Map<String, Value>) -> Result<Option<String>, &'static str> {
    match field(args, "params") {
        None => Ok(None),
        Some(Value::String(s)) if s.is_empty() => Ok(None),
        Some(Value::String(s)) if token_ok(s) => Ok(Some(s.clone())),
        Some(_) => Err(PARAMS_RULE),
    }
}

/// A play endpoint a client sends back (Review Focus 3). Cleaning it (`Endpoint`) keeps only
/// the fields that play something: unknown keys, YouTube's extras (`playerParams`, a start
/// time, logging blocks) are dropped. But a field that cleaning drops for being malformed (a
/// bad id, params of the wrong charset, an index past u32) is refused here instead: right for
/// YouTube's own answers, a silent drop would turn a client's bad `videoId` into "play the
/// whole list from the top". An empty string counts as absent, as in a row.
fn parse_endpoint(v: &Value) -> Result<Endpoint, &'static str> {
    if !v.is_object() {
        return Err(ENDPOINT_RULE);
    }
    // One endpoint, one key. `Endpoint::from_endpoint` would take the `watchEndpoint` and drop
    // the other, so "this song" and "this list from the top" sent together would quietly be
    // read as the one the client may not have meant. A `null` one is absent, as below.
    if !v["watchEndpoint"].is_null() && !v["watchPlaylistEndpoint"].is_null() {
        return Err(ENDPOINT_RULE);
    }
    let endpoint = Endpoint::from_endpoint(v).ok_or(ENDPOINT_RULE)?;
    let given = |raw: &Value, key: &str| {
        raw.get(key)
            .is_some_and(|x| !x.is_null() && x.as_str() != Some(""))
    };
    match &endpoint {
        Endpoint::Watch(w) => {
            let raw = &v["watchEndpoint"];
            if given(raw, "videoId") && w.video_id.is_none() {
                return Err(VIDEO_ID_RULE);
            }
            if given(raw, "playlistId") && w.playlist_id.is_none() {
                return Err(ENDPOINT_PLAYLIST_ID_RULE);
            }
            if given(raw, "index") && w.index.is_none() {
                return Err(ENDPOINT_INDEX_RULE);
            }
            if given(raw, "params") && w.params.is_none() {
                return Err(PARAMS_RULE);
            }
        }
        Endpoint::WatchPlaylist(w) => {
            if given(&v["watchPlaylistEndpoint"], "params") && w.params.is_none() {
                return Err(PARAMS_RULE);
            }
        }
    }
    Ok(endpoint)
}

/// `queue.add`: `songs` (objects with details) or `videoIds` (bare ids, whose details the
/// engine fills in when they play), not both; `at` is `"next"` or `"end"` (the default).
fn parse_add(args: &Map<String, Value>) -> Result<Request, &'static str> {
    const COUNT: &str = "queue.add takes 1 to 500 songs";
    let songs = match (field(args, "songs"), field(args, "videoIds")) {
        (Some(_), Some(_)) => return Err("give songs or videoIds, not both"),
        (None, None) => return Err("queue.add needs songs or videoIds"),
        (Some(Value::Array(list)), None) => {
            if list.is_empty() || list.len() > MAX_ADD {
                return Err(COUNT);
            }
            list.iter().map(parse_song).collect::<Result<_, _>>()?
        }
        (None, Some(Value::Array(list))) => {
            if list.is_empty() || list.len() > MAX_ADD {
                return Err(COUNT);
            }
            list.iter()
                .map(|v| match v.as_str() {
                    Some(id) if is_video_id(id) => Ok(SongItem {
                        video_id: id.into(),
                        ..SongItem::default()
                    }),
                    _ => Err(VIDEO_ID_RULE),
                })
                .collect::<Result<_, _>>()?
        }
        (Some(_), None) => return Err("songs must be a list"),
        (None, Some(_)) => return Err("videoIds must be a list"),
    };
    let at = match field(args, "at") {
        None => AddAt::End,
        Some(v) => match v.as_str() {
            Some("next") => AddAt::Next,
            Some("end") => AddAt::End,
            _ => return Err("at must be \"next\" or \"end\""),
        },
    };
    Ok(Request::QueueAdd { songs, at })
}

/// One `queue.add` song. Only `videoId` is needed. Text over `MAX_TEXT` bytes (or over
/// `MAX_ARTISTS` artists) is refused rather than cut, as the state file does: a song with a
/// trimmed title would be a different song. A thumbnail the engine would not fetch from
/// (not https on an allowed host) is dropped instead: the song itself is still good, and the
/// widgets load art from it.
fn parse_song(v: &Value) -> Result<SongItem, &'static str> {
    const TEXT: &str = "title, album and each artist must be text of at most 4 KiB";
    let Value::Object(song) = v else {
        return Err("each song must be an object");
    };
    let video_id = match song.get("videoId") {
        Some(Value::String(s)) if is_video_id(s) => s.clone(),
        _ => return Err(VIDEO_ID_RULE),
    };
    let text = |name: &str| -> Result<Option<String>, &'static str> {
        match field(song, name) {
            None => Ok(None),
            Some(Value::String(s)) if s.len() <= MAX_TEXT => Ok(Some(s.clone())),
            Some(_) => Err(TEXT),
        }
    };
    let title = text("title")?.unwrap_or_default();
    let album = text("album")?;
    let artists = match field(song, "artists") {
        None => Vec::new(),
        Some(Value::Array(list)) if list.len() <= MAX_ARTISTS => list
            .iter()
            .map(|a| match a {
                // A widget may pass an artist straight from a channel name (a browse row's
                // byline): cleaned like `next`'s, so the queue never holds " - Topic".
                Value::String(s) if s.len() <= MAX_TEXT => Ok(crate::innertube::clean_artist(s)),
                _ => Err(TEXT),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("artists must be a list of at most 20 names"),
    };
    let thumbnail = match field(song, "thumbnail") {
        None => None,
        // Kept in its parsed form, the link that was checked (see `net::allowed_link`); the cap is
        // checked on both, as parsing can lengthen a link (percent-escapes).
        Some(Value::String(s)) => Some(s)
            .filter(|s| s.len() <= MAX_TEXT)
            .and_then(|s| crate::net::allowed_link(s))
            .filter(|s| s.len() <= MAX_TEXT),
        Some(_) => return Err("thumbnail must be a link"),
    };
    let length_seconds = match field(song, "lengthSeconds") {
        None => 0,
        Some(v) => v
            .as_u64()
            .and_then(|s| u32::try_from(s).ok())
            .ok_or("lengthSeconds must be a whole number from 0 up")?,
    };
    Ok(SongItem {
        video_id,
        title,
        artists,
        album,
        album_id: String::new(),
        thumbnail,
        length_seconds,
        playlist_id: None,
    })
}

/// `{"id", "ok": true, "data"}`.
pub fn ok_reply(id: u64, data: Value) -> String {
    line(json!({ "id": id, "ok": true, "data": data }))
}

/// `{"id", "ok": true, "data"}` with `data` serialized straight to text: a browsing answer (a
/// page of up to 1,000 rows) never becomes a `Value` tree first, which would copy every string
/// once more. The same keys, in the same order, as `ok_reply`.
pub fn data_reply<T: Serialize>(id: u64, data: &T) -> String {
    #[derive(Serialize)]
    struct Reply<'a, T> {
        id: u64,
        ok: bool,
        data: &'a T,
    }
    // The browse shapes are plain strings, numbers, lists and options: serializing can't fail.
    let mut s = serde_json::to_string(&Reply { id, ok: true, data }).expect("a reply serializes");
    s.push('\n');
    s
}

/// `{"id", "ok": false, "error": {"code", "message"}}`; the id is null when the request
/// didn't get far enough to have one.
pub fn error_reply(id: Option<u64>, code: &str, message: &str) -> String {
    line(json!({
        "id": id,
        "ok": false,
        "error": { "code": code, "message": message },
    }))
}

/// The engine's volume (0.0 to 1.0) for a percent.
pub fn percent_to_volume(percent: f64) -> f32 {
    (percent / 100.0).clamp(0.0, 1.0) as f32
}

/// A volume as a percent: a whole number when it is one, else two decimals. The f32 round
/// trip turns 55 into 55.000004, which a slider would show.
pub fn volume_to_percent(volume: f32) -> Value {
    let p = (f64::from(volume) * 10_000.0).round() / 100.0;
    if p.fract() == 0.0 {
        json!(p as u64)
    } else {
        json!(p)
    }
}

pub fn state_name(state: PlayState) -> &'static str {
    match state {
        PlayState::Playing => "playing",
        PlayState::Paused => "paused",
        PlayState::Buffering => "buffering",
        PlayState::Stopped => "stopped",
    }
}

pub fn repeat_name(repeat: Repeat) -> &'static str {
    match repeat {
        Repeat::Off => "off",
        Repeat::All => "all",
        Repeat::One => "one",
    }
}

fn add_at_name(at: AddAt) -> &'static str {
    match at {
        AddAt::Next => "next",
        AddAt::End => "end",
    }
}

/// The `status` reply's data: a `state` event's fields without `"event"`. Song details are
/// null until the song's link is resolved (or its queue item has them).
pub fn status_data(status: &Status) -> Map<String, Value> {
    let meta = status.meta.as_ref();
    let mut m = Map::new();
    m.insert("state".into(), json!(state_name(status.state)));
    m.insert("videoId".into(), json!(status.video_id));
    m.insert("title".into(), json!(meta.map(|m| &m.title)));
    m.insert("artist".into(), json!(meta.map(|m| &m.artist)));
    // 0 is what the player answer gives when it has no usable length: unknown, so null.
    m.insert(
        "lengthSeconds".into(),
        json!(meta.map(|m| m.length_seconds).filter(|s| *s > 0)),
    );
    m.insert(
        "thumbnail".into(),
        json!(meta.and_then(|m| m.thumbnail.as_ref())),
    );
    m.insert("position".into(), json!(seconds(status.position)));
    // While muted, the volume unmuting goes back to: a slider keeps its place.
    m.insert("volume".into(), volume_to_percent(status.volume));
    m.insert("muted".into(), json!(status.muted));
    m.insert("album".into(), json!(status.album));
    // A string, "" when there is none (never null), as browse ids are in the browsing shapes.
    m.insert("albumId".into(), json!(status.album_id));
    m.insert("queueId".into(), json!(status.queue_id));
    m.insert("shuffle".into(), json!(status.shuffle));
    m.insert("repeat".into(), json!(repeat_name(status.repeat)));
    // "like", "dislike", "none", or null until known.
    m.insert("liked".into(), json!(status.liked));
    m
}

/// One queue item on the wire. Borrowed and serialized straight to text: a queue of up to 1,000
/// songs (`queue::MAX_ITEMS`) goes out on every queue change, to every client, and a `Value` tree
/// of it first would copy every string once more.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireItem<'a> {
    queue_id: u64,
    video_id: &'a str,
    /// Null for a bare id (its details come when it plays), like a state's title.
    title: Option<&'a str>,
    artists: &'a [String],
    album: Option<&'a str>,
    /// `""` when the song has none, as in a state.
    album_id: &'a str,
    thumbnail: Option<&'a str>,
    /// Null when unknown (0), as in a state.
    length_seconds: Option<u32>,
}

/// The `queue` event's fields; `event` is left out for the `queue.get` reply.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WireQueue<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'static str>,
    items: Vec<WireItem<'a>>,
    current_id: Option<u64>,
    shuffle: bool,
    repeat: &'static str,
}

impl<'a> WireQueue<'a> {
    fn new(
        event: Option<&'static str>,
        items: &'a [QueueItem],
        current_id: Option<u64>,
        shuffle: bool,
        repeat: Repeat,
    ) -> Self {
        WireQueue {
            event,
            items: items
                .iter()
                .map(|i| WireItem {
                    queue_id: i.id,
                    video_id: &i.song.video_id,
                    title: Some(i.song.title.as_str()).filter(|t| !t.is_empty()),
                    artists: &i.song.artists,
                    album: i.song.album.as_deref(),
                    album_id: &i.song.album_id,
                    thumbnail: i.song.thumbnail.as_deref(),
                    length_seconds: Some(i.song.length_seconds).filter(|s| *s > 0),
                })
                .collect(),
            current_id,
            shuffle,
            repeat: repeat_name(repeat),
        }
    }
}

/// The `queue.get` reply's data: a `queue` event's fields without `"event"`.
pub fn queue_data(view: &QueueView) -> Value {
    let wire = WireQueue::new(
        None,
        &view.items,
        view.current_id,
        view.shuffle,
        view.repeat,
    );
    // Plain strings, numbers and bools: serializing them can't fail.
    serde_json::to_value(wire).expect("a queue serializes")
}

/// An engine event as a line.
pub fn event_line(event: &EngineEvent) -> String {
    match event {
        EngineEvent::State(status) => {
            let mut m = Map::new();
            m.insert("event".into(), json!("state"));
            m.extend(status_data(status));
            line(Value::Object(m))
        }
        EngineEvent::Position { seconds: s, seeked } => line(json!({
            "event": "position",
            "seconds": seconds(*s),
            "seeked": seeked,
        })),
        EngineEvent::Queue {
            items,
            current_id,
            shuffle,
            repeat,
        } => {
            let wire = WireQueue::new(Some("queue"), items, *current_id, *shuffle, *repeat);
            let mut s = serde_json::to_string(&wire).expect("a queue serializes");
            s.push('\n');
            s
        }
        EngineEvent::Error { code, message } => line(json!({
            "event": "error",
            "code": code,
            "message": message,
        })),
    }
}

/// Seconds to the millisecond: more digits are noise, and keep every position line short.
fn seconds(s: f64) -> f64 {
    (s * 1000.0).round() / 1000.0
}

fn line(v: Value) -> String {
    let mut s = v.to_string();
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::streams::TrackMeta;
    use std::sync::Arc;

    fn parse(s: &str) -> Result<(u64, Request), BadRequest> {
        parse_request(s.as_bytes())
    }

    fn bad(s: &str) -> BadRequest {
        parse(s).unwrap_err()
    }

    fn song(id: &str) -> SongItem {
        SongItem {
            video_id: id.into(),
            title: "Song".into(),
            artists: vec!["A".into(), "B".into()],
            album: Some("Album".into()),
            album_id: String::new(),
            thumbnail: Some("https://lh3.googleusercontent.com/x=w544-h544".into()),
            length_seconds: 213,
            playlist_id: None,
        }
    }

    fn bare(id: &str) -> SongItem {
        SongItem {
            video_id: id.into(),
            ..SongItem::default()
        }
    }

    #[test]
    fn protocol_roundtrip() {
        let all = [
            Request::Status,
            Request::Play {
                video_id: Some("dQw4w9WgXcQ".into()),
                playlist_id: None,
                index: None,
                start_seconds: 12.5,
            },
            Request::Play {
                video_id: None,
                playlist_id: None,
                index: None,
                start_seconds: 0.0,
            },
            Request::Play {
                video_id: None,
                playlist_id: Some("OLAK5uy_abc-DEF".into()),
                index: Some(3),
                start_seconds: 0.0,
            },
            Request::PlayEndpoint(Endpoint::Watch(crate::browse::WatchEndpoint {
                video_id: Some("dQw4w9WgXcQ".into()),
                playlist_id: Some("PLfake".into()),
                index: Some(3),
                params: Some("wAEB+/=".into()),
            })),
            Request::PlayEndpoint(Endpoint::WatchPlaylist(
                crate::browse::WatchPlaylistEndpoint {
                    playlist_id: "RDAOfake".into(),
                    params: None,
                },
            )),
            Request::Browse {
                browse_id: "FEmusic_home".into(),
                params: None,
            },
            Request::Browse {
                browse_id: "UCfake".into(),
                params: Some("ggMIegYIARoCAQI%3D".into()),
            },
            Request::Search {
                query: "a song".into(),
                params: Some("EgWKAQIIAQ%3D%3D".into()),
            },
            Request::More {
                kind: MoreKind::Browse,
                token: "fake+token/==".into(),
            },
            Request::More {
                kind: MoreKind::Search,
                token: "faketoken".into(),
            },
            Request::PlayPage {
                browse_id: "UCfake".into(),
                params: None,
            },
            Request::Pause,
            Request::Toggle,
            Request::Seek { seconds: 42.5 },
            Request::Volume { percent: 55.0 },
            Request::Next,
            Request::Previous,
            Request::QueueGet,
            Request::QueueAdd {
                songs: vec![song("dQw4w9WgXcQ"), bare("AAAAAAAAAAA")],
                at: AddAt::Next,
            },
            Request::QueueAdd {
                songs: vec![song("BBBBBBBBBBB")],
                at: AddAt::End,
            },
            Request::QueueRemove { id: 4 },
            Request::QueueJump { id: u64::MAX },
            Request::QueueMove { id: 2, index: 0 },
            Request::Shuffle { on: true },
            Request::Shuffle { on: false },
            Request::Repeat { mode: Repeat::Off },
            Request::Repeat { mode: Repeat::All },
            Request::Repeat { mode: Repeat::One },
            Request::Like {
                status: LikeStatus::Like,
                video_id: None,
            },
            Request::Like {
                status: LikeStatus::Dislike,
                video_id: Some("dQw4w9WgXcQ".into()),
            },
            Request::Like {
                status: LikeStatus::Indifferent,
                video_id: None,
            },
            Request::Mute { on: true },
            Request::Mute { on: false },
            Request::Lyrics {
                video_id: "dQw4w9WgXcQ".into(),
            },
            Request::Quit,
        ];
        for (id, req) in all.into_iter().enumerate() {
            let id = id as u64 + 7;
            let line = req.to_line(id);
            assert!(line.ends_with('\n') && !line[..line.len() - 1].contains('\n'));
            assert_eq!(parse(line.trim_end()), Ok((id, req.clone())), "{line}");
        }
    }

    #[test]
    fn parses_the_spec_examples() {
        assert_eq!(
            parse(r#"{"id": 7, "cmd": "seek", "args": {"seconds": 42.5}}"#),
            Ok((7, Request::Seek { seconds: 42.5 }))
        );
        assert_eq!(
            parse(r#"{"id":1,"cmd":"status"}"#),
            Ok((1, Request::Status))
        );
        assert_eq!(
            parse(r#"{"id":2,"cmd":"play","args":{"videoId":"dQw4w9WgXcQ"}}"#),
            Ok((
                2,
                Request::Play {
                    video_id: Some("dQw4w9WgXcQ".into()),
                    playlist_id: None,
                    index: None,
                    start_seconds: 0.0
                }
            ))
        );
        // Integer or number percent; null args is no args; unknown fields are ignored.
        assert_eq!(
            parse(r#"{"id":3,"cmd":"volume","args":{"percent":40}}"#),
            Ok((3, Request::Volume { percent: 40.0 }))
        );
        assert_eq!(
            parse(r#"{"id":4,"cmd":"pause","args":null,"extra":true}"#),
            Ok((4, Request::Pause))
        );
    }

    #[test]
    fn bad_requests_say_what_is_wrong() {
        assert_eq!(bad("{nope").id, None);
        assert_eq!(bad("[1,2]").id, None);
        assert_eq!(bad(r#"{"cmd":"status"}"#).id, None);
        assert_eq!(bad(r#"{"id":-1,"cmd":"status"}"#).id, None);
        assert_eq!(bad(r#"{"id":"1","cmd":"status"}"#).id, None);
        let unknown = bad(r#"{"id":5,"cmd":"frobnicate"}"#);
        assert_eq!(unknown.id, Some(5));
        assert_eq!(unknown.message, "unknown command");
        for line in [
            r#"{"id":5}"#,
            r#"{"id":5,"cmd":"status","args":[1]}"#,
            r#"{"id":5,"cmd":"seek"}"#,
            r#"{"id":5,"cmd":"seek","args":{"seconds":"1"}}"#,
            r#"{"id":5,"cmd":"volume","args":{"percent":101}}"#,
            r#"{"id":5,"cmd":"volume","args":{"percent":-1}}"#,
            r#"{"id":5,"cmd":"play","args":{"videoId":"../etc"}}"#,
            r#"{"id":5,"cmd":"play","args":{"videoId":5}}"#,
            r#"{"id":5,"cmd":"play","args":{"startSeconds":-2}}"#,
        ] {
            assert_eq!(bad(line).id, Some(5), "{line}");
        }
    }

    #[test]
    fn like_and_mute_parse_from_the_wire() {
        assert_eq!(
            parse(r#"{"id":1,"cmd":"like","args":{"status":"like"}}"#),
            Ok((
                1,
                Request::Like {
                    status: LikeStatus::Like,
                    video_id: None
                }
            ))
        );
        assert_eq!(
            parse(r#"{"id":2,"cmd":"like","args":{"status":"none","videoId":"dQw4w9WgXcQ"}}"#),
            Ok((
                2,
                Request::Like {
                    status: LikeStatus::Indifferent,
                    video_id: Some("dQw4w9WgXcQ".into())
                }
            ))
        );
        // A null videoId is none: the song playing.
        assert_eq!(
            parse(r#"{"id":3,"cmd":"like","args":{"status":"dislike","videoId":null}}"#),
            Ok((
                3,
                Request::Like {
                    status: LikeStatus::Dislike,
                    video_id: None
                }
            ))
        );
        assert_eq!(
            parse(r#"{"id":4,"cmd":"mute","args":{"on":true}}"#),
            Ok((4, Request::Mute { on: true }))
        );
        for (line, message) in [
            (r#"{"id":5,"cmd":"like"}"#, LIKE_STATUS_RULE),
            (
                r#"{"id":5,"cmd":"like","args":{"status":"LIKE"}}"#,
                LIKE_STATUS_RULE,
            ),
            (
                r#"{"id":5,"cmd":"like","args":{"status":"indifferent"}}"#,
                LIKE_STATUS_RULE,
            ),
            (
                r#"{"id":5,"cmd":"like","args":{"status":"like","videoId":"../etc"}}"#,
                VIDEO_ID_RULE,
            ),
            (
                r#"{"id":5,"cmd":"like","args":{"status":"like","videoId":5}}"#,
                VIDEO_ID_RULE,
            ),
            (r#"{"id":5,"cmd":"mute"}"#, "on must be true or false"),
            (
                r#"{"id":5,"cmd":"mute","args":{"on":"yes"}}"#,
                "on must be true or false",
            ),
        ] {
            assert_eq!(
                bad(line),
                BadRequest {
                    id: Some(5),
                    message: message.into()
                },
                "{line}"
            );
        }
    }

    #[test]
    fn queue_requests_parse_from_the_wire() {
        assert_eq!(
            parse(
                r#"{"id":1,"cmd":"queue.add","args":{"at":"next","songs":[
                    {"videoId":"dQw4w9WgXcQ","title":"Song","artists":["A","B"],"album":"Album",
                     "thumbnail":"https://lh3.googleusercontent.com/x=w544-h544","lengthSeconds":213}]}}"#
            ),
            Ok((
                1,
                Request::QueueAdd {
                    songs: vec![song("dQw4w9WgXcQ")],
                    at: AddAt::Next
                }
            ))
        );
        // Bare ids: the engine fills their details when they play. `at` defaults to the end.
        assert_eq!(
            parse(
                r#"{"id":2,"cmd":"queue.add","args":{"videoIds":["dQw4w9WgXcQ","AAAAAAAAAAA"]}}"#
            ),
            Ok((
                2,
                Request::QueueAdd {
                    songs: vec![bare("dQw4w9WgXcQ"), bare("AAAAAAAAAAA")],
                    at: AddAt::End
                }
            ))
        );
        // Only the id is needed in a song; nulls are the same as missing.
        assert_eq!(
            parse(
                r#"{"id":3,"cmd":"queue.add","args":{"songs":[{"videoId":"dQw4w9WgXcQ",
                   "title":null,"artists":null,"album":null,"thumbnail":null,"lengthSeconds":null}]}}"#
            ),
            Ok((
                3,
                Request::QueueAdd {
                    songs: vec![bare("dQw4w9WgXcQ")],
                    at: AddAt::End
                }
            ))
        );
        // A thumbnail off the allowlist (or not https) is dropped, not refused: the song is
        // still good, and the widgets never fetch from a host the engine wouldn't.
        for thumb in [
            "http://i.ytimg.com/x.jpg",
            "https://evil.example/x.jpg",
            "not a url",
            &format!("https://i.ytimg.com/{}", "x".repeat(MAX_TEXT)),
        ] {
            let line = json!({"id": 4, "cmd": "queue.add",
                "args": {"songs": [{"videoId": "dQw4w9WgXcQ", "thumbnail": thumb}]}})
            .to_string();
            match parse(&line) {
                Ok((4, Request::QueueAdd { songs, .. })) => {
                    assert_eq!(songs[0].thumbnail, None, "{thumb}")
                }
                other => panic!("{thumb}: {other:?}"),
            }
        }
        // A kept thumbnail is stored in its parsed form, the link that was checked: QUrl would
        // read the backslash one's host as evil.example, and the newline is dropped.
        for (thumb, want) in [
            (
                "https://i.ytimg.com\\@evil.example/x.jpg",
                "https://i.ytimg.com/@evil.example/x.jpg",
            ),
            (
                "https://i.ytimg.com/vi/\nx.jpg",
                "https://i.ytimg.com/vi/x.jpg",
            ),
        ] {
            let line = json!({"id": 4, "cmd": "queue.add",
                "args": {"songs": [{"videoId": "dQw4w9WgXcQ", "thumbnail": thumb}]}})
            .to_string();
            match parse(&line) {
                Ok((4, Request::QueueAdd { songs, .. })) => {
                    assert_eq!(songs[0].thumbnail.as_deref(), Some(want), "{thumb}")
                }
                other => panic!("{thumb}: {other:?}"),
            }
        }
        assert_eq!(
            parse(r#"{"id":5,"cmd":"queue.move","args":{"queueId":7,"index":0}}"#),
            Ok((5, Request::QueueMove { id: 7, index: 0 }))
        );
        assert_eq!(
            parse(r#"{"id":6,"cmd":"repeat","args":{"mode":"one"}}"#),
            Ok((6, Request::Repeat { mode: Repeat::One }))
        );
        assert_eq!(
            parse(r#"{"id":7,"cmd":"play","args":{"playlistId":"LM"}}"#),
            Ok((
                7,
                Request::Play {
                    video_id: None,
                    playlist_id: Some("LM".into()),
                    index: None,
                    start_seconds: 0.0
                }
            ))
        );
    }

    #[test]
    fn bad_queue_requests_say_what_is_wrong() {
        let ok_song = json!({"videoId": "dQw4w9WgXcQ"});
        let many: Vec<Value> = (0..=MAX_ADD).map(|_| ok_song.clone()).collect();
        let many_ids: Vec<Value> = (0..=MAX_ADD).map(|_| json!("dQw4w9WgXcQ")).collect();
        let long = "x".repeat(MAX_TEXT + 1);
        let too_many_artists: Vec<String> = vec!["a".into(); MAX_ARTISTS + 1];
        for (args, cmd) in [
            (json!({}), "queue.add"),
            (json!({"songs": []}), "queue.add"),
            (json!({"videoIds": []}), "queue.add"),
            (json!({"songs": many}), "queue.add"),
            (json!({"videoIds": many_ids}), "queue.add"),
            (
                json!({"songs": [ok_song.clone()], "videoIds": ["dQw4w9WgXcQ"]}),
                "queue.add",
            ),
            (
                json!({"songs": [ok_song.clone()], "at": "first"}),
                "queue.add",
            ),
            (json!({"songs": "dQw4w9WgXcQ"}), "queue.add"),
            (json!({"songs": [{"title": "no id"}]}), "queue.add"),
            (json!({"songs": [{"videoId": "../etc/pass"}]}), "queue.add"),
            (json!({"videoIds": ["short"]}), "queue.add"),
            (json!({"videoIds": [5]}), "queue.add"),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "title": long}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "album": long}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "artists": [long]}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "artists": too_many_artists}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "artists": "A"}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "title": 5}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "lengthSeconds": -1}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "lengthSeconds": 1.5}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "lengthSeconds": 1u64 << 33}]}),
                "queue.add",
            ),
            (
                json!({"songs": [{"videoId": "dQw4w9WgXcQ", "thumbnail": 5}]}),
                "queue.add",
            ),
            (json!({}), "queue.remove"),
            (json!({"queueId": -1}), "queue.remove"),
            (json!({"queueId": "1"}), "queue.jump"),
            (json!({"queueId": 1.5}), "queue.jump"),
            (json!({"queueId": 1}), "queue.move"),
            (json!({"queueId": 1, "index": -1}), "queue.move"),
            (json!({"index": 0}), "queue.move"),
            (json!({}), "shuffle"),
            (json!({"on": "yes"}), "shuffle"),
            (json!({"on": 1}), "shuffle"),
            (json!({}), "repeat"),
            (json!({"mode": "Track"}), "repeat"),
            (json!({"mode": true}), "repeat"),
            (json!({"playlistId": ""}), "play"),
            (json!({"playlistId": "a/b"}), "play"),
            (json!({"playlistId": "x".repeat(257)}), "play"),
            (json!({"playlistId": 5}), "play"),
            (json!({"playlistId": "LM", "index": -1}), "play"),
            (json!({"playlistId": "LM", "index": 1.5}), "play"),
            // An index only means something in a list.
            (json!({"index": 2}), "play"),
        ] {
            let line = json!({"id": 9, "cmd": cmd, "args": args}).to_string();
            let b = bad(&line);
            assert_eq!(b.id, Some(9), "{line}");
            assert!(!b.message.is_empty(), "{line}");
        }
        // The caps themselves are fine.
        let max: Vec<Value> = (0..MAX_ADD).map(|_| ok_song.clone()).collect();
        let line = json!({"id": 1, "cmd": "queue.add", "args": {"songs": max}}).to_string();
        assert!(parse(&line).is_ok());
        let edge = json!({"videoId": "dQw4w9WgXcQ", "title": "t".repeat(MAX_TEXT),
            "album": "a".repeat(MAX_TEXT), "artists": vec!["r".repeat(MAX_TEXT); MAX_ARTISTS],
            "lengthSeconds": u32::MAX});
        let line = json!({"id": 1, "cmd": "queue.add", "args": {"songs": [edge]}}).to_string();
        assert!(parse(&line).is_ok());
        let line = json!({"id": 1, "cmd": "play", "args": {"playlistId": "x".repeat(256)}});
        assert!(parse(&line.to_string()).is_ok());
    }

    #[test]
    fn added_songs_lose_the_topic_suffix() {
        // A widget's song can carry an artist straight from a channel name (a browse row's
        // byline); the queue, its status and state.json keep the artist alone.
        let song = parse_song(&json!({"videoId": "dQw4w9WgXcQ",
            "artists": ["One - Topic", "Topic", "Two"]}))
        .unwrap();
        assert_eq!(song.artists, ["One", "Topic", "Two"]);
    }

    fn item(id: u64, song: SongItem) -> QueueItem {
        QueueItem { id, song }
    }

    #[test]
    fn queue_event_and_reply_shape() {
        let items: Arc<[QueueItem]> = vec![
            item(
                1,
                SongItem {
                    album_id: "MPREb_abc".into(),
                    ..song("dQw4w9WgXcQ")
                },
            ),
            item(2, bare("AAAAAAAAAAA")),
        ]
        .into();
        let event = EngineEvent::Queue {
            items: items.clone(),
            current_id: Some(2),
            shuffle: true,
            repeat: Repeat::All,
        };
        let v: Value = serde_json::from_str(&event_line(&event)).unwrap();
        assert_eq!(
            v,
            json!({"event": "queue", "currentId": 2, "shuffle": true, "repeat": "all",
                   "items": [
                       {"queueId": 1, "videoId": "dQw4w9WgXcQ", "title": "Song",
                        "artists": ["A", "B"], "album": "Album", "albumId": "MPREb_abc",
                        "thumbnail": "https://lh3.googleusercontent.com/x=w544-h544",
                        "lengthSeconds": 213},
                       // A bare id: details are null until the song plays; no album id is "".
                       {"queueId": 2, "videoId": "AAAAAAAAAAA", "title": null, "artists": [],
                        "album": null, "albumId": "", "thumbnail": null, "lengthSeconds": null}]})
        );
        // `queue.get`'s data is the same without "event".
        let mut data = v.as_object().unwrap().clone();
        data.remove("event");
        assert_eq!(
            queue_data(&QueueView {
                items,
                current_id: Some(2),
                shuffle: true,
                repeat: Repeat::All
            }),
            Value::Object(data)
        );
        let empty = EngineEvent::Queue {
            items: Vec::new().into(),
            current_id: None,
            shuffle: false,
            repeat: Repeat::One,
        };
        let v: Value = serde_json::from_str(&event_line(&empty)).unwrap();
        assert_eq!(
            v,
            json!({"event": "queue", "items": [], "currentId": null, "shuffle": false,
                   "repeat": "one"})
        );
    }

    /// The biggest queue the engine holds (`queue::MAX_ITEMS`, 1,000 songs; ruling S15), with
    /// real-sized fields: one event line. Prints its size and how long it takes to build, for
    /// the task report; asserts it stays well under the socket's 1 MiB line cap, which widgets
    /// reading with the same cap rely on.
    #[test]
    fn a_full_queue_event_is_well_under_a_line() {
        let items: Arc<[QueueItem]> = (0..crate::queue::MAX_ITEMS as u64)
            .map(|i| {
                item(
                    i + 1,
                    SongItem {
                        video_id: format!("{:011}", i),
                        title: "A Song Title Of Typical Length (Remastered)".into(),
                        artists: vec!["First Artist".into(), "Second Artist".into()],
                        album: Some("An Album Name Of Typical Length".into()),
                        album_id: "MPREb_abcdefghijk".into(),
                        thumbnail: Some(format!(
                            "https://lh3.googleusercontent.com/{}=w544-h544-l90-rj",
                            "x".repeat(110)
                        )),
                        length_seconds: 245,
                        playlist_id: Some("OLAK5uy_abcdefghijklmnopqrstuvwxyz0123456".into()),
                    },
                )
            })
            .collect();
        let event = EngineEvent::Queue {
            items,
            current_id: Some(250),
            shuffle: false,
            repeat: Repeat::Off,
        };
        let start = std::time::Instant::now();
        let rounds = 20;
        let mut bytes = 0;
        for _ in 0..rounds {
            bytes = event_line(&event).len();
        }
        let each = start.elapsed() / rounds;
        println!("1,000-song queue event: {bytes} bytes, {each:?} to build");
        assert!(bytes < MAX_LINE / 2, "{bytes}");
    }

    #[test]
    fn replies_have_the_spec_shape() {
        let v: Value = serde_json::from_str(&ok_reply(7, json!({}))).unwrap();
        assert_eq!(v, json!({"id": 7, "ok": true, "data": {}}));
        // A browsing answer: the same keys, in the same order, as one line.
        let line = data_reply(8, &crate::browse::MorePage::default());
        assert_eq!(
            line,
            "{\"id\":8,\"ok\":true,\"data\":{\"items\":[],\"sections\":[],\"cont\":\"\"}}\n"
        );
        let line = error_reply(None, BAD_REQUEST, "not valid JSON");
        assert!(line.ends_with('\n'));
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            v,
            json!({"id": null, "ok": false,
                   "error": {"code": "bad_request", "message": "not valid JSON"}})
        );
    }

    #[test]
    fn volume_is_a_percent_on_the_wire() {
        assert_eq!(percent_to_volume(55.0), 0.55);
        assert_eq!(volume_to_percent(percent_to_volume(55.0)), json!(55));
        assert_eq!(volume_to_percent(1.0), json!(100));
        assert_eq!(volume_to_percent(0.0), json!(0));
        assert_eq!(volume_to_percent(percent_to_volume(33.5)), json!(33.5));
    }

    #[test]
    fn events_are_camel_case_lines() {
        let status = Status {
            state: PlayState::Playing,
            video_id: Some("dQw4w9WgXcQ".into()),
            meta: Some(TrackMeta {
                title: "Song".into(),
                artist: "Artist".into(),
                length_seconds: 213,
                thumbnail: Some("https://i.ytimg.com/x.jpg".into()),
            }),
            album: Some("Album".into()),
            album_id: "MPREb_abc".into(),
            queue_id: Some(7),
            position: 1.234_567,
            volume: 0.8,
            muted: true,
            shuffle: true,
            repeat: Repeat::All,
            liked: Some(LikeStatus::Dislike),
        };
        let v: Value =
            serde_json::from_str(&event_line(&EngineEvent::State(status.clone()))).unwrap();
        assert_eq!(
            v,
            json!({"event": "state", "state": "playing", "videoId": "dQw4w9WgXcQ",
                   "title": "Song", "artist": "Artist", "lengthSeconds": 213,
                   "thumbnail": "https://i.ytimg.com/x.jpg", "position": 1.235, "volume": 80,
                   "muted": true, "album": "Album", "albumId": "MPREb_abc", "queueId": 7,
                   "shuffle": true,
                   "repeat": "all", "liked": "dislike"})
        );
        // The status reply is the same without "event".
        let mut data = v.as_object().unwrap().clone();
        data.remove("event");
        assert_eq!(status_data(&status), data);

        let empty = Status {
            state: PlayState::Stopped,
            video_id: None,
            meta: None,
            album: None,
            album_id: String::new(),
            queue_id: None,
            position: 0.0,
            volume: 1.0,
            muted: false,
            shuffle: false,
            repeat: Repeat::Off,
            liked: None,
        };
        assert_eq!(
            Value::Object(status_data(&empty)),
            json!({"state": "stopped", "videoId": null, "title": null, "artist": null,
                   "lengthSeconds": null, "thumbnail": null, "position": 0.0, "volume": 100,
                   "muted": false, "album": null, "albumId": "", "queueId": null,
                   "shuffle": false,
                   "repeat": "off", "liked": null})
        );
        // Each like status by its socket name.
        for (liked, name) in [
            (LikeStatus::Like, "like"),
            (LikeStatus::Dislike, "dislike"),
            (LikeStatus::Indifferent, "none"),
        ] {
            let s = Status {
                liked: Some(liked),
                ..empty.clone()
            };
            assert_eq!(status_data(&s)["liked"], name);
        }

        let v: Value = serde_json::from_str(&event_line(&EngineEvent::Position {
            seconds: 42.5,
            seeked: false,
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"event": "position", "seconds": 42.5, "seeked": false})
        );
        let v: Value = serde_json::from_str(&event_line(&EngineEvent::Position {
            seconds: 3.0,
            seeked: true,
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"event": "position", "seconds": 3.0, "seeked": true})
        );
        let v: Value = serde_json::from_str(&event_line(&EngineEvent::Error {
            code: "network",
            message: "network error: timed out".into(),
        }))
        .unwrap();
        assert_eq!(
            v,
            json!({"event": "error", "code": "network", "message": "network error: timed out"})
        );
    }
}
