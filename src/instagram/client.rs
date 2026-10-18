//! HTTP client for `https://www.instagram.com/api/v1/…`.

use std::time::Duration;

use crate::cookies::CookieJar;
use crate::error::Result;

/// Production API base; tests point the client at a mock server instead.
pub const DEFAULT_API_BASE: &str = "https://www.instagram.com";

/// Desktop Chrome user agent sent when none is configured.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Scheme + host (+ optional port), no trailing slash. Cookies are sent only here.
    pub api_base: String,
    pub user_agent: String,
    /// Minimum pause between consecutive API requests (a random 0–50 % jitter is added).
    pub api_delay: Duration,
    /// How many times a rate-limited request is retried before giving up.
    pub rate_limit_retries: u32,
    /// First backoff after a rate limit; doubles on every further attempt.
    pub rate_limit_wait: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_base: DEFAULT_API_BASE.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            api_delay: Duration::from_millis(1500),
            rate_limit_retries: 3,
            rate_limit_wait: Duration::from_secs(30),
            timeout: Duration::from_secs(30),
        }
    }
}

/// The parts of a user profile `igdl` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    pub id: String,
    pub username: String,
    pub full_name: Option<String>,
    pub is_private: bool,
    pub followed_by_viewer: bool,
}

/// Authenticated, throttled Instagram API client. Cheap to share behind an `Arc`.
pub struct Client {
    _private: (),
}

impl Client {
    /// Builds a client; `cookies` may be `None` for anonymous access (most
    /// endpoints will then fail with [`crate::error::Error::Auth`]).
    pub fn new(config: ClientConfig, cookies: Option<CookieJar>) -> Result<Self> {
        let _ = (config, cookies);
        todo!("instagram agent")
    }

    /// Whether a `sessionid` cookie is present.
    pub fn is_authenticated(&self) -> bool {
        todo!("instagram agent")
    }

    /// GETs `path_and_query` (e.g. `/api/v1/media/123/info/`) relative to the API
    /// base, with throttling, rate-limit backoff and error classification.
    pub async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value> {
        let _ = path_and_query;
        todo!("instagram agent")
    }

    /// POSTs an `application/x-www-form-urlencoded` body (with `X-CSRFToken`), with
    /// the same throttling and error handling as [`Client::get_json`]. Used by
    /// endpoints such as `/api/v1/clips/user/`.
    pub async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
        let _ = (path, form);
        todo!("instagram agent")
    }

    /// The account the cookies belong to (via `ds_user_id`).
    pub async fn current_user(&self) -> Result<UserSummary> {
        todo!("instagram agent")
    }

    /// Looks a profile up by username.
    pub async fn user_by_username(&self, username: &str) -> Result<UserSummary> {
        let _ = username;
        todo!("instagram agent")
    }
}
