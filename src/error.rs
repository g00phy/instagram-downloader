//! Error taxonomy shared by every module.
//!
//! Messages must never contain cookie values or full signed CDN URLs; build them
//! from already-redacted strings (see [`crate::redact`]).

use serde::{Deserialize, Serialize};

/// Result alias used throughout the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Coarse classification of an [`Error`], persisted in the queue for failed items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Auth,
    RateLimited,
    NotFound,
    Private,
    Unsupported,
    Network,
    Http,
    Parse,
    Cookies,
    Config,
    Io,
    Cancelled,
}

impl ErrorKind {
    /// Short machine-friendly name, e.g. `rate_limited`.
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::Auth => "auth",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::NotFound => "not_found",
            ErrorKind::Private => "private",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Network => "network",
            ErrorKind::Http => "http",
            ErrorKind::Parse => "parse",
            ErrorKind::Cookies => "cookies",
            ErrorKind::Config => "config",
            ErrorKind::Io => "io",
            ErrorKind::Cancelled => "cancelled",
        }
    }
}

impl std::fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every failure `igdl` can report.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Missing or expired cookies, `login_required`, `checkpoint_required`.
    #[error("authentication failed: {0}")]
    Auth(String),
    /// HTTP 429 or "please wait a few minutes" after all backoff attempts.
    #[error("rate limited by Instagram: {0}")]
    RateLimited(String),
    /// The post, user, story or highlight does not exist (or is no longer available).
    #[error("not found: {0}")]
    NotFound(String),
    /// A private account the logged-in user does not follow.
    #[error("private account: {0}")]
    Private(String),
    /// A link or input `igdl` cannot handle.
    #[error("unsupported link: {0}")]
    Unsupported(String),
    /// Connection, timeout, truncated body: worth retrying.
    #[error("network error: {0}")]
    Network(String),
    /// A non-success HTTP status not covered by a more specific variant.
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    /// The response did not have the expected shape.
    #[error("unexpected response: {0}")]
    Parse(String),
    /// The cookie file is unreadable, malformed, or has no Instagram cookies.
    #[error("cookies: {0}")]
    Cookies(String),
    /// Bad configuration, CLI value, template, or queue state.
    #[error("{0}")]
    Config(String),
    /// Filesystem failure with context (usually the path involved).
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: std::io::Error,
    },
    /// The operation was interrupted (Ctrl+C).
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    /// Wraps an I/O error with context such as the path being written.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// The [`ErrorKind`] of this error.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Auth(_) => ErrorKind::Auth,
            Error::RateLimited(_) => ErrorKind::RateLimited,
            Error::NotFound(_) => ErrorKind::NotFound,
            Error::Private(_) => ErrorKind::Private,
            Error::Unsupported(_) => ErrorKind::Unsupported,
            Error::Network(_) => ErrorKind::Network,
            Error::Http { .. } => ErrorKind::Http,
            Error::Parse(_) => ErrorKind::Parse,
            Error::Cookies(_) => ErrorKind::Cookies,
            Error::Config(_) => ErrorKind::Config,
            Error::Io { .. } => ErrorKind::Io,
            Error::Cancelled => ErrorKind::Cancelled,
        }
    }

    /// Whether the same request may succeed if repeated later
    /// (network failures, rate limiting, HTTP 429 and 5xx).
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Network(_) | Error::RateLimited(_) => true,
            Error::Http { status, .. } => *status == 429 || *status >= 500,
            _ => false,
        }
    }
}

impl From<reqwest::Error> for Error {
    fn from(err: reqwest::Error) -> Self {
        // Signed URLs must not leak into messages; drop the URL reqwest attaches.
        let status = err.status();
        let message = err.without_url().to_string();
        match status {
            Some(status) => Error::Http {
                status: status.as_u16(),
                message,
            },
            None => Error::Network(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retryable_classification() {
        assert!(Error::Network("reset".into()).is_retryable());
        assert!(
            Error::Http {
                status: 503,
                message: String::new()
            }
            .is_retryable()
        );
        assert!(
            !Error::Http {
                status: 404,
                message: String::new()
            }
            .is_retryable()
        );
        assert!(!Error::Auth("expired".into()).is_retryable());
    }

    #[test]
    fn kind_serializes_snake_case() {
        let json = serde_json::to_string(&ErrorKind::RateLimited).unwrap();
        assert_eq!(json, "\"rate_limited\"");
    }
}
