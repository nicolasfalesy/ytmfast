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
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Error::SignedOut => "signed_out",
            Error::Unavailable(_) => "unavailable",
            Error::Network(_) => "network",
            Error::StreamFailed(_) => "stream_failed",
            Error::Internal(_) => "internal",
        }
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
    }

    #[test]
    fn display_is_human() {
        assert_eq!(Error::SignedOut.to_string(), "signed out");
        assert_eq!(
            Error::Network("timed out".into()).to_string(),
            "network error: timed out"
        );
    }
}
