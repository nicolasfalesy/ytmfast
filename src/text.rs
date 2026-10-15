//! Text that comes from outside (YouTube's answers, a socket client, a state file) and ends up
//! somewhere that reads it as more than text: a D-Bus string, a terminal, a bar label.
//!
//! Control characters are the problem. A NUL is not allowed in a D-Bus string, and the bus
//! drops a connection that sends one (the MPRIS name is then gone). An escape sequence in a
//! title does things in a terminal that shows `playerctl metadata` (set the clipboard, rewrite
//! a link). A bidi override reverses the text around it, and a newline spills a one-line label
//! onto a second line.

/// The line and paragraph separators (Unicode Zl, Zp) and the format characters (Cf) that
/// are invisible: bidi controls, zero-width spaces, invisible operators and the script format
/// controls nobody types into a search box. Pasted in, they make a query look like something
/// else (a bidi override reverses the text) or miss what it looks like.
///
/// That is every Cf but these, which are kept (rulings P12, P12a):
/// - ZWNJ and ZWJ (U+200C, U+200D): part of how Persian, Indic scripts and emoji sequences
///   are written;
/// - the tag characters (U+E0020 to U+E007F): they spell subdivision flags;
/// - the prepended number marks (U+0600 to U+0605, U+06DD, U+070F, U+0890, U+0891, U+08E2,
///   U+110BD, U+110CD): they show as signs in Arabic, Syriac and Kaithi text.
pub fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{2028}' | '\u{2029}' // Zl, Zp
            | '\u{061C}' | '\u{200E}' | '\u{200F}' // bidi marks
            | '\u{202A}'..='\u{202E}' // bidi embeddings and overrides
            | '\u{2066}'..='\u{2069}' // bidi isolates
            | '\u{00AD}' // soft hyphen
            | '\u{180E}' // Mongolian vowel separator
            | '\u{200B}' // zero-width space
            | '\u{2060}'..='\u{2064}' // word joiner, invisible operators
            | '\u{206A}'..='\u{206F}' // deprecated shaping controls
            | '\u{FEFF}' // zero-width no-break space (byte order mark)
            | '\u{FFF9}'..='\u{FFFB}' // interlinear annotation
            | '\u{13430}'..='\u{1343F}' // Egyptian hieroglyph format controls
            | '\u{1BCA0}'..='\u{1BCA3}' // shorthand format controls
            | '\u{1D173}'..='\u{1D17A}' // musical symbol format controls
            | '\u{E0001}' // language tag
    )
}

/// True for the characters `clean` turns into a space: the ones that break a line or a column.
fn is_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\t' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `s` as one line of plain, visible text: a line break (or a run of them) becomes one space,
/// and every other control character (C0, DEL, C1) and `invisible` character is dropped. Text
/// with none of these comes back unchanged, joiners and tag characters included.
pub fn clean(s: &str) -> String {
    if !s
        .chars()
        .any(|c| is_break(c) || c.is_control() || invisible(c))
    {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut after_break = false;
    for c in s.chars() {
        if is_break(c) {
            if !after_break {
                out.push(' ');
            }
            after_break = true;
        } else if !(c.is_control() || invisible(c)) {
            out.push(c);
            after_break = false;
        }
    }
    out
}

/// [`clean`] for text whose lines matter (plain lyrics): every line break (`\r\n`, `\r`, NEL,
/// U+2028, U+2029) becomes `\n` and is kept; every other control and invisible character is
/// dropped, as in [`clean`].
pub fn clean_lines(s: &str) -> String {
    let s = s.replace("\r\n", "\n");
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_break(c) && c != '\t' {
            out.push('\n');
        } else if c == '\t' {
            out.push(' ');
        } else if !(c.is_control() || invisible(c)) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        for s in [
            "",
            "Never Gonna Give You Up",
            "Beyoncé – 夜に駆ける",
            // Joiners and tag characters belong to emoji sequences and scripts.
            "👩\u{200D}🎤 \u{200C} 🏴\u{E0067}\u{E007F}",
            "two  spaces",
        ] {
            assert_eq!(clean(s), s);
        }
    }

    #[test]
    fn control_and_invisible_characters_are_dropped() {
        assert_eq!(clean("x\0y"), "xy");
        assert_eq!(clean("a\u{1b}[31mb"), "a[31mb");
        assert_eq!(clean("a\u{1b}]52;c;AAAA\u{7}b"), "a]52;c;AAAAb");
        assert_eq!(clean("a\u{7f}b\u{9b}c"), "abc");
        assert_eq!(clean("a\u{202E}b\u{2066}c\u{2069}d"), "abcd");
        assert_eq!(clean("a\u{200B}b\u{FEFF}c\u{00AD}d"), "abcd");
    }

    #[test]
    fn line_breaks_become_one_space() {
        assert_eq!(clean("a\nb"), "a b");
        assert_eq!(clean("a\r\n\r\nb"), "a b");
        assert_eq!(clean("a\tb\u{85}c\u{2028}d\u{2029}e"), "a b c d e");
        // A break beside a dropped character is still one break.
        assert_eq!(clean("a\n\0\nb"), "a b");
    }
}
