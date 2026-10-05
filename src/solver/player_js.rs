//! YouTube's player script (`base.js`): which version is current, where to get it, its
//! signature timestamp, and the on-disk cache of the last few versions.
//!
//! The challenge solver needs the player script of the version YouTube currently serves, and
//! the `player` request needs that script's signature timestamp (`sts`), because YouTube only
//! hands out ciphers that version can solve. A version's script never changes, so each one is
//! downloaded once and kept in `$XDG_CACHE_HOME/ytmfast/players/` with its preprocessed form
//! (the solver's output), for the last `KEEP_PLAYERS` versions.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use url::Url;

use crate::error::Error;
use crate::net;

/// Where the current player version is read from. A small public script (about 1 KB) that
/// names the player's path; no session goes with it.
pub const WEB_BASE: &str = "https://www.youtube.com";

/// How many player versions the cache keeps. YouTube serves one version at a time and moves
/// on every few days; three covers a roll-back or an A/B split without the folder growing
/// (each version is about 3 MB of script plus 4 MB preprocessed).
pub const KEEP_PLAYERS: usize = 3;

/// File suffix of the downloaded script, next to the solver's `{id}.js` preprocessed file.
pub const BASE_SUFFIX: &str = "base.js";
/// File suffix of the solver's preprocessed player.
pub const PREPROCESSED_SUFFIX: &str = "js";

/// The current player id, from `https://www.youtube.com/iframe_api`.
pub async fn current_player_id(client: &reqwest::Client) -> Result<String, Error> {
    let base = Url::parse(WEB_BASE).expect("WEB_BASE is a valid URL");
    current_player_id_at(client, &base).await
}

/// `current_player_id` against `base`: production passes `WEB_BASE`; tests inject a local
/// server (ruling R7: the constructor argument is the only way past the allowlist).
pub async fn current_player_id_at(client: &reqwest::Client, base: &Url) -> Result<String, Error> {
    let mut url = base.clone();
    url.set_path("/iframe_api");
    let body = get_text(client, url).await?;
    find_player_id(&body)
        .ok_or_else(|| Error::StreamFailed("could not find the player version".into()))
}

/// `https://www.youtube.com/s/player/{id}/player_ias.vflset/en_US/base.js`.
pub fn url(id: &str) -> String {
    format!("{WEB_BASE}/s/player/{id}/player_ias.vflset/en_US/base.js")
}

/// Downloads player `id`'s script from `base` (production: `WEB_BASE`).
pub async fn fetch_player(client: &reqwest::Client, base: &Url, id: &str) -> Result<String, Error> {
    if !valid_player_id(id) {
        return Err(Error::Internal("not a player id".into()));
    }
    let mut url = base.clone();
    url.set_path(&format!("/s/player/{id}/player_ias.vflset/en_US/base.js"));
    get_text(client, url).await
}

async fn get_text(client: &reqwest::Client, url: Url) -> Result<String, Error> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Network(format!(
            "YouTube answered HTTP {}",
            status.as_u16()
        )));
    }
    let body = net::read_capped(resp, net::MAX_ANSWER).await?;
    String::from_utf8(body).map_err(|_| Error::StreamFailed("the player script is not text".into()))
}

/// The id in the first `player\/XXXXXXXX\/` (or `player/XXXXXXXX/`) of `text`: yt-dlp's
/// `player\\?/([0-9a-fA-F]{8})\\?/`. Hand-matched: two short patterns don't justify the regex
/// crate in the binary.
pub fn find_player_id(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(at) = rest.find("player") {
        let after = &rest[at + "player".len()..];
        if let Some(id) = after
            .strip_prefix('\\')
            .unwrap_or(after)
            .strip_prefix('/')
            .and_then(|s| s.get(..8).map(|id| (id, &s[8..])))
            .filter(|(id, _)| valid_player_id(id))
            .and_then(|(id, tail)| {
                let tail = tail.strip_prefix('\\').unwrap_or(tail);
                tail.starts_with('/').then_some(id)
            })
        {
            return Some(id.to_string());
        }
        rest = after;
    }
    None
}

/// True for exactly 8 hex digits. Player ids end up in file names, so this is also what keeps
/// a crafted id from naming a path outside the cache folder.
pub fn valid_player_id(id: &str) -> bool {
    id.len() == 8 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The signature timestamp in a player script: yt-dlp's
/// `(?:signatureTimestamp|sts)\s*:\s*([0-9]{5})`.
pub fn sts(code: &str) -> Option<u32> {
    for key in ["signatureTimestamp", "sts"] {
        let mut rest = code;
        while let Some(at) = rest.find(key) {
            rest = &rest[at + key.len()..];
            let value = rest.trim_start_matches(is_regex_space);
            let Some(value) = value.strip_prefix(':') else {
                continue;
            };
            let value = value.trim_start_matches(is_regex_space);
            let digits = value.as_bytes().iter().take(5);
            if digits.len() == 5 && digits.clone().all(u8::is_ascii_digit) {
                // The regex has no boundary after the 5 digits, so neither does this.
                return value[..5].parse().ok();
            }
        }
    }
    None
}

/// `\s` in Python's `re` for str patterns: Unicode whitespace.
fn is_regex_space(c: char) -> bool {
    c.is_whitespace()
}

/// `{dir}/{id}.{suffix}`, or `None` for an id that isn't one.
pub fn cache_path(dir: &Path, id: &str, suffix: &str) -> Option<PathBuf> {
    valid_player_id(id).then(|| dir.join(format!("{id}.{suffix}")))
}

/// Reads a cached file and marks it as just used (its mtime), so pruning drops the least
/// recently used version, not just the oldest download.
pub fn load_cached(dir: &Path, id: &str, suffix: &str) -> Option<String> {
    let path = cache_path(dir, id, suffix)?;
    let text = fs::read_to_string(&path).ok()?;
    // Best effort: a failed touch only makes the pruning order a little less exact.
    if let Ok(f) = fs::File::options().write(true).open(&path) {
        let _ = f.set_modified(SystemTime::now());
    }
    Some(text)
}

/// When a cache file was last written or used (its mtime), without touching it.
pub fn modified(dir: &Path, id: &str, suffix: &str) -> Option<SystemTime> {
    let path = cache_path(dir, id, suffix)?;
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Deletes a cache file, if it is there.
pub fn remove_cached(dir: &Path, id: &str, suffix: &str) {
    if let Some(path) = cache_path(dir, id, suffix) {
        let _ = fs::remove_file(path);
    }
}

/// Writes a cache file (whole or not at all: written to a temp name, then renamed, so a
/// crash can't leave a cut-off script that would later fail to solve), then prunes the folder
/// to the newest `KEEP_PLAYERS` versions.
pub fn store_cached(dir: &Path, id: &str, suffix: &str, text: &str) -> io::Result<()> {
    let path = cache_path(dir, id, suffix)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a player id"))?;
    fs::create_dir_all(dir)?;
    // The pid keeps two ytmfast processes (the daemon and a `play`) from sharing a temp file.
    let tmp = dir.join(format!("{id}.{suffix}.tmp{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result?;
    prune(dir, KEEP_PLAYERS)
}

/// Keeps the `keep` most recently used player versions in `dir` and deletes every file of the
/// others. A version's age is the newest mtime among its files. Files whose name doesn't start
/// with a player id are left alone.
pub fn prune(dir: &Path, keep: usize) -> io::Result<()> {
    use std::collections::HashMap;

    let mut versions: HashMap<String, (SystemTime, Vec<PathBuf>)> = HashMap::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.split('.').next())
            .filter(|id| valid_player_id(id))
        else {
            continue;
        };
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let v = versions
            .entry(id.to_string())
            .or_insert((SystemTime::UNIX_EPOCH, Vec::new()));
        v.0 = v.0.max(mtime);
        v.1.push(entry.path());
    }
    let mut by_age: Vec<_> = versions.into_values().collect();
    // Newest first.
    by_age.sort_by_key(|v| std::cmp::Reverse(v.0));
    for (_, files) in by_age.into_iter().skip(keep) {
        for f in files {
            // Another process may have pruned it already.
            match fs::remove_file(&f) {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn player_id_from_iframe_api() {
        let js = r#"var scriptUrl = 'https:\/\/www.youtube.com\/s\/player\/8ab5c328\/www-widgetapi.vflset\/www-widgetapi.js';"#;
        assert_eq!(find_player_id(js).as_deref(), Some("8ab5c328"));
        assert_eq!(
            find_player_id("/s/player/ABCDEF01/x").as_deref(),
            Some("ABCDEF01")
        );
        // Not 8 hex digits, or no closing slash.
        assert_eq!(find_player_id(r"player\/8ab5c32\/"), None);
        assert_eq!(find_player_id(r"player\/8ab5c32g\/"), None);
        assert_eq!(find_player_id(r"player\/8ab5c3281\/"), None);
        assert_eq!(find_player_id("playerplayer/"), None);
        // A miss first, then a hit.
        assert_eq!(
            find_player_id("player/zz/ player/0123abcd/").as_deref(),
            Some("0123abcd")
        );
    }

    #[test]
    fn player_url_shape() {
        assert_eq!(
            url("8ab5c328"),
            "https://www.youtube.com/s/player/8ab5c328/player_ias.vflset/en_US/base.js"
        );
        assert!(net::allowed_host(&Url::parse(&url("8ab5c328")).unwrap()));
    }

    #[test]
    fn sts_from_player() {
        assert_eq!(sts("a,signatureTimestamp:20725,b"), Some(20725));
        assert_eq!(sts("x={sts : 19876}"), Some(19876));
        assert_eq!(sts("sts:1234,signatureTimestamp:20001"), Some(20001));
        assert_eq!(sts("sts:1234"), None);
        assert_eq!(sts("nothing here"), None);
    }

    #[test]
    fn ids_are_checked_before_naming_files() {
        let dir = Path::new("/nonexistent");
        assert!(cache_path(dir, "../../x", "js").is_none());
        assert!(cache_path(dir, "8ab5c328/", "js").is_none());
        assert_eq!(
            cache_path(dir, "8ab5c328", "js").unwrap(),
            dir.join("8ab5c328.js")
        );
    }

    #[test]
    fn store_then_load() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("players");
        store_cached(&dir, "0000000a", BASE_SUFFIX, "code").unwrap();
        assert_eq!(
            load_cached(&dir, "0000000a", BASE_SUFFIX).as_deref(),
            Some("code")
        );
        assert_eq!(load_cached(&dir, "0000000b", BASE_SUFFIX), None);
        // No temp file is left behind.
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("0000000a.base.js")]);
    }

    #[test]
    fn prune_drops_least_recently_used_version_with_all_its_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let base = SystemTime::now() - Duration::from_secs(1000);
        for (i, id) in ["0000000a", "0000000b", "0000000c", "0000000d"]
            .iter()
            .enumerate()
        {
            for suffix in [BASE_SUFFIX, PREPROCESSED_SUFFIX] {
                let p = dir.join(format!("{id}.{suffix}"));
                fs::write(&p, "x").unwrap();
                let f = fs::File::options().write(true).open(&p).unwrap();
                f.set_modified(base + Duration::from_secs(i as u64 * 10))
                    .unwrap();
            }
        }
        fs::write(dir.join("unrelated.txt"), "x").unwrap();
        prune(dir, 3).unwrap();
        assert!(!dir.join("0000000a.base.js").exists());
        assert!(!dir.join("0000000a.js").exists());
        for id in ["0000000b", "0000000c", "0000000d"] {
            assert!(dir.join(format!("{id}.js")).exists(), "{id}");
        }
        assert!(dir.join("unrelated.txt").exists());
    }
}
