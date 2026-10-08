//! Song and artist names as the widget compared and cleaned them (`nameKey`, `sameName`, and
//! the title and first-artist clean-up both lookups make).

use std::sync::LazyLock;

use icu_normalizer::DecomposingNormalizerBorrowed;
use regex_lite::Regex;

use super::js::{SPACE_CLASS, is_space, trim};

/// `nameKey`'s punctuation, besides JavaScript's whitespace.
const NAME_PUNCT: &str =
    ".,!?'\"\u{2019}\u{2018}\u{201C}\u{201D}()[]{}-\u{2013}\u{2014}_:;&/\\\u{2026}~*+";

/// Loose name key: lower case, accents off (NFD, then combining marks U+0300 to U+036F
/// dropped), spaces and punctuation off. "Café Del Mar!" and "cafe del mar" share one.
pub fn name_key(s: &str) -> String {
    let lower = s.to_lowercase();
    DecomposingNormalizerBorrowed::new_nfd()
        .normalize(&lower)
        .chars()
        .filter(|&c| {
            !('\u{300}'..='\u{36F}').contains(&c) && !is_space(c) && !NAME_PUNCT.contains(c)
        })
        .collect()
}

/// Loose name match: case, accents, spaces and punctuation ignored, either one inside the
/// other ("Too Sweet" and "Too Sweet (Live)" match). Two empty keys never match.
pub fn same_name(a: &str, b: &str) -> bool {
    let (x, y) = (name_key(a), name_key(b));
    !x.is_empty() && !y.is_empty() && (x.contains(&y) || y.contains(&x))
}

/// `/\s*[\(\[](feat\.?|ft\.?|with)[^\)\]]*[\)\]]/ig`: a "(feat. X)" or "[with X]" part.
static FEATURING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i){SPACE_CLASS}*[(\[](feat\.?|ft\.?|with)[^)\]]*[)\]]"
    ))
    .expect("a valid pattern")
});

/// `/\s*(?:,|&| x | feat\.? | ft\.? )\s*/i`: where a byline's first artist ends.
static ARTIST_JOIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"(?i){SPACE_CLASS}*(?:,|&| x | feat\.? | ft\.? ){SPACE_CLASS}*"
    ))
    .expect("a valid pattern")
});

/// The title without its "(feat. …)" / "(with …)" parts, trimmed: what the lyrics services
/// file the song under.
pub fn clean_title(title: &str) -> String {
    trim(&FEATURING.replace_all(title, "")).to_string()
}

/// The byline's first artist: everything before the first ",", "&", " x ", " feat. " or
/// " ft. " (case ignored), as `split(...)[0]` gives it (not trimmed).
pub fn first_artist(artist: &str) -> &str {
    match ARTIST_JOIN.find(artist) {
        Some(m) => &artist[..m.start()],
        None => artist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_keys_ignore_case_accents_and_punctuation() {
        assert_eq!(name_key("Café Del Mar!"), "cafedelmar");
        assert_eq!(name_key("Beyoncé — “Halo” (Live)…"), "beyoncehalolive");
        assert_eq!(name_key("the made-up band"), "themadeupband");
        assert_eq!(name_key("星の手紙\u{3000}(TV size)"), "星の手紙tvsize");
        // Hangul decomposes under NFD into jamo, which stay (only U+0300..U+036F go).
        assert_eq!(name_key("가"), "\u{1100}\u{1161}");
        assert!(same_name("Too Sweet", "Too Sweet (Live)"));
        assert!(same_name("THE MADE UP BAND", "the made-up band"));
        assert!(!same_name("", ""));
        assert!(!same_name("...", "Song"));
        assert!(!same_name("Other", "Song"));
    }

    #[test]
    fn titles_and_bylines_clean_up_as_the_widget_did() {
        assert_eq!(
            clean_title("Paper Lanterns (feat. Someone Else)"),
            "Paper Lanterns"
        );
        assert_eq!(clean_title("Quiet Harbour [with Guest]"), "Quiet Harbour");
        assert_eq!(clean_title("Song (FT Someone) (Live)"), "Song (Live)");
        // "with" is a prefix match, as in the widget: "(Without Me)" goes too.
        assert_eq!(clean_title("Hold (Without Me)"), "Hold");
        assert_eq!(clean_title("  Plain\u{3000}"), "Plain");
        assert_eq!(
            first_artist("The Made Up Band & Someone Else"),
            "The Made Up Band"
        );
        assert_eq!(first_artist("Imaginary Duo x Guest"), "Imaginary Duo");
        assert_eq!(first_artist("A, B"), "A");
        assert_eq!(first_artist("A FEAT. B"), "A");
        assert_eq!(first_artist("Xavier"), "Xavier");
        assert_eq!(first_artist("A x"), "A x");
        assert_eq!(first_artist(", A"), "");
    }
}
