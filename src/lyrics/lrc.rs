//! LRCLIB's timed lines (LRC) and plain text, as the widget read them (`parseLrc`,
//! `plainLines`).

use std::sync::LazyLock;

use regex_lite::Regex;

use super::Line;
use super::js::trim;

/// `/\[(\d+):(\d+(?:\.\d+)?)\]/g`: one "[mm:ss.xx]" stamp.
static STAMP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\d+):(\d+(?:\.\d+)?)\]").expect("a valid pattern"));

/// LRC: "[mm:ss.xx] text", sometimes several stamps on one line (a chorus sung twice). Lines
/// with no stamp ("[ar:…]" tags, blank lines) are skipped. Empty timed lines are instrumental
/// breaks (shown as breathing dots), and so is the wait before the first line: a line at 0 s
/// goes first when the first sung line is more than 3 s in.
pub fn parse_lrc(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut stamps = Vec::new();
        let mut last = 0;
        for c in STAMP.captures_iter(line) {
            // Digits only, so both parse; a run too long for f64 reads as JavaScript's does.
            let min: f64 = c[1].parse().unwrap_or(f64::INFINITY);
            let sec: f64 = c[2].parse().unwrap_or(f64::INFINITY);
            stamps.push(min * 60.0 + sec);
            last = c.get(0).map_or(last, |m| m.end());
        }
        if stamps.is_empty() {
            continue;
        }
        let text = trim(&line[last..]);
        out.extend(stamps.into_iter().map(|t| Line::timed(t, text)));
    }
    // Stable, as JavaScript's sort is: two stamps at one time keep their order.
    out.sort_by(|a, b| a.t.unwrap_or(0.0).total_cmp(&b.t.unwrap_or(0.0)));
    if out.first().is_some_and(|l| l.t.unwrap_or(0.0) > 3.0) {
        out.insert(0, Line::timed(0.0, ""));
    }
    out
}

/// Plain text: one untimed line per line, trimmed (blank lines kept, as spacing).
pub fn plain_lines(text: &str) -> Vec<Line> {
    text.split('\n').map(|l| Line::plain(trim(l))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn times(lines: &[Line]) -> Vec<(f64, &str)> {
        lines
            .iter()
            .map(|l| (l.t.unwrap(), l.text.as_str()))
            .collect()
    }

    #[test]
    fn lrc_lines_sort_and_break() {
        let got = parse_lrc(
            "[ar:x]\n[00:05.00][01:05.5] Twice\r\n[00:07.50]  Second \n\n[00:09]\nno stamp",
        );
        assert_eq!(
            times(&got),
            [
                (0.0, ""),
                (5.0, "Twice"),
                (7.5, "Second"),
                (9.0, ""),
                (65.5, "Twice")
            ]
        );
        // The first line within 3 s: no break before it.
        assert_eq!(times(&parse_lrc("[00:02.9]a")), [(2.9, "a")]);
        assert!(parse_lrc("no stamps at all").is_empty());
        // "[1:2.]" and "[a:b]" are not stamps.
        assert!(parse_lrc("[1:2.] x\n[a:b] y").is_empty());
    }

    #[test]
    fn plain_lines_keep_blank_lines() {
        let got = plain_lines("  one \n\ntwo");
        let texts: Vec<_> = got.iter().map(|l| (l.t, l.text.as_str())).collect();
        assert_eq!(texts, [(None, "one"), (None, ""), (None, "two")]);
    }
}
