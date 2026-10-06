//! The one error type every module returns.
//!
//! Messages must never carry session values or signed stream URLs: callers put a short,
//! human description in the `String`, not a raw upstream error that may embed a URL.

/// Every failure ytmfast reports. `code()` is the stable, machine-readable name the
/// socket protocol and the bar widgets match on; the `Display` text is for people.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("signed out")]
    SignedOut,
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("stream failed: {0}")]
    StreamFailed(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// The sound server went away under the output (restarted, or crashed): the stream is
    /// gone for good, and the next `open` makes a new one. Its own variant, not `Internal`
    /// text, because the engine acts on it (it plays the song again once, where it was).
    #[error("internal error: the audio output restarted")]
    OutputRestarted,
    /// What the caller asked for can't be sent as written (an id or token of the wrong
    /// shape, an empty or overlong search). Checked before anything goes out, so the socket
    /// answers it as the client's mistake (`bad_request`), not YouTube's or ours. The text is
    /// fixed: it says what was wrong, never the value itself (ruling R6).
    #[error("bad request: {0}")]
    BadRequest(String),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::SignedOut => "signed_out",
            Error::Unavailable(_) => "unavailable",
            Error::Network(_) => "network",
            Error::StreamFailed(_) => "stream_failed",
            Error::Internal(_) | Error::OutputRestarted => "internal",
            Error::BadRequest(_) => "bad_request",
        }
    }
}

/// The one mapping from an HTTP client error to ours (ruling R6). Every message is fixed
/// text: reqwest's own `Display` names the request URL, and a signed stream link carries an
/// access token, so not even `without_url()` text is passed on (its source chain is hyper's
/// and rustls's wording, which is no help to the user anyway).
impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        let what = if e.is_timeout() {
            "timed out"
        } else if e.is_connect() {
            "could not connect"
        } else if e.is_redirect() {
            "redirect refused"
        } else if e.is_body() || e.is_decode() {
            "the connection dropped while reading the answer"
        } else if e.is_builder() {
            // A request we built badly is our bug, not the network's.
            return Error::Internal("could not build the request".into());
        } else {
            "the request failed"
        };
        Error::Network(what.into())
    }
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn error_codes() {
        assert_eq!(Error::SignedOut.code(), "signed_out");
        assert_eq!(Error::Unavailable("x".into()).code(), "unavailable");
        assert_eq!(Error::Network("x".into()).code(), "network");
        assert_eq!(Error::StreamFailed("x".into()).code(), "stream_failed");
        assert_eq!(Error::Internal("x".into()).code(), "internal");
        assert_eq!(Error::OutputRestarted.code(), "internal");
        // The same code the socket uses for a malformed request (`protocol::BAD_REQUEST`).
        assert_eq!(
            Error::BadRequest("x".into()).code(),
            crate::control::protocol::BAD_REQUEST
        );
    }

    #[test]
    fn display_is_human() {
        assert_eq!(Error::SignedOut.to_string(), "signed out");
        assert_eq!(
            Error::Network("timed out".into()).to_string(),
            "network error: timed out"
        );
    }

    #[tokio::test]
    async fn http_errors_never_carry_the_url() {
        // Port 1 on loopback refuses the connection at once; the URL holds a fake token.
        let e = crate::net::client("ytmfast-test/0")
            .get("http://127.0.0.1:1/videoplayback?sig=FAKETOKEN")
            .send()
            .await
            .unwrap_err();
        assert!(e.to_string().contains("FAKETOKEN"), "reqwest names the URL");
        let ours = Error::from(e);
        assert_eq!(ours, Error::Network("could not connect".into()));
        assert!(!format!("{ours} {ours:?}").contains("FAKETOKEN"));
    }
}
