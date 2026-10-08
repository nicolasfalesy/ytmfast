//! The bits of JavaScript the widget's lyrics code leans on, done the way JavaScript does them,
//! so the port gives the same answer on the same input (the golden test checks it).
//!
//! JavaScript's `\s` and `trim()` know more whitespace than Rust's ASCII helpers (and one
//! character more than `char::is_whitespace`: U+FEFF, which Rust leaves out, while Rust has
//! U+0085, which JavaScript leaves out). Japanese lyrics use U+3000 between phrases, so the
//! difference shows.

use serde_json::Value;

/// JavaScript's `\s`: WhiteSpace plus LineTerminator (ECMA-262), for the code's regexes.
pub const SPACE_CLASS: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";

/// One character of JavaScript's `\s`.
pub fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_space)
}

/// `s.replace(/\s+/g, " ")`.
pub fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_space = false;
    for c in s.chars() {
        if is_space(c) {
            if !in_space {
                out.push(' ');
            }
            in_space = true;
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `s.split(/\s+/)` of a trimmed string: `[""]` for an empty one, as JavaScript gives.
pub fn split_spaces(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return vec![""];
    }
    s.split(is_space).filter(|w| !w.is_empty()).collect()
}

/// `s.length`: UTF-16 code units, the unit a syllable's share of its word is counted in.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// `encodeURIComponent`: everything but `A-Z a-z 0-9 - _ . ! ~ * ' ( )` as `%XX` of its UTF-8.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A value's truthiness (`if (x)`): a missing field and `null` are false, as are `false`, `0`,
/// `NaN` and `""`.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(_) | Value::Object(_)) => true,
    }
}

/// `Number(x || 0)`: a falsy value is 0, a number itself, a numeric string its number (blank
/// is 0), anything else `NaN`.
pub fn number(v: Option<&Value>) -> f64 {
    if !truthy(v) {
        return 0.0;
    }
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Bool(true)) => 1.0,
        Some(Value::String(s)) => {
            let t = trim(s);
            if t.is_empty() {
                0.0
            } else {
                // Rust reads "inf" and "nan", which JavaScript's Number does not.
                match t.parse::<f64>() {
                    Ok(f) if f.is_finite() || t.trim_start_matches(['+', '-']) == "Infinity" => f,
                    _ => f64::NAN,
                }
            }
        }
        _ => f64::NAN,
    }
}

/// `String(x || "")`: a falsy value is `""`, a string itself, a number as JavaScript writes a
/// whole number (the KuGou ids are), `true` as "true".
pub fn string(v: Option<&Value>) -> String {
    if !truthy(v) {
        return String::new();
    }
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => match (n.as_i64(), n.as_u64(), n.as_f64()) {
            (Some(i), _, _) => i.to_string(),
            (_, Some(u), _) => u.to_string(),
            (_, _, Some(f)) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{f:.0}"),
            (_, _, Some(f)) => f.to_string(),
            _ => String::new(),
        },
        Some(Value::Bool(true)) => "true".into(),
        // `String({})` is "[object Object]" (and `String([a, b])` is "a,b"): no lyrics service
        // sends either where text belongs, and neither would match a name, so both read as
        // "[object Object]" here.
        Some(Value::Array(_)) | Some(Value::Object(_)) => "[object Object]".into(),
        _ => String::new(),
    }
}

/// `Math.round` for what it is used on here: a song's length in seconds, never negative.
pub fn round(x: f64) -> f64 {
    (x + 0.5).floor()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spaces_are_javascripts() {
        for c in [
            '\u{3000}', '\u{A0}', '\u{FEFF}', '\u{2028}', '\u{200A}', '\u{0B}',
        ] {
            assert!(is_space(c), "{c:?}");
        }
        // Rust's whitespace has NEL; JavaScript's \s does not.
        assert!(!is_space('\u{85}'));
        assert!(!is_space('\u{200B}'));
        assert_eq!(trim("\u{3000} a b \u{FEFF}"), "a b");
        assert_eq!(collapse_spaces("a \u{3000}\t b"), "a b");
        assert_eq!(split_spaces(""), [""]);
        assert_eq!(split_spaces("a  b\u{3000}c"), ["a", "b", "c"]);
        // The regex class says the same as the function.
        let re = regex_lite::Regex::new(&format!("^{SPACE_CLASS}$")).unwrap();
        for c in (0..=0x3100u32)
            .filter_map(char::from_u32)
            .chain(['\u{FEFF}'])
        {
            assert_eq!(re.is_match(&c.to_string()), is_space(c), "{c:?}");
        }
    }

    #[test]
    fn encodes_as_encode_uri_component() {
        assert_eq!(
            encode_uri_component("The Band & Co - It's (Live) a/b?c=d+é 星"),
            "The%20Band%20%26%20Co%20-%20It's%20(Live)%20a%2Fb%3Fc%3Dd%2B%C3%A9%20%E6%98%9F"
        );
        assert_eq!(encode_uri_component("A-Z_a.z!~*'()09"), "A-Z_a.z!~*'()09");
    }

    #[test]
    fn values_as_javascript_reads_them() {
        assert!(!truthy(None));
        for v in [json!(null), json!(false), json!(0), json!(""), json!(0.0)] {
            assert!(!truthy(Some(&v)), "{v}");
        }
        for v in [json!(true), json!(1), json!("0"), json!([]), json!({})] {
            assert!(truthy(Some(&v)), "{v}");
        }
        assert_eq!(number(Some(&json!(201000))), 201000.0);
        assert_eq!(number(Some(&json!("216"))), 216.0);
        assert_eq!(number(Some(&json!(" 2.5 "))), 2.5);
        assert_eq!(number(None), 0.0);
        assert!(number(Some(&json!("12s"))).is_nan());
        assert!(number(Some(&json!("inf"))).is_nan());
        assert!(number(Some(&json!({}))).is_nan());
        assert_eq!(string(Some(&json!(7001))), "7001");
        assert_eq!(string(Some(&json!("9002"))), "9002");
        assert_eq!(string(Some(&json!(null))), "");
        assert_eq!(string(Some(&json!(0))), "");
        assert_eq!(utf16_len("a星😀"), 4);
        assert_eq!(round(213.5), 214.0);
        assert_eq!(round(213.49), 213.0);
    }
}
