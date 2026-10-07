//! KuGou's words in LRCLIB's spelling (`polishKrc`), as the widget did it.
//!
//! KuGou drops most punctuation and some capitals ("Oh I remember how you were you were…").
//! Where a line has the same words as LRCLIB's version, LRCLIB's spelling is shown with KuGou's
//! timing ("Oh, I remember how you were, you were…"): the whole line, or a run inside a longer
//! LRCLIB line when the two split lines differently. Whole-line matches also check the timing:
//! when the two disagree by more than 1.5 s, KuGou timed another version of the song, and its
//! words are not used (`None`).

use super::Line;
use super::js::{split_spaces, trim};

/// A word to compare: lower case, curly quotes and backticks as `'`, and only `a-z`, `0-9`,
/// `'`, Latin letters with accents (U+00C0 to U+024F) and Cyrillic (U+0400 to U+04FF) kept.
/// Kana, Hangul and CJK have no key, so those lines are never respelled.
fn key(w: &str) -> String {
    w.to_lowercase()
        .chars()
        .map(|c| {
            if matches!(c, '\u{2019}' | '\u{2018}' | '`') {
                '\''
            } else {
                c
            }
        })
        .filter(|&c| {
            c.is_ascii_lowercase()
                || c.is_ascii_digit()
                || c == '\''
                || ('\u{C0}'..='\u{24F}').contains(&c)
                || ('\u{400}'..='\u{4FF}').contains(&c)
        })
        .collect()
}

/// One word of a line: `k` to compare, `w` as shown.
struct Spelled {
    k: String,
    w: String,
}

/// A line's words. Punctuation standing on its own ("-") rides along with the word before it.
fn words(s: &str) -> Vec<Spelled> {
    let mut out: Vec<Spelled> = Vec::new();
    for w in split_spaces(trim(s)) {
        let k = key(w);
        if !k.is_empty() {
            out.push(Spelled { k, w: w.into() });
        } else if let Some(last) = out.last_mut().filter(|_| !w.is_empty()) {
            last.w.push(' ');
            last.w.push_str(w);
        }
    }
    out
}

/// The best LRCLIB line for a KuGou line: its index, score and where the run starts.
struct Best {
    j: usize,
    score: f64,
    at: usize,
}

/// KuGou's `lines` respelled from LRCLIB's timed `lrc` lines; `None` when KuGou timed another
/// version of the song. Without LRCLIB lines, KuGou's are kept as they are.
pub fn polish_krc(mut lines: Vec<Line>, lrc: Option<&[Line]>) -> Option<Vec<Line>> {
    let Some(lrc) = lrc.filter(|l| !l.is_empty()) else {
        return Some(lines);
    };
    let reference: Vec<Vec<Spelled>> = lrc.iter().map(|l| words(&l.text)).collect();
    let time = |l: &Line| l.t.unwrap_or(0.0);
    let mut diffs: Vec<f64> = Vec::new();
    for line in &mut lines {
        let lt = time(line);
        let Some(line_words) = line.words.as_mut() else {
            continue;
        };
        let mut counts = Vec::with_capacity(line_words.len());
        let mut mine: Vec<String> = Vec::new();
        for w in line_words.iter() {
            let ws = words(&w.text);
            counts.push(ws.len());
            mine.extend(ws.into_iter().map(|s| s.k));
        }
        if mine.is_empty() {
            continue;
        }
        // Best candidate: every word the same (1), a run inside a longer line (0.9, three
        // words or more), or for a line of four or more at least three words in four the
        // same ("drivin'" and "driving" still pair).
        let mut best: Option<Best> = None;
        for (j, r) in reference.iter().enumerate() {
            if r.is_empty() || (time(&lrc[j]) - lt).abs() > 8.0 {
                continue;
            }
            let (mut score, mut at) = (0.0, 0);
            if r.len() == mine.len() {
                let same = r.iter().zip(&mine).filter(|(a, b)| a.k == **b).count();
                score = if same == r.len() {
                    1.0
                } else if r.len() >= 4 && same * 4 >= r.len() * 3 {
                    same as f64 / r.len() as f64 * 0.85
                } else {
                    0.0
                };
            } else if mine.len() >= 3
                && r.len() > mine.len()
                && let Some(p) = (0..=r.len() - mine.len())
                    .find(|&p| mine.iter().enumerate().all(|(q, k)| r[p + q].k == *k))
            {
                score = 0.9;
                at = p;
            }
            let better = |b: &Best| {
                score > b.score
                    || (score == b.score
                        && (time(&lrc[j]) - lt).abs() < (time(&lrc[b.j]) - lt).abs())
            };
            if score != 0.0 && best.as_ref().is_none_or(better) {
                best = Some(Best { j, score, at });
            }
        }
        let Some(best) = best else {
            continue;
        };
        if reference[best.j].len() == mine.len() {
            diffs.push(lt - time(&lrc[best.j]));
        }
        let shown = &reference[best.j][best.at..best.at + mine.len()];
        // A run taken from mid-line keeps KuGou's capital, and a line ends without a comma,
        // as the Music app shows it.
        let capital = line_words[0]
            .text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase());
        let mut n = 0;
        let mut text = String::new();
        for (w, &count) in line_words.iter_mut().zip(&counts) {
            if count > 0 {
                let mut spelled = shown[n..n + count]
                    .iter()
                    .map(|s| s.w.as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                if n == 0 && capital {
                    spelled = capitalized(&spelled);
                }
                n += count;
                if n == mine.len() {
                    spelled.truncate(spelled.trim_end_matches(',').len());
                }
                w.text = spelled;
            }
            text.push_str(&w.text);
            if w.gap {
                text.push(' ');
            }
        }
        line.text = text;
    }
    if diffs.len() >= 3 {
        diffs.sort_by(f64::total_cmp);
        if diffs[diffs.len() >> 1].abs() > 1.5 {
            return None;
        }
    }
    Some(lines)
}

/// `w.charAt(0).toUpperCase() + w.slice(1)`: the first UTF-16 unit upper-cased. A character
/// outside the Basic Multilingual Plane is two units, the first a lone surrogate that
/// JavaScript leaves alone, so it stays as it is here too.
fn capitalized(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        Some(c) if c.len_utf16() == 1 => c.to_uppercase().chain(chars).collect(),
        _ => w.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lyrics::{Syl, Word};

    fn word(t: f64, text: &str, gap: bool) -> Word {
        Word {
            t,
            e: t + 0.3,
            text: text.into(),
            gap,
            syl: vec![Syl {
                t,
                e: t + 0.3,
                n: text.len(),
            }],
        }
    }

    fn kugou(t: f64, text: &str) -> Line {
        let ws: Vec<&str> = text.split(' ').collect();
        let words: Vec<Word> = ws
            .iter()
            .enumerate()
            .map(|(i, w)| word(t + i as f64 * 0.3, w, i + 1 < ws.len()))
            .collect();
        Line {
            t: Some(t),
            e: Some(t + ws.len() as f64 * 0.3),
            text: text.into(),
            words: Some(words),
        }
    }

    fn shown(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn keys_compare_loosely() {
        assert_eq!(key("Don’t,"), "don't");
        assert_eq!(key("Café!"), "café");
        assert_eq!(key("Привет"), "привет");
        assert_eq!(key("星"), "");
        assert_eq!(key("—"), "");
    }

    #[test]
    fn whole_lines_runs_and_near_matches() {
        let lrc = vec![
            Line::timed(10.2, "Oh, I remember how you were, you were"),
            Line::timed(20.0, "Under the lights, tonight my love,"),
            Line::timed(30.1, "Driving down the empty road"),
        ];
        let lines = vec![
            kugou(10.0, "oh i remember how you were you were"),
            kugou(22.0, "Tonight my love"),
            kugou(30.0, "drivin down the empty road"),
            kugou(40.0, "nothing like it"),
        ];
        let got = polish_krc(lines, Some(&lrc)).unwrap();
        assert_eq!(
            shown(&got),
            [
                "Oh, I remember how you were, you were",
                "Tonight my love",
                "Driving down the empty road",
                "nothing like it"
            ]
        );
    }

    #[test]
    fn another_version_is_refused() {
        // Three whole-line matches, all 5 s off: the median is past 1.5 s.
        let lrc: Vec<Line> = (0..3)
            .map(|i| Line::timed(f64::from(i) * 10.0, &format!("line number {i} here")))
            .collect();
        let lines: Vec<Line> = (0..3)
            .map(|i| kugou(f64::from(i) * 10.0 + 5.0, &format!("line number {i} here")))
            .collect();
        assert!(polish_krc(lines.clone(), Some(&lrc)).is_none());
        // Without LRCLIB lines, KuGou's stand as they are.
        assert_eq!(polish_krc(lines.clone(), None), Some(lines.clone()));
        assert_eq!(polish_krc(lines.clone(), Some(&[])), Some(lines));
    }

    #[test]
    fn capitals_as_javascript_makes_them() {
        assert_eq!(capitalized("ßa"), "SSa");
        assert_eq!(capitalized("éa"), "Éa");
        assert_eq!(capitalized("𐐨x"), "𐐨x");
        assert_eq!(capitalized(""), "");
    }
}
