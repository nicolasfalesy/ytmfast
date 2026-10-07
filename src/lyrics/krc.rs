//! KuGou's word-timed lyrics (KRC): reading the download (`krcText`, `base64Bytes`, `inflate`,
//! `utf8Text`) and its lines (`parseKrc`), as the widget did.
//!
//! The download's `content` is base64 of a 4-byte "krc1" tag and then a zlib stream XORed with
//! a fixed 16-byte key, the format open lyrics tools read (LyricsX, LDDC).

use std::sync::LazyLock;

use regex_lite::Regex;

use super::js::{SPACE_CLASS, collapse_spaces, is_space, trim, utf16_len};
use super::names::same_name;
use super::{Line, Syl, Word};

/// The XOR key over the zlib stream.
const KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];

/// The most text one KRC may inflate to. The answer is capped at 2 MiB, but DEFLATE packs up to
/// about 1000:1, so a 2 KB answer can ask for megabytes and a 2 MiB one for gigabytes (in the
/// widget a 19 KB stream gave 20 MB in 2.2 s and 569 MB of memory, deep review 2026-10-01).
/// Real lyrics inflate to about 20 KB. Past the cap the KRC is unreadable, and the chain falls
/// back to LRCLIB as for any unreadable answer.
pub const INFLATE_MAX: usize = 1024 * 1024;

/// Why a KRC could not be read. Never shown: an unreadable KRC is only "no word timing".
#[derive(Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// The DEFLATE stream is broken or cut short.
    Inflate,
    /// It inflates past `INFLATE_MAX`.
    TooBig,
}

/// The KRC text from the download's `content`. Too short to hold anything is `""` (no lines).
pub fn krc_text(b64: &str) -> Result<String, Unreadable> {
    let raw = base64_bytes(b64);
    if raw.len() < 8 {
        return Ok(String::new());
    }
    let z: Vec<u8> = raw[4..]
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KEY[i % 16])
        .collect();
    // From byte 2: the zlib header is skipped unread, and so is the checksum after the stream,
    // as the widget's inflate did. Raw DEFLATE (no zlib wrapper) for the same reason.
    let out =
        miniz_oxide::inflate::decompress_to_vec_with_limit(&z[2..], INFLATE_MAX).map_err(|e| {
            match e.status {
                miniz_oxide::inflate::TINFLStatus::HasMoreOutput => Unreadable::TooBig,
                _ => Unreadable::Inflate,
            }
        })?;
    // Invalid UTF-8 (never in a real KRC) becomes U+FFFD; the widget's decoder made other
    // garbage of it. A byte-order mark at the start goes, as there.
    let text = String::from_utf8_lossy(&out);
    Ok(text.strip_prefix('\u{FEFF}').unwrap_or(&text).to_string())
}

/// Base64 the forgiving way the widget read it: characters outside the alphabet (padding,
/// line breaks) are skipped, and a trailing partial group gives what bits it has.
pub fn base64_bytes(s: &str) -> Vec<u8> {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut val = [-1i16; 128];
    for (i, c) in ABC.iter().enumerate() {
        val[*c as usize] = i as i16;
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4 + 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.chars() {
        let v = if (c as u32) < 128 {
            val[c as usize]
        } else {
            -1
        };
        if v < 0 {
            continue;
        }
        acc = ((acc << 6) | v as u32) & 0xff_ffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 255) as u8);
        }
    }
    out
}

/// `/[぀-ヿ㐀-䶿一-鿿豈-﫿]/`: kana and CJK ideographs. Each is a word of its own, so lines wrap
/// between them and each fills on its own. Hangul is not in it: Korean groups by spaces.
fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3040}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{F900}'..='\u{FAFF}')
}

fn has_cjk(s: &str) -> bool {
    s.chars().any(is_cjk)
}

/// `^\[(\d+),(\d+)\](.*)$`, where `.` is JavaScript's (no line terminator).
static LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\[(\d+),(\d+)\]([^\n\r\x{2028}\x{2029}]*)$").expect("a valid pattern")
});

/// `<(\d+),(\d+),-?\d+>([^<]*)`: one timed piece, its offset and length in ms.
static PIECE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<(\d+),(\d+),-?\d+>([^<]*)").expect("a valid pattern"));

/// `/(^|\s)(lyrics|…|recorded)\b/i`: a credit's role, at the start or after a space.
static CREDIT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i)(^|{SPACE_CLASS})(lyrics|lyricist|composed|composer|produced|producer|written|writer|arranged|arranger|music|words|mixed|mastered|vocals?|recorded)\b"
    ))
    .expect("a valid pattern")
});

/// The Chinese for lyricist, composer, arranger, producer, mixing, supervisor and recording.
const CREDIT_CHARS: &str = "作词詞曲编編制混监監录錄";

/// KuGou's copyright notices and its own name.
const NOTICES: [&str; 7] = [
    "著作权",
    "著作權",
    "未经",
    "未經",
    "不得翻唱",
    "酷狗",
    "TME",
];

/// A number of the digits a pattern matched.
fn num(s: &str) -> f64 {
    s.parse().unwrap_or(f64::INFINITY)
}

/// KRC lines: "[start,length]<offset,length,0>word<…>word", times in ms, the offsets from
/// the line's start. Gives lines like `parse_lrc`'s, each with its words: a word may be built
/// from several timed syllables, and `gap` means a space follows it. Chinese and Japanese
/// characters are each their own word. `None` when the words are not really timed (every
/// piece as long as the next in most lines: spread evenly, not sung that way).
///
/// `title` and `artist` are the cleaned title and first artist, for the "Artist - Title" line
/// songs often open with.
pub fn parse_krc(text: &str, title: &str, artist: &str) -> Option<Vec<Line>> {
    let mut lines: Vec<Line> = Vec::new();
    let (mut flat, mut many) = (0usize, 0usize);
    for raw in text.split('\n') {
        let Some(m) = LINE.captures(trim(raw)) else {
            continue;
        };
        let start = num(&m[1]);
        let mut words: Vec<Word> = Vec::new();
        let mut lens: Vec<f64> = Vec::new();
        for w in PIECE.captures_iter(&m[3]) {
            let t0 = (start + num(&w[1])) / 1000.0;
            let len = num(&w[2]);
            let t1 = t0 + len / 1000.0;
            lens.push(len);
            let s = &w[3];
            // A space on its own ends the word before it.
            if !s.chars().any(|c| !is_space(c)) {
                if let Some(prev) = words.last_mut().filter(|_| !s.is_empty()) {
                    prev.gap = true;
                }
                continue;
            }
            let lead = s.chars().next().is_some_and(is_space);
            let trail = s.chars().next_back().is_some_and(is_space);
            let s = collapse_spaces(trim(s));
            let syl = Syl {
                t: t0,
                e: t1,
                n: utf16_len(&s),
            };
            match words.last_mut() {
                Some(prev) if !prev.gap && !lead && !has_cjk(&prev.text) && !has_cjk(&s) => {
                    prev.text.push_str(&s);
                    prev.syl.push(syl);
                    prev.e = prev.e.max(t1);
                }
                _ => words.push(Word {
                    t: t0,
                    e: t1,
                    text: s,
                    gap: false,
                    syl: vec![syl],
                }),
            }
            if trail && let Some(last) = words.last_mut() {
                last.gap = true;
            }
        }
        let Some(last) = words.last_mut() else {
            continue;
        };
        last.gap = false;
        let mut line_text = String::new();
        for w in &words {
            line_text.push_str(&w.text);
            if w.gap {
                line_text.push(' ');
            }
        }
        // Credits ("Lyrics by：…", 作词：…), KuGou's notices, and the "Artist - Title" line
        // songs often open with are not sung.
        if (line_text.contains(':') || line_text.contains('：'))
            && (CREDIT.is_match(&line_text) || line_text.chars().any(|c| CREDIT_CHARS.contains(c)))
        {
            continue;
        }
        if NOTICES.iter().any(|n| line_text.contains(n)) {
            continue;
        }
        if lines.is_empty()
            && line_text.contains(" - ")
            && same_name(&line_text, title)
            && same_name(&line_text, artist)
        {
            continue;
        }
        let end = words.iter().fold(0.0f64, |e, w| e.max(w.e));
        // Every piece exactly as long as the next: spread evenly, not timed.
        if lens.len() >= 3 {
            many += 1;
            let max = lens.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let min = lens.iter().copied().fold(f64::INFINITY, f64::min);
            if max - min <= 1.0 {
                flat += 1;
            }
        }
        lines.push(Line {
            t: Some(words[0].t),
            e: Some(end),
            text: line_text,
            words: Some(words),
        });
    }
    if lines.is_empty() || flat * 2 > many {
        return None;
    }
    lines.sort_by(|a, b| a.t.unwrap_or(0.0).total_cmp(&b.t.unwrap_or(0.0)));
    // Breaks, as `parse_lrc` has them: the wait before the first line, and a silence of 3 s or
    // more (counted from 0.6 s after a line's last word, so a held note is not cut off by the
    // dots).
    let mut out = Vec::with_capacity(lines.len() + 8);
    if lines[0].t.unwrap_or(0.0) > 3.0 {
        out.push(Line::timed(0.0, ""));
    }
    let mut it = lines.into_iter().peekable();
    while let Some(line) = it.next() {
        let end = line.e.unwrap_or(0.0) + 0.6;
        let next_t = it.peek().map(|n| n.t.unwrap_or(0.0));
        out.push(line);
        if next_t.is_some_and(|t| t - end >= 3.0) {
            out.push(Line::timed(end, ""));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A KRC answer's `content` for `text`, built as KuGou's are.
    fn content(text: &str) -> String {
        use std::io::Write;
        let mut z = Vec::new();
        {
            let mut enc = miniz_zlib(&mut z);
            enc.write_all(text.as_bytes()).unwrap();
        }
        let mut raw = b"krc1".to_vec();
        raw.extend(z.iter().enumerate().map(|(i, b)| b ^ KEY[i % 16]));
        b64(&raw)
    }

    /// zlib by hand: a stored-block stream (header, blocks, Adler-32), so the tests need no
    /// compressor; miniz reads it like any other.
    fn miniz_zlib(out: &mut Vec<u8>) -> impl std::io::Write + '_ {
        struct Stored<'a>(&'a mut Vec<u8>, Vec<u8>);
        impl std::io::Write for Stored<'_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.1.extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Drop for Stored<'_> {
            fn drop(&mut self) {
                let data = std::mem::take(&mut self.1);
                self.0.extend_from_slice(&[0x78, 0x01]);
                let chunks: Vec<&[u8]> = if data.is_empty() {
                    vec![&[]]
                } else {
                    data.chunks(65535).collect()
                };
                for (i, c) in chunks.iter().enumerate() {
                    self.0.push(u8::from(i + 1 == chunks.len()));
                    let n = c.len() as u16;
                    self.0.extend_from_slice(&n.to_le_bytes());
                    self.0.extend_from_slice(&(!n).to_le_bytes());
                    self.0.extend_from_slice(c);
                }
                let (mut a, mut b) = (1u32, 0u32);
                for x in &data {
                    a = (a + u32::from(*x)) % 65521;
                    b = (b + a) % 65521;
                }
                self.0.extend_from_slice(&((b << 16) | a).to_be_bytes());
            }
        }
        Stored(out, Vec::new())
    }

    fn b64(raw: &[u8]) -> String {
        const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in raw.chunks(3) {
            let n = c.iter().fold(0u32, |a, b| (a << 8) | u32::from(*b)) << (8 * (3 - c.len()));
            for i in 0..=c.len() {
                s.push(ABC[(n >> (18 - 6 * i)) as usize & 63] as char);
            }
        }
        while !s.len().is_multiple_of(4) {
            s.push('=');
        }
        s
    }

    #[test]
    fn base64_skips_what_is_not_base64() {
        assert_eq!(base64_bytes("aGk="), b"hi");
        assert_eq!(base64_bytes("aG\nk=\u{3000}"), b"hi");
        assert_eq!(base64_bytes("aGk"), b"hi");
        assert_eq!(base64_bytes(""), b"");
    }

    #[test]
    fn krc_text_reads_a_built_answer() {
        assert_eq!(
            krc_text(&content("\u{FEFF}[0,10]<0,10,0>hi")),
            Ok("[0,10]<0,10,0>hi".into())
        );
        // Too short to hold a stream: no text.
        assert_eq!(krc_text("a3JjMQ=="), Ok(String::new()));
        // A broken stream.
        assert_eq!(
            krc_text(&b64(&[b'k', b'r', b'c', b'1', 1, 2, 3, 4, 5, 6, 7, 8])),
            Err(Unreadable::Inflate)
        );
    }

    #[test]
    fn inflate_stops_at_one_mib() {
        // Exactly the cap is read; one byte more is refused.
        let at_cap = "x".repeat(INFLATE_MAX);
        assert_eq!(
            krc_text(&content(&at_cap)).map(|t| t.len()),
            Ok(INFLATE_MAX)
        );
        let over = "x".repeat(INFLATE_MAX + 1);
        assert_eq!(krc_text(&content(&over)), Err(Unreadable::TooBig));
    }

    fn texts(lines: &[Line]) -> Vec<(f64, String)> {
        lines
            .iter()
            .map(|l| (l.t.unwrap(), l.text.clone()))
            .collect()
    }

    #[test]
    fn krc_words_syllables_and_gaps() {
        let krc = "[1000,900]<0,300,0>beau<300,200,0>ti<500,200,0>ful <700,100,0> <800,100,0>day";
        let lines = parse_krc(krc, "x", "y").unwrap();
        let line = &lines[0];
        assert_eq!(line.text, "beautiful day");
        let w = line.words.as_ref().unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(
            (w[0].text.as_str(), w[0].gap, w[0].syl.len()),
            ("beautiful", true, 3)
        );
        assert_eq!((w[0].t, w[0].e), (1.0, 1.7));
        assert_eq!(w[0].syl.iter().map(|s| s.n).collect::<Vec<_>>(), [4, 2, 3]);
        assert_eq!((w[1].text.as_str(), w[1].gap), ("day", false));
        // Kana and kanji one by one; Hangul by spaces.
        let lines = parse_krc(
            "[0,900]<0,300,0>夜<300,250,0>空<550,350,0>に\n[1000,900]<0,300,0>푸<300,200,0>른 <500,400,0>비",
            "x",
            "y",
        )
        .unwrap();
        let words: Vec<Vec<&str>> = lines
            .iter()
            .map(|l| {
                l.words
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|w| w.text.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(words, [vec!["夜", "空", "に"], vec!["푸른", "비"]]);
    }

    #[test]
    fn krc_drops_credits_notices_and_the_title_line() {
        let krc = "[0,500]<0,250,0>Band <250,250,0>- <500,250,0>Song\n\
                   [600,500]<0,500,0>Composed by：Someone\n\
                   [1200,500]<0,500,0>作曲：某人\n\
                   [1800,500]<0,500,0>酷狗音乐\n\
                   [2400,500]<0,500,0>Music: is not a credit here\n\
                   [3000,500]<0,250,0>Sung <250,300,0>line";
        let got = parse_krc(krc, "Song", "Band").unwrap();
        // "Music:" with a colon is a credit by the widget's rule, too.
        assert_eq!(texts(&got), [(3.0, "Sung line".to_string())]);
        // Not the first line: an "Artist - Title" line later on is sung.
        let krc = "[0,500]<0,500,0>first\n[600,500]<0,250,0>Band <250,200,0>- <450,300,0>Song";
        assert_eq!(parse_krc(krc, "Song", "Band").unwrap().len(), 2);
    }

    #[test]
    fn evenly_spread_pieces_are_not_timing() {
        let flat = "[0,900]<0,300,0>la <300,300,0>la <600,300,0>la\n[1000,900]<0,300,0>la <300,301,0>la <600,300,0>la";
        assert_eq!(parse_krc(flat, "x", "y"), None);
        // Half flat is still timing ("more than half" is the rule).
        let half = "[0,900]<0,300,0>la <300,300,0>la <600,300,0>la\n[1000,900]<0,100,0>la <100,300,0>la <400,500,0>la";
        assert!(parse_krc(half, "x", "y").is_some());
        assert_eq!(parse_krc("[id:1]\nno lines", "x", "y"), None);
    }

    #[test]
    fn krc_breaks_before_and_between() {
        let krc = "[4000,500]<0,500,0>a\n[9000,500]<0,500,0>b\n[11000,500]<0,500,0>c";
        let got = parse_krc(krc, "x", "y").unwrap();
        // 4.5 s + 0.6 = 5.1; 9 - 5.1 >= 3: dots. 11 - 10.1 < 3: none.
        assert_eq!(
            texts(&got),
            [
                (0.0, String::new()),
                (4.0, "a".into()),
                (5.1, String::new()),
                (9.0, "b".into()),
                (11.0, "c".into())
            ]
        );
    }
}
