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
}

/// The TV client, used for `player`: with the session it gave the Premium formats (itag 774)
/// in the feasibility check. This is yt-dlp's `tv_downgraded` entry: its `Cobalt/Version`
/// user agent and the `5.x` version make YouTube serve the classic TV player answer.
pub const TV: ClientInfo = ClientInfo {
    name: "TVHTML5",
    version: "5.20260707",
    name_id: 7,
    user_agent: "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version",
    origin: "https://www.youtube.com",
    api_host: "www.youtube.com",
    sends_auth_user: false,
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
};

/// Every client, so tests can check each one against the host allowlist.
pub const ALL: &[ClientInfo] = &[TV, WEB_REMIX];
