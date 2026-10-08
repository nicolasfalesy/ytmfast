//! The browsing requests: `browse` (a page), `search`, a list's next page, like / dislike (and
//! a song's like status), and lyrics. All go out as the music web client (`clients::WEB_REMIX`)
//! to music.youtube.com, the same as `next`, through the one posting path (`Innertube::post`):
//! the session's cookies and SAPISIDHASH, `Set-Cookie` rotations kept, the host allowlist, the
//! 32 MiB read cap and the fixed-text error mapping. The answers become `crate::browse`'s small
//! shapes at once; no raw YouTube JSON leaves this module.
//!
//! Every input is checked for shape before anything is sent (`Error::BadRequest`): ids with
//! `browse::id_ok` / `streams::is_video_id`, `params` and tokens with `browse::token_ok`, the
//! search text for length and control characters. They go back to YouTube inside a JSON body,
//! where the charset checks keep anything but an id or token out. No error or log line holds
//! an id, a query, a token or a URL (ruling R6): failures are logged by endpoint and code only.
//!
//! Every method takes `&self` and holds no lock across an await (the session lock is held only
//! to copy the cookies), so one shared `Arc<Innertube>` serves the engine's browsing and the
//! queue source's `next` at the same time.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::next::{context, request_body};
use super::{Innertube, NextRequest, SongNext, clients};
use crate::browse::{self, LikeStatus, Lyrics, MorePage, Page, SearchPage, id_ok, token_ok};
use crate::error::Error;
use crate::streams::is_video_id;

/// The longest search, in characters, after trimming. The widget's box never needs more, and
/// it bounds what one request can make YouTube (and the log of a refusal) carry.
pub const MAX_QUERY: usize = 200;

/// Which list a continuation token belongs to: it goes back to the endpoint that gave it.
/// On the socket: `"browse"` or `"search"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoreKind {
    Browse,
    Search,
}

impl MoreKind {
    fn endpoint(self) -> &'static str {
        match self {
            MoreKind::Browse => "browse",
            MoreKind::Search => "search",
        }
    }
}

impl Innertube {
    /// A page: Home, the library, a playlist, an album, an artist, a podcast, or a section's
    /// "more" page (`params` from its link).
    ///
    /// Errors: `BadRequest` for a malformed id or params (nothing is sent), `SignedOut` for no
    /// session or a 401, `Network` for transport trouble, an error status or an answer over
    /// 32 MiB, `Internal` for an answer that is not JSON. An answer with nothing we can list
    /// is an empty page, as `Page.js` gave.
    pub async fn browse(&self, browse_id: &str, params: Option<&str>) -> Result<Page, Error> {
        if !id_ok(browse_id) {
            return Err(Error::BadRequest("not a browse id".into()));
        }
        let mut fields = Map::new();
        fields.insert("browseId".into(), json!(browse_id));
        if let Some(p) = check_params(params)? {
            fields.insert("params".into(), json!(p));
        }
        let answer = self.post_json("browse", fields).await?;
        Ok(browse::parse_browse(&answer))
    }

    /// A search. With a filter chip's `params` (Songs, Albums, ...) it is a filtered search,
    /// which pages and keeps more rows a section (see `browse::parse_search`).
    ///
    /// The query is trimmed, and must then be 1 to `MAX_QUERY` characters with no control or
    /// invisible characters (`check_query`). Errors as for `browse`.
    pub async fn search(&self, query: &str, params: Option<&str>) -> Result<SearchPage, Error> {
        let query = check_query(query)?;
        let params = check_params(params)?;
        let mut fields = Map::new();
        fields.insert("query".into(), json!(query));
        if let Some(p) = params {
            fields.insert("params".into(), json!(p));
        }
        let answer = self.post_json("search", fields).await?;
        Ok(browse::parse_search(&answer, params.is_some()))
    }

    /// A list's next page: the token alone, sent back to the endpoint the list came from.
    /// Errors as for `browse`.
    pub async fn more(&self, kind: MoreKind, token: &str) -> Result<MorePage, Error> {
        if !token_ok(token) {
            return Err(Error::BadRequest("not a continuation token".into()));
        }
        let mut fields = Map::new();
        fields.insert("continuation".into(), json!(token));
        let answer = self.post_json(kind.endpoint(), fields).await?;
        Ok(browse::parse_more(&answer))
    }

    /// Sets a song's like status: `like/like`, `like/dislike`, or `like/removelike` for
    /// `Indifferent`. The answer's body is read (capped) but not looked at: a 2xx is done.
    ///
    /// Errors: `BadRequest` for a malformed video id, `SignedOut` for no session or a 401 or
    /// 403 (an account action refused), otherwise as for `browse`.
    pub async fn like(&self, video_id: &str, status: LikeStatus) -> Result<(), Error> {
        check_video_id(video_id)?;
        let endpoint = match status {
            LikeStatus::Like => "like/like",
            LikeStatus::Dislike => "like/dislike",
            LikeStatus::Indifferent => "like/removelike",
        };
        let mut fields = Map::new();
        fields.insert("target".into(), json!({ "videoId": video_id }));
        self.post_with(&clients::WEB_REMIX, endpoint, &body(fields), true)
            .await
            .map(drop)
            .inspect_err(|e| log_failure(endpoint, e))
    }

    /// A song's like status: the song's `next` (`song_next`), read for its like button. For a
    /// song whose queue fetch didn't name it (ruling P1). Errors as for `browse`.
    pub async fn like_status(&self, video_id: &str) -> Result<Option<LikeStatus>, Error> {
        Ok(self.song_next(video_id).await?.like)
    }

    /// A song's own `next` (the queue's body for one song, as the app asks it), read for what
    /// it says about the song: its like status (`browse::parse_like_for`) and its Lyrics tab
    /// (`browse::parse_lyrics_tab`). One answer serves the like lookup and lyrics alike.
    /// Errors: `BadRequest` for a malformed video id (nothing is sent), else as for `browse`.
    pub async fn song_next(&self, video_id: &str) -> Result<SongNext, Error> {
        check_video_id(video_id)?;
        let body = request_body(
            &clients::WEB_REMIX,
            &NextRequest {
                video_id: Some(video_id.to_owned()),
                ..NextRequest::default()
            },
        );
        let next = self.post_value("next", &body).await?;
        Ok(SongNext {
            like: browse::parse_like_for(&next, video_id),
            // `parse_lyrics_tab` shape-checks the id it returns.
            lyrics_tab: browse::parse_lyrics_tab(&next),
        })
    }

    /// The lyrics page a song's Lyrics tab names (`SongNext::lyrics_tab`): one browse.
    /// `Ok(None)` when the page has no text. Errors: `BadRequest` for a malformed page id
    /// (nothing is sent), else as for `browse`.
    pub async fn lyrics_page(&self, page_id: &str) -> Result<Option<Lyrics>, Error> {
        if !id_ok(page_id) {
            return Err(Error::BadRequest("not a browse id".into()));
        }
        let mut fields = Map::new();
        fields.insert("browseId".into(), json!(page_id));
        let page = self.post_json("browse", fields).await?;
        Ok(browse::parse_lyrics(&page).map(|(text, source)| Lyrics { text, source }))
    }

    /// A song's lyrics as YouTube Music shows them, `(text, source)`: two requests, the song's
    /// `next` (whose Lyrics tab names the lyrics page) and then a browse of that page.
    /// `Ok(None)` when the song has no Lyrics tab, or the page has no text. Errors as for
    /// `browse`. The socket's `lyrics` makes the same two through `browse::Browser`, skipping
    /// the first when the engine already knows the tab (`control::lyrics`).
    pub async fn lyrics(&self, video_id: &str) -> Result<Option<(String, String)>, Error> {
        let Some(page_id) = self.song_next(video_id).await?.lyrics_tab else {
            return Ok(None);
        };
        Ok(self
            .lyrics_page(&page_id)
            .await?
            .map(|l| (l.text, l.source)))
    }

    /// The signed-in account's name, as YouTube Music's account button shows it: one
    /// `account/account_menu` request. `import-session` asks it before saving a session, so a
    /// session that signs nobody in is never saved and a wrong Google account is seen at once
    /// (spec A2, ruling P19).
    ///
    /// The name is cleaned for a terminal and the bar (`account_name_from`). Errors:
    /// `SignedOut` for no session, a 401 or 403, or an answer with no account in it (YouTube
    /// answers a dead session with the signed-out menu, not an error); otherwise as for
    /// `browse`. Not logged here: the one caller prints its own fixed line, and a log line
    /// before it would only push it off the last line the bar widget reads.
    pub async fn account_name(&self) -> Result<String, Error> {
        Ok(self.account().await?.name)
    }

    /// The signed-in account: its name (as `account_name`) and its channel id, from the same
    /// one `account/account_menu` request. The id is what a renewed session is matched on
    /// (`Session::account`): unlike the name, two Google accounts can't share it.
    pub async fn account(&self) -> Result<Account, Error> {
        const ENDPOINT: &str = "account/account_menu";
        let bytes = self
            .post_with(&clients::WEB_REMIX, ENDPOINT, &body(Map::new()), true)
            .await?;
        let answer: Value = serde_json::from_slice(&bytes)
            .map_err(|_| Error::Internal(format!("the {ENDPOINT} answer could not be read")))?;
        Ok(Account {
            name: account_name_from(&answer).ok_or(Error::SignedOut)?,
            channel_id: account_channel_from(&answer),
        })
    }

    /// POSTs the music web context plus `fields` to `endpoint` and reads the answer as JSON.
    async fn post_json(
        &self,
        endpoint: &'static str,
        fields: Map<String, Value>,
    ) -> Result<Value, Error> {
        self.post_value(endpoint, &body(fields)).await
    }

    /// POSTs `body` as the music web client and reads the answer as JSON. A failure is logged
    /// here by endpoint and code only (nothing the caller sent is in either).
    async fn post_value(&self, endpoint: &'static str, body: &Value) -> Result<Value, Error> {
        let result = async {
            let bytes = self.post(&clients::WEB_REMIX, endpoint, body).await?;
            // Fixed text: serde_json's message can quote part of the answer, and an answer can
            // quote the request back.
            serde_json::from_slice(&bytes)
                .map_err(|_| Error::Internal(format!("the {endpoint} answer could not be read")))
        }
        .await;
        result.inspect_err(|e| log_failure(endpoint, e))
    }
}

/// The socket's browsing commands (`browse::Browser`), straight to the methods above.
#[async_trait::async_trait]
impl browse::Browser for Innertube {
    async fn browse(&self, browse_id: &str, params: Option<&str>) -> Result<Page, Error> {
        // The inherent methods, not these.
        Innertube::browse(self, browse_id, params).await
    }

    async fn search(&self, query: &str, params: Option<&str>) -> Result<SearchPage, Error> {
        Innertube::search(self, query, params).await
    }

    async fn more(&self, kind: MoreKind, token: &str) -> Result<MorePage, Error> {
        Innertube::more(self, kind, token).await
    }

    async fn song_next(&self, video_id: &str) -> Result<SongNext, Error> {
        Innertube::song_next(self, video_id).await
    }

    async fn lyrics_page(&self, page_id: &str) -> Result<Option<Lyrics>, Error> {
        Innertube::lyrics_page(self, page_id).await
    }
}

/// The request body: the music web client's context, then `fields`.
fn body(fields: Map<String, Value>) -> Value {
    let mut body = Map::new();
    body.insert("context".into(), context(&clients::WEB_REMIX));
    body.extend(fields);
    Value::Object(body)
}

/// `params` as sent, or `None`. An empty one counts as none: rows and links carry `""` for "no
/// params" (the `Page.js` shapes never use null), and that is how a client sends one back.
pub fn check_params(params: Option<&str>) -> Result<Option<&str>, Error> {
    match params {
        None | Some("") => Ok(None),
        Some(p) if token_ok(p) => Ok(Some(p)),
        Some(_) => Err(Error::BadRequest("not a params value".into())),
    }
}

/// The search text as sent: trimmed, 1 to `MAX_QUERY` characters (not bytes: a search in
/// another script is as long as it looks), no control characters and no `invisible`
/// characters (a newline, an escape sequence or a bidi override is never something the user
/// typed into a search box).
pub fn check_query(query: &str) -> Result<&str, Error> {
    let q = query.trim();
    if q.is_empty() {
        return Err(Error::BadRequest("the search is empty".into()));
    }
    if q.chars().nth(MAX_QUERY).is_some() {
        return Err(Error::BadRequest(format!(
            "the search is longer than {MAX_QUERY} characters"
        )));
    }
    if q.chars().any(char::is_control) {
        return Err(Error::BadRequest(
            "the search holds a control character".into(),
        ));
    }
    if q.chars().any(invisible) {
        return Err(Error::BadRequest(
            "the search holds an invisible character".into(),
        ));
    }
    Ok(q)
}

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
fn invisible(c: char) -> bool {
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

/// The longest account name kept, in characters: Google's own limit is far shorter, and a
/// line in a terminal or a label in the bar has no room for more.
const MAX_ACCOUNT_NAME: usize = 100;

/// The account's name in an `account/account_menu` answer (its header's `accountName` runs,
/// joined), or `None` when the answer names no account. Control and invisible characters are
/// dropped (an escape sequence or a bidi override must reach neither a terminal nor the bar),
/// the ends trimmed, and the name cut to `MAX_ACCOUNT_NAME` characters; an empty result is no
/// name.
fn account_name_from(answer: &Value) -> Option<String> {
    let runs = answer
        .pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header")?
        .pointer("/activeAccountHeaderRenderer/accountName/runs")?
        .as_array()?;
    let joined: String = runs
        .iter()
        .filter_map(|r| r.get("text")?.as_str())
        .flat_map(str::chars)
        .filter(|c| !c.is_control() && !invisible(*c))
        .collect();
    let name: String = joined.trim().chars().take(MAX_ACCOUNT_NAME).collect();
    let name = name.trim_end().to_owned();
    (!name.is_empty()).then_some(name)
}

/// The signed-in account, as `Innertube::account` reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    /// The account's own YouTube channel (`UC` and 22 more characters), or `None` when the
    /// menu links to none.
    pub channel_id: Option<String>,
}

/// The account's channel id in an `account/account_menu` answer: the first `browseEndpoint`
/// in the menu's sections that opens a channel (`UC…`). In the answers seen (2026-10-07) that
/// is the menu's first item, "Your channel"; the search does not depend on its position, only
/// on the menu linking to one channel before any other, which the menu (the account's own
/// links) does.
fn account_channel_from(answer: &Value) -> Option<String> {
    fn first_channel(v: &Value) -> Option<String> {
        match v {
            Value::Object(o) => {
                if let Some(id) = o
                    .get("browseEndpoint")
                    .and_then(|b| b.get("browseId"))
                    .and_then(Value::as_str)
                    .filter(|id| is_channel_id(id))
                {
                    return Some(id.to_owned());
                }
                o.values().find_map(first_channel)
            }
            Value::Array(a) => a.iter().find_map(first_channel),
            _ => None,
        }
    }
    first_channel(
        answer.pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/sections")?,
    )
}

/// A YouTube channel id: `UC` and 22 characters of the URL-safe base64 alphabet.
fn is_channel_id(id: &str) -> bool {
    id.len() == 24
        && id.starts_with("UC")
        && id[2..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn check_video_id(id: &str) -> Result<(), Error> {
    if is_video_id(id) {
        Ok(())
    } else {
        Err(Error::BadRequest("not a video id".into()))
    }
}

/// One line per failed request: the endpoint and the error's code, never its text (the text
/// is fixed today, but the code is all a reader of the log needs, and it can't grow a value).
fn log_failure(endpoint: &str, e: &Error) {
    eprintln!("ytmfast: the {endpoint} request failed ({})", e.code());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_checks() {
        assert_eq!(check_query("  a b  "), Ok("a b"));
        assert_eq!(check_query(&"é".repeat(MAX_QUERY)).map(str::len), Ok(400));
        for bad in ["", " \n ", "a\u{0}b", "a\u{1b}[2Jb", "a\u{85}b"] {
            assert_eq!(
                check_query(bad).unwrap_err().code(),
                "bad_request",
                "{bad:?}"
            );
        }
        assert!(check_query(&"x".repeat(MAX_QUERY + 1)).is_err());
    }

    #[test]
    fn query_refuses_separators_and_invisible_format_characters() {
        // Line and paragraph separators (Zl, Zp), bidi controls (which can make the text read
        // in another order than it was sent) and invisible zero-width characters are never
        // typed into a search box.
        for bad in [
            "a\u{2028}b",
            "a\u{2029}b",
            "a\u{202E}b",
            "a\u{202A}b",
            "a\u{2066}b",
            "a\u{2069}b",
            "a\u{200E}b",
            "a\u{200F}b",
            "a\u{061C}b",
            "a\u{200B}b",
            "a\u{2060}b",
            "a\u{FEFF}b",
            "a\u{00AD}b",
            "a\u{180E}b",
            "a\u{206A}b",
            "a\u{FFF9}b",
            "a\u{E0001}b",
            "a\u{13430}b",
            "a\u{1343F}b",
            "a\u{1BCA0}b",
            "a\u{1BCA3}b",
            "a\u{1D173}b",
            "a\u{1D17A}b",
        ] {
            assert_eq!(
                check_query(bad).unwrap_err().code(),
                "bad_request",
                "{bad:?}"
            );
        }
        // Joiners stay: Persian and Indic text (ZWNJ, ZWJ) and emoji sequences need them, as
        // do the tag characters of subdivision flags; and the prepended number marks, which
        // show as signs (rulings P12, P12a).
        for good in [
            "\u{0645}\u{06CC}\u{200C}\u{062E}\u{0648}\u{0627}\u{0645}",
            "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}",
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0073}\u{E0063}\u{E0074}\u{E007F}",
            "\u{0600}12",
            "\u{0605}12",
            "\u{06DD}12",
            "\u{070F}a",
            "\u{0890}12",
            "\u{0891}12",
            "\u{08E2}12",
            "\u{110BD}12",
            "\u{110CD}12",
        ] {
            assert_eq!(check_query(good), Ok(good), "{good:?}");
        }
    }

    #[test]
    fn empty_params_are_none() {
        assert_eq!(check_params(None), Ok(None));
        assert_eq!(check_params(Some("")), Ok(None));
        assert_eq!(check_params(Some("ab+/=")), Ok(Some("ab+/=")));
        assert!(check_params(Some("a b")).is_err());
    }

    #[test]
    fn account_channel_is_the_first_channel_link_in_the_sections() {
        let menu = |sections: Value| {
            json!({"actions": [{"openPopupAction": {"popup": {"multiPageMenuRenderer": {
                "header": {"activeAccountHeaderRenderer": {
                    "settingsEndpoint": {"browseEndpoint": {"browseId": "UCheaderheaderheaderhead"}}}},
                "sections": sections}}}}]})
        };
        let link = |id: &str| json!({"compactLinkRenderer": {"navigationEndpoint": {"browseEndpoint": {"browseId": id}}}});
        let section = |items: Vec<Value>| json!({"multiPageMenuSectionRenderer": {"items": items}});
        assert_eq!(
            account_channel_from(&menu(json!([section(vec![
                link("FEmusic_library"),
                link("UCfake0fake1fake2fake3fa"),
                link("UCsecondsecondsecondseco"),
            ])]))),
            Some("UCfake0fake1fake2fake3fa".into())
        );
        for none in [
            json!({}),
            // Only the header links a channel: the header is not searched.
            menu(json!([])),
            // Not a channel id: too short, or a character outside the alphabet.
            menu(json!([section(vec![link("UCshort")])])),
            menu(json!([section(vec![link("UCfake0fake1fake2fake3/a")])])),
            menu(json!([section(vec![link("VLAABaXVryaMi-_t2HXKVEQw")])])),
        ] {
            assert_eq!(account_channel_from(&none), None, "{none}");
        }
    }

    #[test]
    fn account_name_reads_the_header_only() {
        let menu = |header: Value| {
            json!({"actions": [{"openPopupAction": {"popup": {"multiPageMenuRenderer": {
                "header": header}}}}]})
        };
        let named = |runs: Value| {
            menu(json!({"activeAccountHeaderRenderer": {"accountName": {"runs": runs}}}))
        };
        assert_eq!(
            account_name_from(&named(json!([{"text": "Fake"}, {"text": " Person"}]))),
            Some("Fake Person".into())
        );
        // A run without text is skipped, not the whole name.
        assert_eq!(
            account_name_from(&named(json!([{"text": "Fake"}, {"bold": true}]))),
            Some("Fake".into())
        );
        for none in [
            json!({}),
            json!({"actions": []}),
            menu(json!({"someOtherHeaderRenderer": {}})),
            named(json!("Fake Person")),
            named(json!([])),
            named(json!([{"text": "\u{200F}\t"}])),
        ] {
            assert_eq!(account_name_from(&none), None, "{none}");
        }
        // Cut at the cap, and never left ending in a space.
        let long = format!("{} b", "a".repeat(MAX_ACCOUNT_NAME - 1));
        assert_eq!(
            account_name_from(&named(json!([{ "text": long }]))),
            Some("a".repeat(MAX_ACCOUNT_NAME - 1))
        );
    }

    #[test]
    fn body_has_the_context_first() {
        let mut fields = Map::new();
        fields.insert("browseId".into(), json!("FEmusic_home"));
        let b = body(fields);
        let keys: Vec<&String> = b.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["context", "browseId"]);
        assert_eq!(b["context"]["client"]["clientName"], "WEB_REMIX");
    }
}
