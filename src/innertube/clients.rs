//! The InnerTube client table: the one place to edit when YouTube changes a client.
//!
//! Values follow yt-dlp 2026.08.19 (`INNERTUBE_CLIENTS` in `yt_dlp/extractor/youtube/_base.py`).

/// One InnerTube client: what goes in `context.client` and the matching headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientInfo {
    /// `context.client.clientName`.
    pub name: &'static str,
    /// `context.client.clientVersion` and the `X-YouTube-Client-Version` header.
    pub version: &'static str,
    /// The `X-YouTube-Client-Name` header: the client's number, not its name.
    pub name_id: u32,
    /// The `User-Agent` header, and `context.client.userAgent`.
    pub user_agent: &'static str,
    /// The `Origin` and `X-Origin` headers, and the origin the SAPISIDHASH is signed for.
    pub origin: &'static str,
    /// The host the API requests go to.
    pub api_host: &'static str,
    /// Whether requests carry `X-Goog-AuthUser: 0`. The music web client sends it with a
    /// signed-in session; the TV client never has, and step 1's working TV requests are kept
    /// exactly as they were.
    pub sends_auth_user: bool,
    /// `context.client.deviceMake`, `deviceModel`, `osName` and `osVersion`, for a client
    /// that names its device (the TV client, below); `None` sends none of them.
    pub device: Option<Device>,
}

/// The device a client says it runs on (see `ClientInfo::device`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub make: &'static str,
    pub model: &'static str,
    pub os_name: &'static str,
    pub os_version: &'static str,
}

/// The TV client, used for `player`: with the session it gave the Premium formats (itag 774)
/// in the feasibility check. The `5.x` version makes YouTube serve the classic TV player
/// answer. It names a Samsung TV (yt-dlp PR #17723's `tv_samsung` entry, not merged yet):
/// from 2026-10-07 this account got "The page needs to be reloaded." (UNPLAYABLE) for
/// yt-dlp's `tv_downgraded` entry, which sends the bare `Cobalt/Version` user agent and no
/// device (yt-dlp issue #17389, open since 2026-08), while the same request naming the TV
/// gets the Premium formats again (checked live on two songs).
pub const TV: ClientInfo = ClientInfo {
    name: "TVHTML5",
    version: "5.20260707",
    name_id: 7,
    user_agent: "Mozilla/5.0 (SMART-TV; Linux; Tizen 2.4.0) AppleWebKit/538.1 (KHTML, like Gecko) Version/2.4.0 TV Safari/538.1",
    origin: "https://www.youtube.com",
    api_host: "www.youtube.com",
    sends_auth_user: false,
    device: Some(Device {
        make: "Samsung",
        model: "UKS9800",
        os_name: "Tizen",
        os_version: "2.4.0",
    }),
};

/// The YouTube Music web client, for `next` (the queue) and the browse/search requests of later
/// steps. yt-dlp sends a desktop Chrome user agent with it; this is one from its current range.
pub const WEB_REMIX: ClientInfo = ClientInfo {
    name: "WEB_REMIX",
    version: "1.20260707.12.00",
    name_id: 67,
    user_agent: "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/148.0.0.0 Safari/537.36",
    origin: "https://music.youtube.com",
    api_host: "music.youtube.com",
    sends_auth_user: true,
    device: None,
};

/// Every client, so tests can check each one against the host allowlist.
pub const ALL: &[ClientInfo] = &[TV, WEB_REMIX];
