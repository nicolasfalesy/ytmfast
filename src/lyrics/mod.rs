//! Timed lyrics: the bar widget's whole lyrics chain, moved into the engine (step 4, spec A1).
//!
//! Ported from the widget's JavaScript (`loadLyrics` and the functions it calls), rule for
//! rule, and checked against it by a golden test (`tests/lyrics_golden.rs`, the widget's own
//! code run by deno on the same answers):
//!
//! 1. KuGou's word timing (`kugou`) and LRCLIB's timed lines (`lrclib`) are asked at the same
//!    time (KuGou's two requests take about 1.3 s, LRCLIB's about 0.2 s).
//! 2. KuGou's words win, in LRCLIB's spelling (`polish`); then LRCLIB's timed lines; then
//!    YouTube Music's own plain lyrics (the caller's `youtube` step, asked only now); then
//!    LRCLIB's plain text.
//!
//! Each service receives the song's title, artist and length (LRCLIB the album too), nothing
//! else: no cookie, no account, no id. The three hosts are reached only from here
//! (`web::allowed_host`), never through the YouTube client.

pub mod js;
pub mod krc;
pub mod lookup;
pub mod lrc;
pub mod names;
pub mod polish;
pub mod web;

use std::future::Future;

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

use crate::browse::Lyrics;
use crate::error::Error;

pub use web::{Fetched, HttpWeb, LyricsWeb};

/// What the lookups need to know of a song: what the widget passed them (`shownSong`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SongFacts {
    pub title: String,
    /// Every artist, joined with ", " (the state's `artist`).
    pub artist: String,
    pub album: Option<String>,
    /// 0 when unknown.
    pub length_seconds: u32,
}

/// One syllable of a word: its time span, and its share of the word's letters (UTF-16 units),
/// for filling the word syllable by syllable.
#[derive(Debug, Clone, PartialEq)]
pub struct Syl {
    pub t: f64,
    pub e: f64,
    pub n: usize,
}

/// One word of a line, from one or more timed syllables. `gap`: a space follows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Word {
    pub t: f64,
    pub e: f64,
    pub text: String,
    pub gap: bool,
    pub syl: Vec<Syl>,
}

/// One line. `t` is `None` on a plain (untimed) line; an empty timed line is a break. `e` is
/// when the last word ends (KuGou's lines only, for the breaks after them).
#[derive(Debug, Clone, PartialEq)]
pub struct Line {
    pub t: Option<f64>,
    pub e: Option<f64>,
    pub text: String,
    pub words: Option<Vec<Word>>,
}

impl Line {
    pub fn timed(t: f64, text: &str) -> Line {
        Line {
            t: Some(t),
            e: None,
            text: text.into(),
            words: None,
        }
    }

    pub fn plain(text: &str) -> Line {
        Line {
            t: None,
            e: None,
            text: text.into(),
            words: None,
        }
    }
}

/// Lyrics found: where from (`"KuGou"`, `"LRCLIB"`, or YouTube Music's own "Source: …" line),
/// whether the lines are timed, whether they carry word timing, and the lines.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub source: String,
    pub synced: bool,
    pub words: bool,
    pub lines: Vec<Line>,
}

/// What the chain came to. `failed`: some request on the way failed (no network, a timeout,
/// a 5xx, an answer too big), so the answer may be worse than the song's real one and must not
/// be kept; the next ask tries again. `youtube_error`: YouTube Music's step itself failed, with
/// its error, for a caller that has nothing else to show.
#[derive(Debug)]
pub struct Outcome {
    pub answer: Option<Found>,
    pub failed: bool,
    pub youtube_error: Option<Error>,
}

/// The most bytes one answer may take as JSON. It goes to the widget as one socket line
/// (`control::protocol::MAX_LINE`, 1 MiB, with room for the reply around it), and 20 are kept.
/// Real lyrics come to 5 to 60 KB; only a freak answer comes near, and that source is then
/// passed over as if it had none (as the widget passed over a KRC past its inflate cap).
pub const MAX_ANSWER_BYTES: usize = 1024 * 1024 - 1024;

/// Whether `found` fits `MAX_ANSWER_BYTES` as JSON. Counted, never written out.
pub fn fits(found: &Found) -> bool {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0 += b.len();
            if self.0 > MAX_ANSWER_BYTES {
                // Stop as soon as it is known not to fit.
                return Err(std::io::Error::other("too big"));
            }
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Count(0), found).is_ok()
}

/// Runs the chain for `song` (`None`: the engine knows nothing of the song, so only YouTube
/// Music can be asked). `youtube` is YouTube Music's plain lyrics step, run only when neither
/// KuGou nor LRCLIB has timing for the song.
pub async fn find<F, Fut>(web: &dyn LyricsWeb, song: Option<&SongFacts>, youtube: F) -> Outcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<Option<Lyrics>, Error>>,
{
    let (lr, kg, mut failed) = match song {
        Some(song) => {
            let (lr, kg) = tokio::join!(lookup::lrclib(web, song), lookup::kugou(web, song));
            (lr.value, kg.value, lr.failed || kg.failed)
        }
        None => (None, None, false),
    };
    let lr_lines = lr.as_ref().filter(|l| l.synced).map(|l| l.lines.as_slice());
    // A source whose answer would not fit a socket line counts as having none (LRCLIB's
    // lines still lend KuGou their spelling first, as in the widget).
    let words = kg
        .and_then(|kg| polish::polish_krc(kg, lr_lines))
        .map(|lines| Found {
            source: "KuGou".into(),
            synced: true,
            words: true,
            lines,
        })
        .filter(fits);
    let lr = lr.filter(fits);
    if let Some(words) = words {
        return Outcome {
            answer: Some(words),
            failed,
            youtube_error: None,
        };
    }
    if lr.as_ref().is_some_and(|l| l.synced) {
        return Outcome {
            answer: lr,
            failed,
            youtube_error: None,
        };
    }
    let (youtube, youtube_error) = match youtube().await {
        Ok(Some(y)) if !y.text.is_empty() => {
            let source = if y.source.is_empty() {
                "YouTube Music".to_string()
            } else {
                y.source
            };
            let found = Found {
                source,
                synced: false,
                words: false,
                lines: lrc::plain_lines(&y.text),
            };
            (Some(found).filter(fits), None)
        }
        Ok(_) => (None, None),
        Err(e) => {
            failed = true;
            (None, Some(e))
        }
    };
    Outcome {
        answer: youtube.or(lr),
        failed,
        youtube_error,
    }
}

/// Seconds to the millisecond on the wire: more digits are float noise (12.340000000000002),
/// and every line and word carries one or two times.
fn ms(s: f64) -> f64 {
    (s * 1000.0).round() / 1000.0
}

/// The wire shape (spec A1): `{"source", "synced", "words", "lines": [{"t"?, "text",
/// "words"?: [{"t", "d", "text", "syl"?}]}]}`. A word's `text` ends with a space when one
/// follows it (`gap`), so a line is its words joined as they are. `syl` is there only for a
/// word sung over two or more syllables: `[{"t", "d", "n"}]`, `n` its share of the word's
/// letters, which the widget fills one syllable after another.
impl Serialize for Found {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(4))?;
        m.serialize_entry("source", &self.source)?;
        m.serialize_entry("synced", &self.synced)?;
        m.serialize_entry("words", &self.words)?;
        m.serialize_entry("lines", &self.lines)?;
        m.end()
    }
}

impl Serialize for Line {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        if let Some(t) = self.t {
            m.serialize_entry("t", &ms(t))?;
        }
        m.serialize_entry("text", &self.text)?;
        if let Some(words) = &self.words {
            m.serialize_entry("words", words)?;
        }
        m.end()
    }
}

impl Serialize for Word {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(None)?;
        m.serialize_entry("t", &ms(self.t))?;
        m.serialize_entry("d", &ms(self.e - self.t))?;
        if self.gap {
            m.serialize_entry("text", &format!("{} ", self.text))?;
        } else {
            m.serialize_entry("text", &self.text)?;
        }
        if self.syl.len() > 1 {
            m.serialize_entry("syl", &self.syl)?;
        }
        m.end()
    }
}

impl Serialize for Syl {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(3))?;
        m.serialize_entry("t", &ms(self.t))?;
        m.serialize_entry("d", &ms(self.e - self.t))?;
        m.serialize_entry("n", &self.n)?;
        m.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_shape() {
        let found = Found {
            source: "KuGou".into(),
            synced: true,
            words: true,
            lines: vec![
                Line::timed(0.0, ""),
                Line {
                    t: Some(12.340000000000002),
                    e: Some(13.1),
                    text: "A line".into(),
                    words: Some(vec![
                        Word {
                            t: 12.34,
                            e: 12.75,
                            text: "A".into(),
                            gap: true,
                            syl: vec![Syl {
                                t: 12.34,
                                e: 12.75,
                                n: 1,
                            }],
                        },
                        Word {
                            t: 12.75,
                            e: 13.1,
                            text: "line".into(),
                            gap: false,
                            syl: vec![
                                Syl {
                                    t: 12.75,
                                    e: 12.9,
                                    n: 2,
                                },
                                Syl {
                                    t: 12.9,
                                    e: 13.1,
                                    n: 2,
                                },
                            ],
                        },
                    ]),
                },
            ],
        };
        assert_eq!(
            serde_json::to_value(&found).unwrap(),
            json!({"source": "KuGou", "synced": true, "words": true, "lines": [
                {"t": 0.0, "text": ""},
                {"t": 12.34, "text": "A line", "words": [
                    {"t": 12.34, "d": 0.41, "text": "A "},
                    {"t": 12.75, "d": 0.35, "text": "line", "syl": [
                        {"t": 12.75, "d": 0.15, "n": 2}, {"t": 12.9, "d": 0.2, "n": 2}]}]}]})
        );
        // The key order is the spec's.
        let text = serde_json::to_string(&found).unwrap();
        assert!(
            text.starts_with(
                r#"{"source":"KuGou","synced":true,"words":true,"lines":[{"t":0.0,"text":""}"#
            ),
            "{text}"
        );
        let plain = Found {
            source: "Source: Musixmatch".into(),
            synced: false,
            words: false,
            lines: vec![Line::plain("One"), Line::plain("")],
        };
        assert_eq!(
            serde_json::to_value(&plain).unwrap(),
            json!({"source": "Source: Musixmatch", "synced": false, "words": false,
                   "lines": [{"text": "One"}, {"text": ""}]})
        );
    }

    /// LRCLIB's exact song only; everything else "not found".
    struct OneAnswer(serde_json::Value);

    #[async_trait::async_trait]
    impl LyricsWeb for OneAnswer {
        async fn get_json(&self, url: &str) -> Fetched {
            if url.starts_with("https://lrclib.net/api/get?") {
                Fetched::Json(self.0.clone())
            } else {
                Fetched::NotFound
            }
        }
    }

    /// A source whose answer would not fit one socket line is passed over, as if it had none:
    /// here LRCLIB's timed lines, so YouTube Music's plain lyrics are used.
    #[tokio::test]
    async fn a_source_too_big_for_a_line_is_passed_over() {
        let song = SongFacts {
            title: "Song".into(),
            artist: "Artist".into(),
            album: None,
            length_seconds: 100,
        };
        let youtube = || async {
            Ok(Some(Lyrics {
                text: "plain".into(),
                source: String::new(),
            }))
        };
        let huge = format!("[00:01.00] {}", "x".repeat(MAX_ANSWER_BYTES));
        let web = OneAnswer(json!({ "syncedLyrics": huge }));
        let got = find(&web, Some(&song), youtube).await;
        assert_eq!(got.answer.unwrap().source, "YouTube Music");
        assert!(
            !got.failed,
            "not a failure: asked again, it would be as big"
        );
        // The same lines at a real size are used.
        let web = OneAnswer(json!({ "syncedLyrics": "[00:01.00] fine" }));
        let got = find(&web, Some(&song), youtube).await;
        assert_eq!(got.answer.unwrap().source, "LRCLIB");
    }

    #[test]
    fn answers_past_a_line_do_not_fit() {
        let big = |n: usize| Found {
            source: "LRCLIB".into(),
            synced: false,
            words: false,
            lines: vec![Line::plain(&"x".repeat(n))],
        };
        let around = serde_json::to_string(&big(0)).unwrap().len();
        assert!(fits(&big(MAX_ANSWER_BYTES - around)));
        assert!(!fits(&big(MAX_ANSWER_BYTES - around + 1)));
    }
}
