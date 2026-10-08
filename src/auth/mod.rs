//! The YouTube Music session: its cookies, where it is stored, and how it is kept fresh.
//!
//! The session is imported once from the `pear-desktop` profile (`chromium::import`) or the
//! Brave Origin one (`chromium::import_brave_origin`, its key from `SafeStorageKeys`), stored in
//! the login keyring (`KeyringStore`), sent on every request (`Session::cookie_header`) and kept
//! up to date from `Set-Cookie` answers (`Session::apply_set_cookie`), because Google rotates
//! some of these cookies.
//!
//! Cookie values are secrets. Nothing in here puts one in an error, a log line or `Debug`
//! output: `Cookie`'s `Debug` prints `<redacted>` in place of the value.

pub mod chromium;
pub mod sidhash;

use std::fmt;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::Error;

/// One cookie. `domain` follows Chromium's `host_key` convention: a leading dot means the
/// cookie is sent to that domain and every subdomain; no dot means only that exact host.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cookie {
    pub domain: String,
    pub name: String,
    pub value: String,
    pub path: String,
    pub secure: bool,
    /// Expiry in Unix seconds; `None` for a cookie with no expiry (a "session" cookie).
    pub expires_utc: Option<i64>,
}

impl fmt::Debug for Cookie {
    /// Everything but the value: the value is the secret, and `Debug` output ends up in
    /// panics, logs and test failures.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("domain", &self.domain)
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("path", &self.path)
            .field("secure", &self.secure)
            .field("expires_utc", &self.expires_utc)
            .finish()
    }
}

impl Cookie {
    /// True when this cookie goes on a request to `host` + `path` (RFC 6265 5.4): the domain
    /// and path match, it is not `Secure` on a plain-http request, and it has not expired.
    pub(crate) fn applies_to(&self, host: &str, path: &str, https: bool, now: i64) -> bool {
        domain_matches(&self.domain, host)
            && path_matches(&self.path, path)
            && (https || !self.secure)
            && self.expires_utc.is_none_or(|t| t > now)
    }
}

/// A leading dot means "this domain and its subdomains"; no dot means "this host only".
fn domain_matches(cookie_domain: &str, host: &str) -> bool {
    match cookie_domain.strip_prefix('.') {
        Some(domain) => host_in_domain(host, domain),
        None => host.eq_ignore_ascii_case(cookie_domain),
    }
}

/// `host` is `domain` or a subdomain of it. The dot check is what stops `evilyoutube.com`
/// from matching `youtube.com`.
fn host_in_domain(host: &str, domain: &str) -> bool {
    if host.eq_ignore_ascii_case(domain) {
        return true;
    }
    let Some(cut) = host.len().checked_sub(domain.len() + 1) else {
        return false;
    };
    host.get(cut..cut + 1) == Some(".")
        && host
            .get(cut + 1..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(domain))
}

/// RFC 6265 5.1.4: `/youtubei` matches `/youtubei` and `/youtubei/v1`, not `/youtubeiv`.
fn path_matches(cookie_path: &str, request_path: &str) -> bool {
    match request_path.strip_prefix(cookie_path) {
        Some(rest) => rest.is_empty() || cookie_path.ends_with('/') || rest.starts_with('/'),
        None => false,
    }
}

/// RFC 6265 5.1.4 default-path: the request path up to (not including) its last `/`.
fn default_path(url: &Url) -> String {
    let path = url.path();
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].into(),
    }
}

/// A new expiry within this much of the stored one is not worth a keyring write. Cookies
/// set with `Max-Age` move their expiry forward on every answer; saving for that alone would
/// write the keyring on every API call. A day keeps the stored expiry close enough that a
/// restarted engine never drops a live cookie as expired.
const EXPIRY_SLACK_SECS: u64 = 86_400;

fn expiry_close(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a.abs_diff(b) <= EXPIRY_SLACK_SECS,
        (None, None) => true,
        _ => false,
    }
}

/// Case-insensitive name-prefix check, as RFC 6265bis does for `__Secure-` and `__Host-`.
fn has_prefix(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub cookies: Vec<Cookie>,
}

impl Session {
    /// The `Cookie` header value for a request to `url`: every cookie whose domain, path and
    /// `Secure` flag fit the URL and that has not expired, as `name=value` joined by `; `.
    pub fn cookie_header(&self, url: &Url) -> String {
        self.cookie_header_at(url, now_unix())
    }

    fn cookie_header_at(&self, url: &Url, now: i64) -> String {
        let mut matched = self.matching(url, now);
        // RFC 6265 5.4: longer paths first. The sort is stable, so equal paths keep the
        // stored order.
        matched.sort_by_key(|c| std::cmp::Reverse(c.path.len()));
        matched
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The cookies that apply to a request to `url` at `now`, in stored order.
    pub(crate) fn matching(&self, url: &Url, now: i64) -> Vec<&Cookie> {
        let Some(host) = url.host_str() else {
            return Vec::new();
        };
        let https = url.scheme() == "https";
        self.cookies
            .iter()
            .filter(|c| c.applies_to(host, url.path(), https, now))
            .collect()
    }

    /// Applies one `Set-Cookie` header received from `url`. Returns true when the stored
    /// session changed (so the caller saves it). A header that is malformed, or that tries
    /// to set a cookie for a domain `url` doesn't belong to, is ignored (false).
    pub fn apply_set_cookie(&mut self, url: &Url, header: &str) -> bool {
        self.apply_set_cookie_at(url, header, now_unix())
    }

    fn apply_set_cookie_at(&mut self, url: &Url, header: &str, now: i64) -> bool {
        let Some(host) = url.host_str().map(str::to_ascii_lowercase) else {
            return false;
        };
        // RFC 6265 5.2: the first `;`-separated part is name=value, the rest are attributes.
        let mut parts = header.split(';');
        let Some((name, value)) = parts.next().and_then(|nv| nv.split_once('=')) else {
            return false;
        };
        let (name, value) = (name.trim(), value.trim());
        if name.is_empty() {
            return false;
        }

        let mut domain_attr: Option<String> = None;
        let mut path_attr: Option<String> = None;
        let mut secure = false;
        let mut expires: Option<i64> = None;
        let mut max_age: Option<i64> = None;
        for attr in parts {
            let (key, val) = match attr.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (attr.trim(), ""),
            };
            match key.to_ascii_lowercase().as_str() {
                "expires" => {
                    if let Some(t) = parse_cookie_date(val) {
                        expires = Some(t);
                    }
                }
                "max-age" => {
                    if let Ok(n) = val.parse::<i64>() {
                        max_age = Some(n);
                    }
                }
                "domain" => {
                    let d = val.strip_prefix('.').unwrap_or(val).to_ascii_lowercase();
                    if !d.is_empty() {
                        domain_attr = Some(d);
                    }
                }
                // A path that isn't absolute means "use the default" (RFC 6265 5.2.4).
                "path" => path_attr = val.starts_with('/').then(|| val.to_string()),
                "secure" => secure = true,
                _ => {}
            }
        }

        let domain = match &domain_attr {
            // A domain without a dot ("com") would send the cookie to every host under that
            // top-level domain; and a server may only set cookies for its own domain.
            Some(d) if !d.contains('.') || !host_in_domain(&host, d) => return false,
            Some(d) => format!(".{d}"),
            None => host,
        };
        let path = path_attr.unwrap_or_else(|| default_path(url));
        if has_prefix(name, "__Secure-") && !secure {
            return false;
        }
        if has_prefix(name, "__Host-") && (!secure || domain_attr.is_some() || path != "/") {
            return false;
        }
        // Max-Age wins over Expires (RFC 6265 5.3 step 3). Zero or less means "delete now".
        let expires_utc = match max_age {
            Some(n) if n <= 0 => Some(i64::MIN),
            Some(n) => Some(now.saturating_add(n)),
            None => expires,
        };

        let existing = self.cookies.iter().position(|c| {
            c.name == name && c.path == path && c.domain.eq_ignore_ascii_case(&domain)
        });
        if expires_utc.is_some_and(|t| t <= now) {
            return match existing {
                Some(i) => {
                    self.cookies.remove(i);
                    true
                }
                None => false,
            };
        }
        let new = Cookie {
            domain,
            name: name.to_string(),
            value: value.to_string(),
            path,
            secure,
            expires_utc,
        };
        match existing {
            Some(i) => {
                let old = &mut self.cookies[i];
                let changed = old.value != new.value
                    || old.secure != new.secure
                    || !expiry_close(old.expires_utc, new.expires_utc);
                *old = new;
                changed
            }
            None => {
                self.cookies.push(new);
                true
            }
        }
    }
}

/// Parses a cookie date the way browsers do (RFC 6265 5.1.1), which accepts the dashed
/// `Mon, 04-Oct-2027 10:00:00 GMT` form Google sends as well as the HTTP-date forms.
fn parse_cookie_date(s: &str) -> Option<i64> {
    let is_delimiter = |c: char| matches!(c, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~');
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in s.split(is_delimiter).filter(|t| !t.is_empty()) {
        if time.is_none()
            && let Some(t) = parse_hms(token)
        {
            time = Some(t);
        } else if day.is_none()
            && let Some(d) = leading_number(token, 1, 2)
        {
            day = Some(d);
        } else if month.is_none()
            && let Some(m) = month_number(token)
        {
            month = Some(m);
        } else if year.is_none()
            && let Some(y) = leading_number(token, 2, 4)
        {
            year = Some(y);
        }
    }
    let ((hour, minute, second), day, month, mut year) = (time?, day?, month?, year?);
    // Two-digit years, as RFC 6265 5.1.1 step 3 and 4 say.
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year as i64, month, day as i64);
    Some(days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64)
}

/// `n` digits at the start of `token` (min..=max of them), followed by a non-digit or the end.
fn leading_number(token: &str, min: usize, max: usize) -> Option<u32> {
    let n = token.bytes().take_while(u8::is_ascii_digit).count();
    if n < min || n > max {
        return None;
    }
    token[..n].parse().ok()
}

/// `hh:mm:ss`, each one or two digits; anything may follow the seconds.
fn parse_hms(token: &str) -> Option<(u32, u32, u32)> {
    let mut fields = token.splitn(3, ':');
    let (h, m, s) = (fields.next()?, fields.next()?, fields.next()?);
    let exact = |f: &str| (1..=2).contains(&f.len()) && f.bytes().all(|b| b.is_ascii_digit());
    if !exact(h) || !exact(m) {
        return None;
    }
    Some((h.parse().ok()?, m.parse().ok()?, leading_number(s, 1, 2)?))
}

fn month_number(token: &str) -> Option<i64> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let head = token.get(..3)?;
    MONTHS
        .iter()
        .position(|m| head.eq_ignore_ascii_case(m))
        .map(|i| i as i64 + 1)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm), so no
/// date crate is needed for one conversion.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Where the session is kept between runs.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// The stored session, or `SignedOut` when there is none.
    async fn load(&self) -> Result<Session, Error>;
    /// Stores `s`, replacing any earlier session.
    async fn save(&self, s: &Session) -> Result<(), Error>;
    /// Stores `s` only if that needs nothing from the user: no unlock prompt. For saves in
    /// the background (cookie rotations), where nobody is waiting to answer a prompt and a
    /// prompt would hold the save open. A store with no prompts just saves.
    async fn save_without_prompt(&self, s: &Session) -> Result<(), Error> {
        self.save(s).await
    }
}

/// An in-memory store, for tests.
#[derive(Default)]
pub struct MemoryStore {
    session: Mutex<Option<Session>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SessionStore for MemoryStore {
    async fn load(&self) -> Result<Session, Error> {
        let guard = self.session.lock().unwrap_or_else(|e| e.into_inner());
        guard.clone().ok_or(Error::SignedOut)
    }

    async fn save(&self, s: &Session) -> Result<(), Error> {
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.clone());
        Ok(())
    }
}

/// The keyring item's label, as the user sees it in a keyring manager.
const KEYRING_LABEL: &str = "ytmfast session";
/// The attributes the item is found by (`secret-tool search application ytmfast`).
const KEYRING_ATTRIBUTES: [(&str, &str); 1] = [("application", "ytmfast")];

/// The real store: the user's login keyring (the Secret Service's default collection), with
/// the session as a JSON secret.
///
/// It talks to the Secret Service over D-Bus only (`oo7::dbus`), never `oo7::Keyring`: that
/// one silently switches to a file-backed keyring inside a sandbox, and the session must
/// never be a file. A fresh D-Bus connection is opened per call: the engine loads when a song
/// is first asked for (and again only until a session is found, `streams::lazy`) and saves
/// only when Google rotates a cookie, so a kept-open connection would buy nothing.
pub struct KeyringStore;

impl KeyringStore {
    pub fn new() -> Self {
        KeyringStore
    }
}

impl Default for KeyringStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Every Secret Service failure becomes this one fixed message: the oo7/zbus error text can
/// name D-Bus object paths and isn't useful to the user, and it must never carry a secret.
fn keyring_unavailable<E>(_: E) -> Error {
    Error::Internal("keyring locked or unavailable".into())
}

/// Unlocks `collection` if it is locked. This may show the desktop's unlock prompt; a
/// dismissed or failed prompt is an error.
async fn unlock(collection: &oo7::dbus::Collection) -> Result<(), Error> {
    if collection.is_locked().await.map_err(keyring_unavailable)? {
        collection.unlock(None).await.map_err(keyring_unavailable)?;
        if collection.is_locked().await.map_err(keyring_unavailable)? {
            return Err(keyring_unavailable(()));
        }
    }
    Ok(())
}

#[async_trait]
impl SessionStore for KeyringStore {
    async fn load(&self) -> Result<Session, Error> {
        let service = oo7::dbus::Service::new()
            .await
            .map_err(keyring_unavailable)?;
        // `with_alias`, not `default_collection`: a load must not create a collection.
        let Some(collection) = service
            .with_alias(oo7::dbus::Service::DEFAULT_COLLECTION)
            .await
            .map_err(keyring_unavailable)?
        else {
            return Err(Error::SignedOut);
        };
        unlock(&collection).await?;
        let items = collection
            .search_items(&KEYRING_ATTRIBUTES)
            .await
            .map_err(keyring_unavailable)?;
        let Some(item) = items.first() else {
            return Err(Error::SignedOut);
        };
        if item.is_locked().await.map_err(keyring_unavailable)? {
            item.unlock(None).await.map_err(keyring_unavailable)?;
        }
        let secret = item.secret().await.map_err(keyring_unavailable)?;
        // A fixed message: serde_json's error text can quote part of the input, which is
        // the session itself.
        serde_json::from_slice(secret.as_bytes()).map_err(|_| {
            Error::Internal(
                "the stored session is unreadable; run `ytmfast import-session` again".into(),
            )
        })
    }

    async fn save(&self, s: &Session) -> Result<(), Error> {
        keyring_save(s, true).await
    }

    async fn save_without_prompt(&self, s: &Session) -> Result<(), Error> {
        keyring_save(s, false).await
    }
}

/// Writes the session to the login keyring. With `prompt` false, a locked collection is an
/// error instead of an unlock prompt; the caller keeps the session in memory and logs it.
async fn keyring_save(s: &Session, prompt: bool) -> Result<(), Error> {
    let service = oo7::dbus::Service::new()
        .await
        .map_err(keyring_unavailable)?;
    let collection = service
        .default_collection()
        .await
        .map_err(keyring_unavailable)?;
    if prompt {
        unlock(&collection).await?;
    } else if collection.is_locked().await.map_err(keyring_unavailable)? {
        return Err(Error::Internal(
            "keyring locked; the refreshed session was not saved".into(),
        ));
    }
    let json = serde_json::to_string(s)
        .map_err(|_| Error::Internal("could not encode the session".into()))?;
    // replace = true: the item with the same attributes is overwritten, so there is
    // only ever one ytmfast session in the keyring.
    collection
        .create_item(
            KEYRING_LABEL,
            &KEYRING_ATTRIBUTES,
            oo7::Secret::text(json),
            true,
            None,
        )
        .await
        .map_err(keyring_unavailable)?;
    Ok(())
}

/// Chromium's libsecret schema for its "Safe Storage" password
/// (`components/os_crypt/sync/key_storage_libsecret.cc`, `kKeystoreSchemaV2`). libsecret
/// stores the schema's name as the `xdg:schema` attribute; the schema's one attribute is
/// `application`.
const SAFE_STORAGE_SCHEMA: &str = "chrome_libsecret_os_crypt_password_v2";

/// A Chromium browser's cookie password in the Secret Service: the `v11` key's source.
///
/// Found as Chromium finds it, by its attributes, in every collection. Only when nothing has
/// them is it looked up by its label, which also finds an item an older Chromium saved
/// without them. Every item found is returned, as the cookies decide which one is right.
pub struct SafeStorageKeys {
    /// The `application` attribute: Chromium's `--password-store` app name.
    application: &'static str,
    /// The item's label (`KeyStorageLinux::kKey`'s value for this browser).
    label: &'static str,
}

impl SafeStorageKeys {
    /// Brave's (Brave Origin shares it): `application` = `brave`, labelled
    /// "Brave Safe Storage".
    pub fn brave() -> Self {
        SafeStorageKeys {
            application: "brave",
            label: "Brave Safe Storage",
        }
    }

    async fn find(&self) -> Result<Vec<oo7::Secret>, Error> {
        let service = oo7::dbus::Service::new()
            .await
            .map_err(keyring_unavailable)?;
        let collections = service.collections().await.map_err(keyring_unavailable)?;
        let attributes = [
            ("xdg:schema", SAFE_STORAGE_SCHEMA),
            ("application", self.application),
        ];
        let mut items = Vec::new();
        for c in &collections {
            items.extend(
                c.search_items(&attributes)
                    .await
                    .map_err(keyring_unavailable)?,
            );
        }
        if items.is_empty() {
            for c in &collections {
                // A locked collection's labels can't be read: unlocking it is what Chromium's
                // own lookup (`SECRET_SEARCH_UNLOCK`) would ask for too.
                unlock(c).await?;
                for item in c.items().await.map_err(keyring_unavailable)? {
                    if item.label().await.map_err(keyring_unavailable)? == self.label {
                        items.push(item);
                    }
                }
            }
        }
        let mut secrets = Vec::with_capacity(items.len());
        for item in &items {
            if item.is_locked().await.map_err(keyring_unavailable)? {
                item.unlock(None).await.map_err(keyring_unavailable)?;
            }
            secrets.push(item.secret().await.map_err(keyring_unavailable)?);
        }
        Ok(secrets)
    }
}

impl chromium::KeySource for SafeStorageKeys {
    /// Runs the lookup on a runtime of its own: the import is plain blocking code, run before
    /// `main` starts any runtime (one can't be started inside another).
    fn passwords(&self) -> Result<Vec<oo7::Secret>, Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Internal("could not start the async runtime".into()))?;
        runtime.block_on(self.find())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(domain: &str, name: &str, value: &str, path: &str, secure: bool) -> Cookie {
        Cookie {
            domain: domain.into(),
            name: name.into(),
            value: value.into(),
            path: path.into(),
            secure,
            expires_utc: None,
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    const NOW: i64 = 1_800_000_000;

    #[test]
    fn debug_redacts_cookie_values() {
        let session = Session {
            cookies: vec![cookie(".youtube.com", "SAPISID", "s3cr3t-value", "/", true)],
        };
        let shown = format!("{session:?}");
        assert!(!shown.contains("s3cr3t-value"), "{shown}");
        assert!(shown.contains("SAPISID"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn cookie_header_matches_domain_path_secure_and_expiry() {
        let mut expired = cookie(".youtube.com", "OLD", "o", "/", false);
        expired.expires_utc = Some(NOW - 1);
        let mut later = cookie(".youtube.com", "LATER", "l", "/", false);
        later.expires_utc = Some(NOW + 60);
        let session = Session {
            cookies: vec![
                cookie(".youtube.com", "A", "1", "/", true),
                cookie("music.youtube.com", "HOSTONLY", "2", "/", true),
                cookie("www.youtube.com", "OTHERHOST", "3", "/", true),
                cookie(".google.com", "G", "4", "/", true),
                cookie(".youtube.com", "DEEP", "5", "/youtubei", true),
                cookie(".youtube.com", "WRONGPATH", "6", "/youtubeiv", true),
                expired,
                later,
            ],
        };
        let h = session.cookie_header_at(&url("https://music.youtube.com/youtubei/v1/player"), NOW);
        // Longer paths first (RFC 6265 5.4), then stored order.
        assert_eq!(h, "DEEP=5; A=1; HOSTONLY=2; LATER=l");
        // `Secure` cookies never go over plain http.
        let h = session.cookie_header_at(&url("http://music.youtube.com/"), NOW);
        assert_eq!(h, "LATER=l");
        // A lookalike domain gets nothing.
        let h = session.cookie_header_at(&url("https://evilyoutube.com/"), NOW);
        assert_eq!(h, "");
    }

    #[test]
    fn set_cookie_rotation_updates_value() {
        let mut session = Session {
            cookies: vec![cookie(".youtube.com", "__Secure-3PSIDTS", "old", "/", true)],
        };
        let u = url("https://music.youtube.com/youtubei/v1/player");
        let h = "__Secure-3PSIDTS=new; Domain=.youtube.com; Path=/; \
                 Expires=Mon, 04-Oct-2027 10:00:00 GMT; Secure; HttpOnly; Priority=HIGH; SameSite=none";
        assert!(session.apply_set_cookie_at(&u, h, NOW));
        assert_eq!(session.cookies.len(), 1);
        assert_eq!(session.cookies[0].value, "new");
        assert_eq!(session.cookies[0].expires_utc, Some(1_822_644_000));
        assert_eq!(session.cookie_header_at(&u, NOW), "__Secure-3PSIDTS=new");
        // The same header again changes nothing, so the caller doesn't save again.
        assert!(!session.apply_set_cookie_at(&u, h, NOW));
    }

    #[test]
    fn set_cookie_expiry_creep_is_not_a_change() {
        // Max-Age moves the expiry forward on every answer; saving to the keyring on every
        // request for that alone would be wasteful.
        let mut session = Session::default();
        let u = url("https://www.youtube.com/");
        assert!(session.apply_set_cookie_at(&u, "A=1; Max-Age=86400; Domain=youtube.com", NOW));
        assert_eq!(session.cookies[0].domain, ".youtube.com");
        assert_eq!(session.cookies[0].expires_utc, Some(NOW + 86400));
        assert!(!session.apply_set_cookie_at(
            &u,
            "A=1; Max-Age=86400; Domain=youtube.com",
            NOW + 60
        ));
        // But the new expiry is still kept in memory.
        assert_eq!(session.cookies[0].expires_utc, Some(NOW + 60 + 86400));
        // A big jump (over a day) is worth saving.
        assert!(session.apply_set_cookie_at(&u, "A=1; Max-Age=999999; Domain=youtube.com", NOW));
    }

    #[test]
    fn set_cookie_without_domain_is_host_only() {
        let mut session = Session::default();
        let u = url("https://music.youtube.com/youtubei/v1/player");
        assert!(session.apply_set_cookie_at(&u, "H=1", NOW));
        let c = &session.cookies[0];
        assert_eq!(c.domain, "music.youtube.com");
        // Default path is the request path's folder (RFC 6265 5.1.4).
        assert_eq!(c.path, "/youtubei/v1");
        assert_eq!(c.expires_utc, None);
        assert!(!c.secure);
    }

    #[test]
    fn set_cookie_rejects_foreign_or_bad_input() {
        let mut session = Session::default();
        let u = url("https://music.youtube.com/");
        assert!(!session.apply_set_cookie_at(&u, "X=1; Domain=example.com", NOW));
        assert!(!session.apply_set_cookie_at(&u, "X=1; Domain=evilyoutube.com", NOW));
        // A bare top-level domain would be sent to every .com host.
        assert!(!session.apply_set_cookie_at(&u, "X=1; Domain=com", NOW));
        assert!(!session.apply_set_cookie_at(&u, "novalue", NOW));
        assert!(!session.apply_set_cookie_at(&u, "=nameless", NOW));
        // Prefixed names must keep their promises (RFC 6265bis 4.1.3).
        assert!(!session.apply_set_cookie_at(&u, "__Secure-X=1; Path=/", NOW));
        assert!(!session.apply_set_cookie_at(
            &u,
            "__Host-X=1; Secure; Path=/; Domain=youtube.com",
            NOW
        ));
        assert!(session.cookies.is_empty());
        assert!(session.apply_set_cookie_at(&u, "__Host-X=1; Secure; Path=/", NOW));
    }

    #[test]
    fn set_cookie_in_the_past_deletes() {
        let mut session = Session {
            cookies: vec![
                cookie(".youtube.com", "GONE", "x", "/", true),
                cookie(".youtube.com", "KEPT", "y", "/", true),
            ],
        };
        let u = url("https://www.youtube.com/");
        assert!(session.apply_set_cookie_at(
            &u,
            "GONE=; Domain=.youtube.com; Path=/; Max-Age=0",
            NOW
        ));
        assert_eq!(session.cookies.len(), 1);
        assert_eq!(session.cookies[0].name, "KEPT");
        // Deleting what isn't there is not a change.
        assert!(!session.apply_set_cookie_at(
            &u,
            "GONE=; Domain=.youtube.com; Path=/; Expires=Thu, 01 Jan 1970 00:00:00 GMT",
            NOW
        ));
    }

    #[test]
    fn cookie_dates_parse() {
        assert_eq!(parse_cookie_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_cookie_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_cookie_date("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_cookie_date("Sun Nov  6 08:49:37 1994"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_cookie_date("Mon, 04-Oct-2027 10:00:00 GMT"),
            Some(1_822_644_000)
        );
        assert_eq!(
            parse_cookie_date("Tue, 29 Feb 2028 23:59:59 GMT"),
            Some(1_835_481_599)
        );
        assert_eq!(parse_cookie_date("not a date"), None);
        assert_eq!(parse_cookie_date("Mon, 32 Oct 2027 10:00:00 GMT"), None);
        assert_eq!(parse_cookie_date("Mon, 04 Oct 2027 25:00:00 GMT"), None);
    }

    #[test]
    fn session_json_roundtrip() {
        let mut c = cookie(".youtube.com", "SAPISID", "v", "/", true);
        c.expires_utc = Some(NOW);
        let s = Session { cookies: vec![c] };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
    }

    #[tokio::test]
    async fn memory_store_roundtrip() {
        let store = MemoryStore::new();
        let s = Session {
            cookies: vec![cookie(".youtube.com", "SAPISID", "v", "/", true)],
        };
        store.save(&s).await.unwrap();
        assert_eq!(store.load().await.unwrap(), s);
        // Works as the trait object the engine holds (ruling R1).
        let dyn_store: std::sync::Arc<dyn SessionStore> = std::sync::Arc::new(store);
        assert_eq!(dyn_store.load().await.unwrap(), s);
    }

    #[tokio::test]
    async fn save_without_prompt_defaults_to_save() {
        // A store with no prompts (the memory one) saves as usual on the no-prompt path.
        let store = MemoryStore::new();
        let s = Session {
            cookies: vec![cookie(".youtube.com", "SAPISID", "v", "/", true)],
        };
        store.save_without_prompt(&s).await.unwrap();
        assert_eq!(store.load().await.unwrap(), s);
    }

    #[tokio::test]
    async fn missing_item_is_signed_out() {
        assert_eq!(MemoryStore::new().load().await, Err(Error::SignedOut));
    }
}
