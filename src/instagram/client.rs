//! HTTP client for `https://www.instagram.com/api/v1/…`.
//!
//! Every request carries the web app's headers, the cookie jar (only ever sent to
//! the API base) and `X-CSRFToken`; `Set-Cookie` responses update the in-memory
//! jar. Requests are serialised and spaced by `api_delay` plus jitter. Responses
//! are classified into [`Error`] variants (see AGENTS.md "Error signals"), with
//! exponential backoff for rate limits, 5xx and network failures.

use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::time::Instant;

use crate::cookies::CookieJar;
use crate::error::{Error, Result};
use crate::instagram::models::{self, ProfileUser, UserInfo, WebProfileInfo};
use crate::redact;

/// Production API base; tests point the client at a mock server instead.
pub const DEFAULT_API_BASE: &str = "https://www.instagram.com";

/// Desktop Chrome user agent sent when none is configured.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// `X-IG-App-ID` of the Instagram web app.
pub const IG_APP_ID: &str = "936619743392459";

/// `X-ASBD-ID` sent by the Instagram web app.
pub const ASBD_ID: &str = "129477";

/// Error text for missing or expired cookies.
pub const NOT_LOGGED_IN: &str =
    "not logged in: cookies missing or expired — re-export them from your browser";

/// Error text for a login Instagram wants confirmed (checkpoint / challenge).
pub const CHECKPOINT: &str = "Instagram wants you to confirm this login: open instagram.com in the browser the cookies came from, then re-export";

/// Longest server text (in characters) copied into an error message.
const MAX_SERVER_TEXT: usize = 200;

/// Upper bound of the backoff base for 5xx / network failures.
const MAX_TRANSIENT_WAIT: Duration = Duration::from_secs(5);

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
    http: reqwest::Client,
    config: ClientConfig,
    /// `api_base` without a trailing slash.
    base: String,
    /// Never held across an `.await`.
    jar: Mutex<CookieJar>,
    /// When the previous request was sent; held while waiting so requests are spaced.
    last_request: tokio::sync::Mutex<Option<Instant>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("api_base", &self.base)
            .field("cookies", &*self.lock_jar())
            .finish_non_exhaustive()
    }
}

/// The body of a request.
enum Payload<'a> {
    Get,
    Form(&'a str),
}

/// How one attempt failed.
enum Failure {
    /// Retry after the rate-limit backoff; the message is already redacted.
    RateLimited(String),
    /// 5xx or network: retry after a short backoff.
    Transient(Error),
    /// Do not retry.
    Fatal(Error),
}

impl Client {
    /// Builds a client; `cookies` may be `None` for anonymous access (most
    /// endpoints will then fail with [`crate::error::Error::Auth`]).
    pub fn new(config: ClientConfig, cookies: Option<CookieJar>) -> Result<Self> {
        let base = config.api_base.trim_end_matches('/').to_string();
        let parsed = url::Url::parse(&base)
            .map_err(|err| Error::Config(format!("invalid API base '{base}': {err}")))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(Error::Config(format!(
                "invalid API base '{base}': expected http(s)://host"
            )));
        }
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| Error::Config("the user agent contains invalid characters".into()))?;
        let http = reqwest::Client::builder()
            .user_agent(user_agent)
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .gzip(true)
            .build()
            .map_err(|err| Error::Config(format!("cannot build the HTTP client: {err}")))?;
        Ok(Client {
            http,
            config,
            base,
            jar: Mutex::new(cookies.unwrap_or_default()),
            last_request: tokio::sync::Mutex::new(None),
        })
    }

    /// Whether a `sessionid` cookie is present.
    pub fn is_authenticated(&self) -> bool {
        self.lock_jar()
            .get("sessionid")
            .is_some_and(|value| !value.is_empty())
    }

    /// The settings this client was built with.
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// The logged-in user's id (`ds_user_id` cookie) when it is numeric.
    pub(crate) fn viewer_id(&self) -> Option<String> {
        self.lock_jar()
            .get("ds_user_id")
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string)
    }

    /// GETs `path_and_query` (e.g. `/api/v1/media/123/info/`) relative to the API
    /// base, with throttling, rate-limit backoff and error classification.
    pub async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value> {
        self.request_json(path_and_query, Payload::Get).await
    }

    /// POSTs an `application/x-www-form-urlencoded` body (with `X-CSRFToken`), with
    /// the same throttling and error handling as [`Client::get_json`]. Used by
    /// endpoints such as `/api/v1/clips/user/`.
    pub async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter())
            .finish();
        self.request_json(path, Payload::Form(&body)).await
    }

    /// The account the cookies belong to (via `ds_user_id`).
    pub async fn current_user(&self) -> Result<UserSummary> {
        if !self.is_authenticated() {
            return Err(Error::Auth(NOT_LOGGED_IN.into()));
        }
        let (path, fallback_id) = match self.viewer_id() {
            Some(id) => (format!("/api/v1/users/{id}/info/"), Some(id)),
            None => ("/api/v1/accounts/current_user/?edit=true".to_string(), None),
        };
        let value = self.get_json(&path).await?;
        let info: UserInfo = models::parse(value, "user info")?;
        let user = info.user.ok_or_else(|| {
            Error::Parse("the user info response has no user (are the cookies valid?)".into())
        })?;
        summary(user, fallback_id.as_deref(), "")
            .ok_or_else(|| Error::Parse("the user info response has no user id".into()))
    }

    /// Looks a profile up by username.
    pub async fn user_by_username(&self, username: &str) -> Result<UserSummary> {
        let username = username.trim().trim_start_matches('@');
        let not_found = || Error::NotFound(format!("user @{username}"));
        if username.is_empty() {
            return Err(not_found());
        }
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("username", username)
            .finish();
        let value = match self
            .get_json(&format!("/api/v1/users/web_profile_info/?{query}"))
            .await
        {
            Err(Error::NotFound(_)) => return Err(not_found()),
            other => other?,
        };
        let info: WebProfileInfo = models::parse(value, "profile")?;
        let user = info.data.user.ok_or_else(not_found)?;
        summary(user, None, username).ok_or_else(not_found)
    }

    fn lock_jar(&self) -> MutexGuard<'_, CookieJar> {
        // A poisoned jar is still a valid jar: every mutation is a single call.
        self.jar
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `text` redacted against the jar and truncated for an error message.
    fn server_text(&self, text: &str) -> String {
        let redacted = redact::redact_with(text.trim(), &self.lock_jar());
        models::truncate(&redacted, MAX_SERVER_TEXT)
    }

    async fn request_json(&self, path_and_query: &str, payload: Payload<'_>) -> Result<Value> {
        if !path_and_query.starts_with('/') || path_and_query.starts_with("//") {
            // Only paths: cookies must never be sent anywhere but the API base.
            return Err(Error::Config(format!(
                "API path must start with '/': {}",
                redact::redact_url(path_and_query)
            )));
        }
        let url = format!("{}{}", self.base, path_and_query);
        let mut rate_attempt: u32 = 0;
        let mut transient_attempt: u32 = 0;
        loop {
            self.throttle().await;
            let failure = match self.attempt(&url, path_and_query, &payload).await {
                Ok(value) => return Ok(value),
                Err(failure) => failure,
            };
            match failure {
                Failure::Fatal(err) => return Err(err),
                Failure::RateLimited(message) => {
                    if rate_attempt >= self.config.rate_limit_retries {
                        return Err(Error::RateLimited(format!(
                            "{message} (gave up after {rate_attempt} retries; try again later or raise the API delay)"
                        )));
                    }
                    let wait = backoff(self.config.rate_limit_wait, rate_attempt);
                    tracing::warn!(
                        wait_secs = wait.as_secs_f64(),
                        attempt = rate_attempt + 1,
                        "rate limited by Instagram; waiting {:.0?} before retrying",
                        wait
                    );
                    rate_attempt += 1;
                    tokio::time::sleep(wait).await;
                }
                Failure::Transient(err) => {
                    if transient_attempt >= self.config.rate_limit_retries {
                        return Err(err);
                    }
                    let base = self.config.rate_limit_wait.min(MAX_TRANSIENT_WAIT);
                    let wait = backoff(base, transient_attempt);
                    tracing::warn!(
                        error = %err,
                        attempt = transient_attempt + 1,
                        "API request failed; retrying in {:.1?}",
                        wait
                    );
                    transient_attempt += 1;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Waits until `api_delay` (+ jitter) has passed since the previous request.
    async fn throttle(&self) {
        let mut last = self.last_request.lock().await;
        let delay = self.config.api_delay;
        if let Some(previous) = *last
            && !delay.is_zero()
        {
            let jitter = delay.mul_f64(rand::random_range(0.0..=0.5));
            tokio::time::sleep_until(previous + delay + jitter).await;
        }
        *last = Some(Instant::now());
    }

    /// Headers for one request: the web app's, plus cookies and CSRF token.
    fn request_headers(&self, payload: &Payload<'_>) -> std::result::Result<HeaderMap, Failure> {
        let mut headers = HeaderMap::new();
        let fixed: [(&str, &str); 4] = [
            ("x-ig-app-id", IG_APP_ID),
            ("x-asbd-id", ASBD_ID),
            ("x-ig-www-claim", "0"),
            ("x-requested-with", "XMLHttpRequest"),
        ];
        for (name, value) in fixed {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        headers.insert(header::ACCEPT, HeaderValue::from_static("*/*"));
        let referer = format!("{}/", self.base);
        if let Ok(value) = HeaderValue::from_str(&referer) {
            headers.insert(header::REFERER, value);
        }
        if let Payload::Form(_) = payload {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
            if let Ok(value) = HeaderValue::from_str(&self.base) {
                headers.insert(header::ORIGIN, value);
            }
        }
        let (cookie, csrf) = {
            let jar = self.lock_jar();
            (jar.header_value(), jar.get("csrftoken").map(str::to_string))
        };
        if let Some(cookie) = cookie {
            let mut value = HeaderValue::from_str(&cookie).map_err(|_| {
                Failure::Fatal(Error::Cookies(
                    "a cookie value contains characters not allowed in an HTTP header".into(),
                ))
            })?;
            value.set_sensitive(true);
            headers.insert(header::COOKIE, value);
        }
        if let Some(csrf) = csrf.filter(|token| !token.is_empty()) {
            let mut value = HeaderValue::from_str(&csrf).map_err(|_| {
                Failure::Fatal(Error::Cookies(
                    "the csrftoken cookie contains characters not allowed in an HTTP header".into(),
                ))
            })?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("x-csrftoken"), value);
        }
        Ok(headers)
    }

    /// One request + classification.
    async fn attempt(
        &self,
        url: &str,
        path: &str,
        payload: &Payload<'_>,
    ) -> std::result::Result<Value, Failure> {
        let headers = self.request_headers(payload)?;
        let request = match payload {
            Payload::Get => self.http.request(Method::GET, url),
            Payload::Form(body) => self.http.request(Method::POST, url).body(body.to_string()),
        };
        tracing::debug!(
            method = if matches!(payload, Payload::Get) { "GET" } else { "POST" },
            path = %redact::redact_url(path),
            "API request"
        );
        let response = request
            .headers(headers)
            .send()
            .await
            .map_err(|err| Failure::Transient(Error::from(err)))?;
        let status = response.status();
        self.apply_set_cookies(response.headers());
        let location = response
            .headers()
            .get(header::LOCATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|err| Failure::Transient(Error::from(err)))?;
        self.classify(status, location.as_deref(), &content_type, &body, path)
    }

    fn apply_set_cookies(&self, headers: &HeaderMap) {
        let values: Vec<&str> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .collect();
        if values.is_empty() {
            return;
        }
        let mut jar = self.lock_jar();
        for value in values {
            jar.apply_set_cookie(value);
        }
    }

    /// Maps a response to a JSON value or a [`Failure`].
    fn classify(
        &self,
        status: StatusCode,
        location: Option<&str>,
        content_type: &str,
        body: &str,
        path: &str,
    ) -> std::result::Result<Value, Failure> {
        let code = status.as_u16();
        if status.is_redirection() {
            let target = location.unwrap_or_default();
            return Err(Failure::Fatal(if target.contains("/accounts/login") {
                Error::Auth(NOT_LOGGED_IN.into())
            } else if target.contains("/challenge") || target.contains("/checkpoint") {
                Error::Auth(CHECKPOINT.into())
            } else {
                Error::Http {
                    status: code,
                    message: format!(
                        "unexpected redirect to {}",
                        self.server_text(&redact::redact_url(target))
                    ),
                }
            }));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Failure::RateLimited(format!(
                "HTTP 429 on {}",
                short_path(path)
            )));
        }
        let json: Option<Value> = serde_json::from_str(body).ok().filter(Value::is_object);
        if let Some(json) = &json
            && !(status.is_success() && json.get("status").and_then(Value::as_str) == Some("ok"))
            && let Some(failure) = self.classify_json_signal(json)
        {
            return Err(failure);
        }
        if status.is_server_error() {
            return Err(Failure::Transient(Error::Http {
                status: code,
                message: self.error_message(json.as_ref(), body, "Instagram server error"),
            }));
        }
        if json.is_none() && looks_like_login_page(body) {
            return Err(Failure::Fatal(Error::Auth(NOT_LOGGED_IN.into())));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(Failure::Fatal(Error::NotFound(short_path(path))));
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            let detail = json
                .as_ref()
                .and_then(json_message)
                .map(|message| format!(" ({})", self.server_text(&message)))
                .unwrap_or_default();
            return Err(Failure::Fatal(Error::Auth(format!(
                "Instagram refused the request (HTTP {code}){detail}; the cookies may be expired — re-export them from your browser"
            ))));
        }
        if !status.is_success() {
            return Err(Failure::Fatal(Error::Http {
                status: code,
                message: self.error_message(json.as_ref(), body, "request failed"),
            }));
        }
        let Some(json) = json else {
            let kind = if content_type.is_empty() {
                "unknown content type".to_string()
            } else {
                self.server_text(content_type)
            };
            return Err(Failure::Fatal(Error::Parse(format!(
                "expected JSON from {} but got a non-JSON response ({kind}, {} bytes)",
                short_path(path),
                body.len()
            ))));
        };
        if json.get("status").and_then(Value::as_str) == Some("fail") {
            return Err(Failure::Fatal(Error::Http {
                status: code,
                message: self.error_message(Some(&json), body, "Instagram reported a failure"),
            }));
        }
        Ok(json)
    }

    /// Login / checkpoint / rate-limit signals inside a JSON body (any status).
    fn classify_json_signal(&self, json: &Value) -> Option<Failure> {
        let message = json_message(json).unwrap_or_default().to_ascii_lowercase();
        let error_type = json
            .get("error_type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let has = |needle: &str| message.contains(needle) || error_type.contains(needle);
        if has("checkpoint_required") || has("challenge_required") {
            return Some(Failure::Fatal(Error::Auth(CHECKPOINT.into())));
        }
        if has("login_required") || json.get("require_login") == Some(&Value::Bool(true)) {
            return Some(Failure::Fatal(Error::Auth(NOT_LOGGED_IN.into())));
        }
        if message.contains("please wait a few minutes") {
            return Some(Failure::RateLimited(format!(
                "Instagram asked to wait: {}",
                self.server_text(json_message(json).as_deref().unwrap_or_default())
            )));
        }
        None
    }

    /// The server's message (JSON `message`, else a body snippet when it is
    /// short text), redacted, or `fallback`.
    fn error_message(&self, json: Option<&Value>, body: &str, fallback: &str) -> String {
        if let Some(message) = json.and_then(json_message) {
            return self.server_text(&message);
        }
        if json.is_none() && !body.trim().is_empty() && !body.contains('<') {
            return self.server_text(body);
        }
        fallback.to_string()
    }
}

/// `base * 2^attempt`, saturating.
fn backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(2u32.saturating_pow(attempt))
}

/// The path without its query (for messages).
fn short_path(path: &str) -> String {
    path.split('?').next().unwrap_or_default().to_string()
}

/// `message` of an error JSON (string, or `{ "message": … }`-like objects).
fn json_message(json: &Value) -> Option<String> {
    match json.get("message")? {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Whether an HTML body is Instagram's login page.
fn looks_like_login_page(body: &str) -> bool {
    body.contains("/accounts/login") || body.contains("loginForm")
}

/// Builds a [`UserSummary`]; `None` without an id.
fn summary(
    user: ProfileUser,
    fallback_id: Option<&str>,
    fallback_name: &str,
) -> Option<UserSummary> {
    let id = user
        .id
        .or(user.pk)
        .or_else(|| fallback_id.map(str::to_string))?;
    let username = if user.username.is_empty() {
        fallback_name.to_string()
    } else {
        user.username
    };
    Some(UserSummary {
        id,
        username,
        full_name: user.full_name.filter(|name| !name.is_empty()),
        is_private: user.is_private,
        followed_by_viewer: user.followed_by_viewer,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles() {
        let base = Duration::from_secs(30);
        assert_eq!(backoff(base, 0), Duration::from_secs(30));
        assert_eq!(backoff(base, 2), Duration::from_secs(120));
        assert_eq!(backoff(Duration::ZERO, 5), Duration::ZERO);
    }

    #[test]
    fn rejects_bad_api_base_and_paths() {
        let config = ClientConfig {
            api_base: "ftp://x".into(),
            ..ClientConfig::default()
        };
        assert!(matches!(Client::new(config, None), Err(Error::Config(_))));
    }

    #[tokio::test]
    async fn absolute_urls_are_refused() {
        let client = Client::new(ClientConfig::default(), None).unwrap();
        let err = client.get_json("https://evil.test/x").await.unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        let err = client.get_json("//evil.test/x").await.unwrap_err();
        assert!(matches!(err, Error::Config(_)));
    }

    #[tokio::test]
    async fn current_user_without_session_makes_no_request() {
        let client = Client::new(
            ClientConfig {
                api_base: "http://127.0.0.1:9".into(),
                ..ClientConfig::default()
            },
            Some(CookieJar::from_pairs([("csrftoken", "x")])),
        )
        .unwrap();
        assert!(!client.is_authenticated());
        assert!(matches!(client.current_user().await, Err(Error::Auth(_))));
    }

    #[test]
    fn login_page_detection() {
        assert!(looks_like_login_page("<form id=\"loginForm\">"));
        assert!(!looks_like_login_page("<html>oops</html>"));
    }
}
