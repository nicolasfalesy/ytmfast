//! The control socket's wire format: newline-delimited JSON, one message per line.
//!
//! A request is `{"id": u64, "cmd": str, "args": {...}?}`. A reply carries the request's id
//! and either `"ok": true, "data": {...}` or `"ok": false, "error": {"code", "message"}`. An
//! event has no id: `{"event": "state" | "position" | "error", ...}`. Field names are
//! camelCase. `docs/protocol.md` is the reader's version of this file.
//!
//! Parsing goes through `serde_json::Value` by hand rather than a derive, so each bad field
//! gets its own plain message in the `bad_request` reply.

use serde_json::{Map, Value, json};

use crate::engine::{EngineEvent, PlayState, Status};

/// The longest line the socket takes, newline not counted. A longer one closes the
/// connection: nothing legitimate comes close, and it bounds what one client can make the
/// engine hold.
pub const MAX_LINE: usize = 1024 * 1024;

/// The error code for a request the engine can't take as written. Protocol only: it is
/// never an `Error` (see the ledger's pre-flight scan).
pub const BAD_REQUEST: &str = "bad_request";

/// The step 1 commands.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Status,
    /// No id: resume, or play the last song again.
    Play {
        video_id: Option<String>,
        start_seconds: f64,
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
                start_seconds,
            } => {
                let mut args = Map::new();
                if let Some(v) = video_id {
                    args.insert("videoId".into(), json!(v));
                }
                args.insert("startSeconds".into(), json!(start_seconds));
                ("play", Some(Value::Object(args)))
            }
            Request::Pause => ("pause", None),
            Request::Toggle => ("toggle", None),
            Request::Seek { seconds } => ("seek", Some(json!({ "seconds": seconds }))),
            Request::Volume { percent } => ("volume", Some(json!({ "percent": percent }))),
            Request::Quit => ("quit", None),
        };
        let mut msg = json!({ "id": id, "cmd": cmd });
        if let Some(args) = args {
            msg["args"] = args;
        }
        line(msg)
    }
}

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
    let bad = |message: &str| bad(Some(id), message);
    let cmd = msg
        .get("cmd")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("cmd must be a string"))?;
    let empty = Map::new();
    let args = match msg.get("args") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(a)) => a,
        Some(_) => return Err(bad("args must be an object")),
    };
    let request = match cmd {
        "status" => Request::Status,
        "play" => {
            let video_id = match args.get("videoId") {
                None | Some(Value::Null) => None,
                Some(Value::String(s)) if crate::streams::is_video_id(s) => Some(s.clone()),
                Some(_) => {
                    return Err(bad(
                        "videoId must be 11 characters of A-Z, a-z, 0-9, _ and -",
                    ));
                }
            };
            let start_seconds = match args.get("startSeconds") {
                None | Some(Value::Null) => 0.0,
                Some(v) => v
                    .as_f64()
                    .filter(|s| s.is_finite() && *s >= 0.0)
                    .ok_or_else(|| bad("startSeconds must be a number from 0 up"))?,
            };
            Request::Play {
                video_id,
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
                .ok_or_else(|| bad("seconds must be a number"))?,
        },
        "volume" => Request::Volume {
            percent: args
                .get("percent")
                .and_then(Value::as_f64)
                .filter(|p| (0.0..=100.0).contains(p))
                .ok_or_else(|| bad("percent must be a number from 0 to 100"))?,
        },
        "quit" => Request::Quit,
        _ => return Err(bad("unknown command")),
    };
    Ok((id, request))
}

/// `{"id", "ok": true, "data"}`.
pub fn ok_reply(id: u64, data: Value) -> String {
    line(json!({ "id": id, "ok": true, "data": data }))
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

/// The `status` reply's data: a `state` event's fields without `"event"`. Song details are
/// null until the song's link is resolved.
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
    m.insert("volume".into(), volume_to_percent(status.volume));
    m
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
        EngineEvent::Position { seconds: s } => {
            line(json!({ "event": "position", "seconds": seconds(*s) }))
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

    fn parse(s: &str) -> Result<(u64, Request), BadRequest> {
        parse_request(s.as_bytes())
    }

    fn bad(s: &str) -> BadRequest {
        parse(s).unwrap_err()
    }

    #[test]
    fn protocol_roundtrip() {
        let all = [
            Request::Status,
            Request::Play {
                video_id: Some("dQw4w9WgXcQ".into()),
                start_seconds: 12.5,
            },
            Request::Play {
                video_id: None,
                start_seconds: 0.0,
            },
            Request::Pause,
            Request::Toggle,
            Request::Seek { seconds: 42.5 },
            Request::Volume { percent: 55.0 },
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
    fn replies_have_the_spec_shape() {
        let v: Value = serde_json::from_str(&ok_reply(7, json!({}))).unwrap();
        assert_eq!(v, json!({"id": 7, "ok": true, "data": {}}));
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
            position: 1.234_567,
            volume: 0.8,
        };
        let v: Value =
            serde_json::from_str(&event_line(&EngineEvent::State(status.clone()))).unwrap();
        assert_eq!(
            v,
            json!({"event": "state", "state": "playing", "videoId": "dQw4w9WgXcQ",
                   "title": "Song", "artist": "Artist", "lengthSeconds": 213,
                   "thumbnail": "https://i.ytimg.com/x.jpg", "position": 1.235, "volume": 80})
        );
        // The status reply is the same without "event".
        let mut data = v.as_object().unwrap().clone();
        data.remove("event");
        assert_eq!(status_data(&status), data);

        let empty = Status {
            state: PlayState::Stopped,
            video_id: None,
            meta: None,
            position: 0.0,
            volume: 1.0,
        };
        assert_eq!(
            Value::Object(status_data(&empty)),
            json!({"state": "stopped", "videoId": null, "title": null, "artist": null,
                   "lengthSeconds": null, "thumbnail": null, "position": 0.0, "volume": 100})
        );

        let v: Value =
            serde_json::from_str(&event_line(&EngineEvent::Position { seconds: 42.5 })).unwrap();
        assert_eq!(v, json!({"event": "position", "seconds": 42.5}));
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
