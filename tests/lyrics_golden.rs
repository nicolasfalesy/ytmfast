//! The lyrics chain against the bar widget's own JavaScript (the code it replaces): each case in
//! `tests/fixtures/lyrics` is a song, the KuGou / LRCLIB answers for it, and what YouTube Music's
//! plain lyrics step gives; `<case>.expected.json` is what the widget's JavaScript made of the
//! same answers (run by deno, see `tests/fixtures/lyrics/README.md`). The Rust chain must give
//! the same lines, words, syllables and times, field by field, ask for the same links, and keep
//! (or not keep) the answer the same way. The lyrics in the fixtures are made up.
//!
//! `live_answers_match_the_widget` (ignored) runs the same comparison over a folder of real
//! answers recorded locally (`YTMFAST_LYRICS_GOLDEN=<folder>`); real lyrics are never committed.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use ytmfast::browse::Lyrics;
use ytmfast::error::Error;
use ytmfast::lyrics::{self, Fetched, Found, Line, LyricsWeb, SongFacts, Word};

/// Answers from a case's saved answers, and notes every link asked for.
struct Saved {
    answers: HashMap<String, Value>,
    asked: Mutex<Vec<String>>,
}

#[async_trait]
impl LyricsWeb for Saved {
    async fn get_json(&self, url: &str) -> Fetched {
        self.asked.lock().unwrap().push(url.to_string());
        let Some(a) = self.answers.get(url) else {
            return Fetched::Failed;
        };
        if let Some(v) = a.get("json") {
            // A body that parses to nothing (null, false, 0, "") is a failure, as in the widget.
            return Fetched::from_json(v.clone());
        }
        // The engine's own rule for a status, so this fake can't drift from it.
        match a.get("status").and_then(Value::as_u64) {
            Some(s) => Fetched::from_status(u16::try_from(s).unwrap_or(0)),
            None => Fetched::Failed,
        }
    }
}

fn song_of(case: &Value) -> SongFacts {
    let s = &case["song"];
    SongFacts {
        title: s["title"].as_str().unwrap().into(),
        artist: s["artist"].as_str().unwrap().into(),
        album: s["album"]
            .as_str()
            .filter(|a| !a.is_empty())
            .map(String::from),
        length_seconds: s["songDuration"].as_u64().unwrap() as u32,
    }
}

fn youtube_of(case: &Value) -> Result<Option<Lyrics>, Error> {
    match &case["youtube"] {
        Value::String(s) if s == "none" => Ok(None),
        Value::String(s) if s == "fail" => Err(Error::Network("timed out".into())),
        v => Ok(Some(Lyrics {
            text: v["text"].as_str().unwrap().into(),
            source: v["source"].as_str().unwrap().into(),
        })),
    }
}

/// The widget's result object as the chain's types: a plain line's `t: -1` is no time.
fn found_of(res: &Value) -> Option<Found> {
    if res.get("none") == Some(&Value::Bool(true)) {
        return None;
    }
    let f64_of = |v: &Value| v.as_f64().unwrap();
    let lines = res["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| Line {
            t: Some(f64_of(&l["t"])).filter(|t| *t != -1.0),
            e: l.get("e").map(f64_of),
            text: l["text"].as_str().unwrap().into(),
            words: l.get("words").map(|ws| {
                ws.as_array()
                    .unwrap()
                    .iter()
                    .map(|w| Word {
                        t: f64_of(&w["t"]),
                        e: f64_of(&w["e"]),
                        text: w["text"].as_str().unwrap().into(),
                        gap: w["gap"].as_bool().unwrap(),
                        syl: w["syl"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|s| lyrics::Syl {
                                t: f64_of(&s["t"]),
                                e: f64_of(&s["e"]),
                                n: s["n"].as_u64().unwrap() as usize,
                            })
                            .collect(),
                    })
                    .collect()
            }),
        })
        .collect();
    Some(Found {
        source: res["source"].as_str().unwrap().into(),
        synced: res["synced"].as_bool().unwrap(),
        words: res.get("words").and_then(Value::as_bool).unwrap_or(false),
        lines,
    })
}

/// Two times as the same double, give or take serde_json's parse of the expected file (its
/// default float parser can land one unit in the last place off; the arithmetic itself is the
/// same IEEE double arithmetic JavaScript does).
fn same_time(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() <= 2.0 * f64::EPSILON * a.abs().max(b.abs())
}

/// The answers field by field: the first difference, named.
fn same(got: Option<&Found>, want: Option<&Found>) -> Result<(), String> {
    let (got, want) = match (got, want) {
        (None, None) => return Ok(()),
        (Some(g), Some(w)) => (g, w),
        (g, w) => {
            return Err(format!(
                "answer {:?} vs widget {:?}",
                g.map(|f| &f.source),
                w.map(|f| &f.source)
            ));
        }
    };
    let check = |ok: bool, what: String| if ok { Ok(()) } else { Err(what) };
    check(
        got.source == want.source,
        format!("source {:?} vs {:?}", got.source, want.source),
    )?;
    check(got.synced == want.synced, "synced".into())?;
    check(got.words == want.words, "words".into())?;
    check(
        got.lines.len() == want.lines.len(),
        format!("{} lines vs {}", got.lines.len(), want.lines.len()),
    )?;
    let opt_time = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => same_time(a, b),
        _ => false,
    };
    for (i, (g, w)) in got.lines.iter().zip(&want.lines).enumerate() {
        check(
            opt_time(g.t, w.t),
            format!("line {i}: t {:?} vs {:?}", g.t, w.t),
        )?;
        check(
            opt_time(g.e, w.e),
            format!("line {i}: e {:?} vs {:?}", g.e, w.e),
        )?;
        check(
            g.text == w.text,
            format!("line {i}: text {:?} vs {:?}", g.text, w.text),
        )?;
        let (gw, ww) = (
            g.words.as_deref().unwrap_or(&[]),
            w.words.as_deref().unwrap_or(&[]),
        );
        check(
            g.words.is_some() == w.words.is_some() && gw.len() == ww.len(),
            format!("line {i}: words"),
        )?;
        for (k, (a, b)) in gw.iter().zip(ww).enumerate() {
            let at = format!("line {i} word {k}");
            check(
                same_time(a.t, b.t) && same_time(a.e, b.e),
                format!("{at}: time {a:?} vs {b:?}"),
            )?;
            check(
                a.text == b.text && a.gap == b.gap,
                format!("{at}: {a:?} vs {b:?}"),
            )?;
            check(
                a.syl.len() == b.syl.len(),
                format!("{at}: syllables {a:?} vs {b:?}"),
            )?;
            for (x, y) in a.syl.iter().zip(&b.syl) {
                check(
                    same_time(x.t, y.t) && same_time(x.e, y.e) && x.n == y.n,
                    format!("{at}: syllable {x:?} vs {y:?}"),
                )?;
            }
        }
    }
    Ok(())
}

/// Runs every `<name>.case.json` in `dir` and compares; returns how many ran.
async fn compare_folder(dir: &Path) -> usize {
    let mut names: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| {
            let n = e.unwrap().file_name().into_string().unwrap();
            n.strip_suffix(".case.json").map(String::from)
        })
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no cases in {}", dir.display());
    for name in &names {
        let case: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.case.json"))).unwrap(),
        )
        .unwrap();
        let want: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("{name}.expected.json"))).unwrap(),
        )
        .unwrap();
        let answers: HashMap<String, Value> =
            serde_json::from_value(case["answers"].clone()).unwrap();
        let web = Saved {
            answers,
            asked: Mutex::new(Vec::new()),
        };
        let song = song_of(&case);
        let youtube = youtube_of(&case);
        let got = lyrics::find(&web, Some(&song), || async move { youtube }).await;

        if let Err(e) = same(got.answer.as_ref(), found_of(&want["res"]).as_ref()) {
            panic!("{name}: {e}\n  rust: {:?}", got.answer);
        }
        assert_eq!(!got.failed, want["kept"].as_bool().unwrap(), "{name}: kept");
        // The same links (KuGou and LRCLIB go at the same time, so the order may differ).
        let mut asked = web.asked.lock().unwrap().clone();
        let mut want_asked: Vec<String> = serde_json::from_value(want["asked"].clone()).unwrap();
        asked.sort();
        want_asked.sort();
        assert_eq!(asked, want_asked, "{name}: the links asked for");
    }
    names.len()
}

#[tokio::test]
async fn synthetic_answers_match_the_widget() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lyrics");
    let n = compare_folder(&dir).await;
    assert!(n >= 18, "{n} cases");
}

/// Real answers, recorded locally from lrclib.net and KuGou for a handful of songs (never
/// committed: real lyrics are copyrighted). Run with
/// `YTMFAST_LYRICS_GOLDEN=<folder> cargo test --test lyrics_golden -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "needs YTMFAST_LYRICS_GOLDEN: a folder of locally recorded real answers"]
async fn live_answers_match_the_widget() {
    let dir = std::env::var("YTMFAST_LYRICS_GOLDEN").expect("YTMFAST_LYRICS_GOLDEN");
    let n = compare_folder(Path::new(&dir)).await;
    println!("{n} recorded songs: no differences");
    // A count per source, for the report.
    let mut by: BTreeMap<String, usize> = BTreeMap::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let p = e.unwrap().path();
        if p.to_string_lossy().ends_with(".expected.json") {
            let v: Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            let s = v["res"]["source"].as_str().unwrap_or("none").to_string();
            *by.entry(s).or_default() += 1;
        }
    }
    println!("by source: {by:?}");
}
