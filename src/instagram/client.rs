//! HTTP client for Instagram's web app (`https://www.instagram.com`).
//!
//! Three kinds of requests leave this client:
//!
//! * **API calls** (`/api/v1/…` REST and the GraphQL endpoints): the web app's
//!   headers (`X-IG-App-ID`, `X-ASBD-ID`, `X-IG-WWW-Claim`, `X-CSRFToken`,
//!   `Origin`, `Referer`), the cookie jar, and pacing: `api_delay` plus a random
//!   0–100 % between two API calls (the first is not delayed).
//! * **Page fetches** (`/{username}/`): browser navigation headers and the
//!   cookies (same host as the API); exempt from `api_delay`.
//! * **Static JS bundles** (`static.cdninstagram.com/rsrc.php/…`, for doc id
//!   discovery): a separate HTTP client that never sends cookies; exempt from
//!   `api_delay`.
//!
//! An HTTP 429 sets a global pause ([`ClientConfig::rate_limit_wait`]) that every
//! request waits for, then the request is retried once; a second 429 is
//! [`Error::RateLimited`]. `Set-Cookie` and `x-ig-set-www-claim` response headers
//! update the jar and the claim. See AGENTS.md "Instagram web API notes".

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};
use reqwest::{Method, StatusCode};
use serde_json::Value;
use tokio::time::Instant;

use crate::cookies::CookieJar;
use crate::error::{Error, Result};
use crate::instagram::doc_ids::DocIdState;
use crate::instagram::models;
use crate::instagram::web::{self, PageInfo, Tokens};
use crate::redact;

/// Production API base; tests point the client at a mock server instead.
pub const DEFAULT_API_BASE: &str = "https://www.instagram.com";

/// Where the web app's JS bundles are served from (doc id discovery).
pub const DEFAULT_STATIC_BASE: &str = "https://static.cdninstagram.com";

/// Desktop Chrome user agent sent when none is configured.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";

/// Default pause between two API calls (a random 0–100 % is added).
pub const DEFAULT_API_DELAY: Duration = Duration::from_secs(6);

/// Default global pause after an HTTP 429.
pub const DEFAULT_RATE_LIMIT_WAIT: Duration = Duration::from_secs(60);

/// `X-IG-App-ID` of the Instagram web app.
pub const IG_APP_ID: &str = "936619743392459";

/// `X-ASBD-ID` sent by the Instagram web app (gallery-dl 1.32.14+, yt-dlp).
pub const ASBD_ID: &str = "359341";

/// Error text for missing or expired cookies.
pub const NOT_LOGGED_IN: &str =
    "not logged in: cookies missing or expired — re-export them from your browser";

/// Error text for a login Instagram wants confirmed (checkpoint / challenge).
pub const CHECKPOINT: &str = "Instagram wants you to confirm this login: open instagram.com in the browser the cookies came from, then re-export";

/// Error text after a second HTTP 429.
pub const RATE_LIMITED: &str = "Instagram is rate limiting this account — wait 10-30 minutes, then try again with a larger API delay";

/// Error text for `feedback_required` (an account-level restriction).
pub const FEEDBACK_REQUIRED: &str = "Instagram has temporarily restricted this account (feedback_required): wait, open instagram.com in the browser, re-export cookies";

/// Error text for "Please wait a few minutes before you try again".
pub const PLEASE_WAIT: &str =
    "Instagram asked to wait a few minutes before trying again: re-export cookies or wait";

/// "Who am I" step 1: the account settings form (only `form_data.username` is read).
const WEB_FORM_DATA: &str = "/api/v1/accounts/edit/web_form_data/";

/// "Who am I" step 2: instaloader's `test_login` query (`data.user.username`).
const LOGIN_PROBE: &str =
    "/graphql/query?query_hash=d6f4427fbe92d846298cf93df0b937d3&variables=%7B%7D";

/// Per-request timeout for the web app's static scripts (doc id discovery).
const STATIC_TIMEOUT: Duration = Duration::from_secs(15);

/// Longest server text (in characters) copied into an error message.
const MAX_SERVER_TEXT: usize = 200;

/// Pages whose parsed contents are kept (profile pages, tokens, bundle lists).
const MAX_CACHED_PAGES: usize = 32;

/// `Accept` of a browser page navigation.
const NAVIGATION_ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7";

/// Client settings.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Scheme + host (+ optional port), no trailing slash. Cookies are sent only here.
    pub api_base: String,
    pub user_agent: String,
    /// Minimum pause between two API calls; a random 0–100 % is added. HTML
    /// pages and static scripts are exempt.
    pub api_delay: Duration,
    /// How often a request answered with HTTP 429 is retried (after the global pause).
    pub rate_limit_retries: u32,
    /// Global pause after an HTTP 429: every request waits until it is over.
    pub rate_limit_wait: Duration,
    /// Per-request timeout.
    pub timeout: Duration,
    /// How often a 5xx or network failure is retried.
    pub transient_retries: u32,
    /// First wait before retrying a 5xx / network failure; doubles every time.
    pub transient_wait: Duration,
    /// Scheme + host of the JS bundles scanned for GraphQL doc ids (never sent cookies).
    pub static_base: String,
    /// Directory for `doc_ids.json` (the data directory); `None` keeps
    /// discovered doc ids in memory only.
    pub cache_dir: Option<PathBuf>,
    /// GraphQL doc id overrides (`[doc_ids]` in `config.toml`): operation name → id.
    pub doc_ids: BTreeMap<String, String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            api_base: DEFAULT_API_BASE.to_string(),
            user_agent: DEFAULT_USER_AGENT.to_string(),
            api_delay: DEFAULT_API_DELAY,
            rate_limit_retries: 1,
            rate_limit_wait: DEFAULT_RATE_LIMIT_WAIT,
            timeout: Duration::from_secs(30),
            transient_retries: 2,
            transient_wait: Duration::from_secs(2),
            static_base: DEFAULT_STATIC_BASE.to_string(),
            cache_dir: None,
            doc_ids: BTreeMap::new(),
        }
    }
}

/// The parts of a user profile `igdl` needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSummary {
    pub id: String,
    pub username: String,
    pub full_name: Option<String>,
    /// From the profile page when it says so (else `false`).
    pub is_private: bool,
    /// Best effort: `true` only when the profile page says so.
    pub followed_by_viewer: bool,
}

/// How one API call is made and classified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CallOptions {
    /// One attempt that never waits (interactive checks); a 429 still records
    /// the global pause.
    pub quick: bool,
    /// Retry 5xx and network failures.
    pub retry_transient: bool,
    /// A redirect to the home page means the cookies were not accepted.
    pub home_redirect_is_auth: bool,
    /// A 401/403 without a login signal is [`Error::Auth`] (else [`Error::Http`]).
    pub forbidden_is_auth: bool,
}

impl Default for CallOptions {
    fn default() -> Self {
        CallOptions {
            quick: false,
            retry_transient: true,
            home_redirect_is_auth: false,
            forbidden_is_auth: true,
        }
    }
}

/// One API call.
pub(crate) struct ApiRequest<'a> {
    /// Path and query relative to the API base.
    pub path: &'a str,
    /// Form-encoded body: the request is a POST.
    pub form: Option<&'a str>,
    /// Page path for `Referer` (default `/`).
    pub referer: Option<&'a str>,
    pub extra_headers: HeaderMap,
    /// A GraphQL operation: no `X-Requested-With`, and HTTP 200/400 JSON bodies
    /// are returned as they are (errors inside them are interpreted by the caller).
    pub graphql: bool,
    pub options: CallOptions,
}

impl<'a> ApiRequest<'a> {
    /// A GET of `path`.
    pub fn get(path: &'a str) -> Self {
        ApiRequest {
            path,
            form: None,
            referer: None,
            extra_headers: HeaderMap::new(),
            graphql: false,
            options: CallOptions::default(),
        }
    }

    /// With other call options.
    pub fn options(mut self, options: CallOptions) -> Self {
        self.options = options;
        self
    }
}

/// What a request is, for pacing and headers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Api,
    Page,
    Static,
}

/// Authenticated, throttled Instagram client. Cheap to share behind an `Arc`.
pub struct Client {
    http: reqwest::Client,
    /// For static bundles only: never given cookies.
    static_http: reqwest::Client,
    config: ClientConfig,
    /// `api_base` without a trailing slash.
    pub(super) base: String,
    /// `static_base` without a trailing slash.
    pub(super) static_base: String,
    /// Never held across an `.await`.
    jar: Mutex<CookieJar>,
    /// `X-IG-WWW-Claim`: `0` until a response sets one.
    www_claim: Mutex<String>,
    /// Serialises waiting for a turn, so pauses and delays apply in order.
    turn: tokio::sync::Mutex<()>,
    /// When the latest API call started or finished.
    last_api: Mutex<Option<Instant>>,
    /// Global pause after a 429: no request starts before this.
    pause_until: Mutex<Option<Instant>>,
    /// When a crawl last met a 429 (quick checks do not count).
    last_rate_limit: Mutex<Option<Instant>>,
    /// Parsed pages by path.
    pages: Mutex<HashMap<String, Arc<PageInfo>>>,
    /// Tokens of the latest page that had some.
    latest_tokens: Mutex<Option<Tokens>>,
    /// GraphQL doc ids: overrides, cache and discovery state.
    pub(super) doc_ids: tokio::sync::Mutex<DocIdState>,
    /// `__req` counter of GraphQL requests.
    pub(super) request_counter: AtomicU32,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("api_base", &self.base)
            .field("cookies", &*self.lock_jar())
            .finish_non_exhaustive()
    }
}

/// How one attempt failed.
enum Failure {
    /// HTTP 429: pause everything, retry once; the message is already redacted.
    RateLimited(String),
    /// 5xx or network: retry after a short backoff.
    Transient(Error),
    /// Do not retry.
    Fatal(Error),
}

/// A response with its body read.
struct Raw {
    status: StatusCode,
    location: Option<String>,
    content_type: String,
    body: String,
}

/// Locks a std mutex; a poisoned one is still usable (every mutation is a
/// single assignment or call).
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Validates a base URL (`http(s)://host[:port]`) and strips trailing slashes.
fn parse_base(base: &str, what: &str) -> Result<String> {
    let base = base.trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&base)
        .map_err(|err| Error::Config(format!("invalid {what} '{base}': {err}")))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(Error::Config(format!(
            "invalid {what} '{base}': expected http(s)://host"
        )));
    }
    Ok(base)
}

impl Client {
    /// Builds a client; `cookies` may be `None` for anonymous access (most
    /// endpoints will then fail with [`crate::error::Error::Auth`]).
    pub fn new(config: ClientConfig, cookies: Option<CookieJar>) -> Result<Self> {
        let base = parse_base(&config.api_base, "API base")?;
        let static_base = parse_base(&config.static_base, "static base")?;
        let user_agent = HeaderValue::from_str(&config.user_agent)
            .map_err(|_| Error::Config("the user agent contains invalid characters".into()))?;
        let build = || {
            reqwest::Client::builder()
                .user_agent(user_agent.clone())
                .timeout(config.timeout)
                .redirect(reqwest::redirect::Policy::none())
                .gzip(true)
                .build()
                .map_err(|err| Error::Config(format!("cannot build the HTTP client: {err}")))
        };
        let http = build()?;
        let static_http = build()?;
        let doc_ids = DocIdState::new(&config.doc_ids, config.cache_dir.as_deref());
        Ok(Client {
            http,
            static_http,
            base,
            static_base,
            jar: Mutex::new(cookies.unwrap_or_default()),
            www_claim: Mutex::new("0".to_string()),
            turn: tokio::sync::Mutex::new(()),
            last_api: Mutex::new(None),
            pause_until: Mutex::new(None),
            last_rate_limit: Mutex::new(None),
            pages: Mutex::new(HashMap::new()),
            latest_tokens: Mutex::new(None),
            doc_ids: tokio::sync::Mutex::new(doc_ids),
            request_counter: AtomicU32::new(0),
            config,
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

    /// The current `X-IG-WWW-Claim` value (`0` until Instagram sets one).
    pub fn www_claim(&self) -> String {
        lock(&self.www_claim).clone()
    }

    /// How long the global rate-limit pause still lasts, if one is active.
    pub fn paused_for(&self) -> Option<Duration> {
        self.pause_deadline()
            .map(|until| until.saturating_duration_since(Instant::now()))
    }

    /// The logged-in user's id (`ds_user_id` cookie) when it is numeric.
    pub(crate) fn viewer_id(&self) -> Option<String> {
        self.lock_jar()
            .get("ds_user_id")
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_string)
    }

    /// GETs `path_and_query` (e.g. `/api/v1/media/123/info/`) relative to the API
    /// base, with pacing, the 429 pause, retries and error classification.
    pub async fn get_json(&self, path_and_query: &str) -> Result<serde_json::Value> {
        self.api(&ApiRequest::get(path_and_query)).await
    }

    /// POSTs an `application/x-www-form-urlencoded` body (with `X-CSRFToken`),
    /// with the same pacing and error handling as [`Client::get_json`].
    pub async fn post_form(&self, path: &str, form: &[(&str, &str)]) -> Result<serde_json::Value> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter())
            .finish();
        let mut request = ApiRequest::get(path);
        request.form = Some(&body);
        self.api(&request).await
    }

    /// The account the cookies belong to.
    ///
    /// Offline first: without a `sessionid` cookie this is [`Error::Auth`] and
    /// nothing is sent. Then, one quick attempt each (no pause, no retries):
    /// 1. `GET /api/v1/accounts/edit/web_form_data/` → `form_data.username`
    ///    (nothing else of that response is read: it holds e-mail, phone and
    ///    birthday); the id comes from the `ds_user_id` cookie;
    /// 2. instaloader's login probe `GET /graphql/query/?query_hash=d6f4427f…`
    ///    → `data.user.username` (`data.user == null` means not logged in).
    ///
    /// A login redirect, `login_required`, `require_login`, a checkpoint or a
    /// `null` user ends the check with [`Error::Auth`]. Rate limits, 5xx, network
    /// errors, "Please wait a few minutes" and redirects to the home page move on
    /// to the next step; when every step fails that way the result is
    /// [`Error::RateLimited`] (or [`Error::Network`]) saying the cookies are
    /// probably fine.
    pub async fn current_user(&self) -> Result<UserSummary> {
        if !self.is_authenticated() {
            return Err(Error::Auth(NOT_LOGGED_IN.into()));
        }
        let viewer_id = self.viewer_id();
        let options = CallOptions {
            quick: true,
            retry_transient: false,
            home_redirect_is_auth: false,
            forbidden_is_auth: false,
        };
        let mut failures: Vec<(&str, Error)> = Vec::new();

        let form = self
            .api(&ApiRequest::get(WEB_FORM_DATA).options(options))
            .await
            .and_then(username_from_form);
        match form {
            Ok(username) => {
                return Ok(viewer_summary(viewer_id.unwrap_or_default(), username));
            }
            Err(err) if ends_login_check(&err) => return Err(err),
            Err(err) => {
                tracing::debug!(error = %err, "login check: web_form_data failed");
                failures.push(("account form", err));
            }
        }

        let probe = self
            .api(&ApiRequest::get(LOGIN_PROBE).options(options))
            .await
            .and_then(user_from_probe);
        match probe {
            Ok((username, id)) => {
                let id = viewer_id.or(id).unwrap_or_default();
                Ok(viewer_summary(id, username))
            }
            Err(err) if ends_login_check(&err) => Err(err),
            Err(err) => {
                tracing::debug!(error = %err, "login check: GraphQL probe failed");
                failures.push(("login probe", err));
                Err(unverified(&failures))
            }
        }
    }

    /// Looks a profile up by username: `GET /{username}/` like a browser
    /// navigation, then reads the profile id (and tokens for GraphQL) from the page.
    pub async fn user_by_username(&self, username: &str) -> Result<UserSummary> {
        let username = username.trim().trim_start_matches('@').to_ascii_lowercase();
        let not_found = || Error::NotFound(format!("user @{username}"));
        let valid = username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_');
        if username.is_empty() || !valid {
            return Err(not_found());
        }
        let info = match self.page_info(&format!("/{username}/"), true).await {
            Err(Error::NotFound(_)) => return Err(not_found()),
            other => other?,
        };
        let Some(id) = info.profile_id.clone() else {
            if info.login_form {
                return Err(Error::Auth(NOT_LOGGED_IN.into()));
            }
            tracing::debug!(user = %username, "the profile page names no profile id");
            return Err(not_found());
        };
        let page_name = info
            .username
            .as_deref()
            .filter(|name| name.eq_ignore_ascii_case(&username))
            .map(str::to_ascii_lowercase);
        Ok(UserSummary {
            id,
            username: page_name.unwrap_or(username),
            full_name: None,
            is_private: info.is_private.unwrap_or(false),
            followed_by_viewer: info.followed_by_viewer.unwrap_or(false),
        })
    }

    /// The parsed page at `path` (`/alice/`), fetched unless already known (or
    /// `refresh`).
    pub(crate) async fn page_info(&self, path: &str, refresh: bool) -> Result<Arc<PageInfo>> {
        if !refresh && let Some(info) = self.cached_page(path) {
            return Ok(info);
        }
        let html = self.fetch_page(path).await?;
        let info = Arc::new(web::parse_page(&html, &self.static_base));
        drop(html);
        tracing::debug!(
            page = %path,
            profile_id = info.profile_id.is_some(),
            tokens = !info.tokens.is_empty(),
            bundles = info.bundles.len(),
            "parsed page"
        );
        {
            let mut pages = lock(&self.pages);
            if pages.len() >= MAX_CACHED_PAGES {
                pages.clear();
            }
            pages.insert(path.to_string(), Arc::clone(&info));
        }
        if !info.tokens.is_empty() {
            *lock(&self.latest_tokens) = Some(info.tokens.clone());
        }
        Ok(info)
    }

    /// The page at `path` if it was parsed before.
    pub(crate) fn cached_page(&self, path: &str) -> Option<Arc<PageInfo>> {
        lock(&self.pages).get(path).cloned()
    }

    /// Tokens of the latest page that had some.
    pub(crate) fn latest_tokens(&self) -> Option<Tokens> {
        lock(&self.latest_tokens).clone()
    }

    /// An API call with pacing, the 429 pause, retries and classification.
    pub(crate) async fn api(&self, request: &ApiRequest<'_>) -> Result<Value> {
        check_path(request.path)?;
        let url = format!("{}{}", self.base, request.path);
        let url = url.as_str();
        self.run(Kind::Api, request.options, || self.api_once(url, request))
            .await
    }

    /// GETs an HTML page of the API host like a browser navigation (with the
    /// cookies, without `api_delay`).
    pub(crate) async fn fetch_page(&self, path: &str) -> Result<String> {
        check_path(path)?;
        let url = format!("{}{}", self.base, path);
        let url = url.as_str();
        self.run(Kind::Page, CallOptions::default(), || {
            self.page_once(url, path)
        })
        .await
    }

    /// GETs a JS bundle under the static base — never with cookies, without
    /// `api_delay`, one attempt.
    pub(crate) async fn fetch_static(&self, url: &str) -> Result<String> {
        let prefix = format!("{}/", self.static_base);
        if !url.starts_with(&prefix) {
            return Err(Error::Config(format!(
                "refusing to fetch {} (not under the static base)",
                redact::redact_url(url)
            )));
        }
        let options = CallOptions {
            retry_transient: false,
            ..CallOptions::default()
        };
        self.run(Kind::Static, options, || self.static_once(url))
            .await
    }

    fn lock_jar(&self) -> MutexGuard<'_, CookieJar> {
        lock(&self.jar)
    }

    /// `text` redacted against the jar and truncated for an error message.
    pub(super) fn server_text(&self, text: &str) -> String {
        let redacted = redact::redact_with(text.trim(), &self.lock_jar());
        models::truncate(&redacted, MAX_SERVER_TEXT)
    }

    /// Runs `once` until it succeeds or fails for good.
    async fn run<T, F, Fut>(&self, kind: Kind, options: CallOptions, mut once: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = std::result::Result<T, Failure>>,
    {
        let mut rate_attempt: u32 = 0;
        let mut transient_attempt: u32 = 0;
        loop {
            self.wait_turn(kind, options.quick).await;
            match once().await {
                Ok(value) => return Ok(value),
                Err(Failure::Fatal(err)) => return Err(err),
                Err(Failure::RateLimited(detail)) => {
                    self.pause_all();
                    if options.quick {
                        return Err(Error::RateLimited(detail));
                    }
                    if self.repeated_rate_limit() {
                        tracing::warn!("{detail} again soon after the last one; giving up");
                        return Err(Error::RateLimited(RATE_LIMITED.to_string()));
                    }
                    if rate_attempt >= self.config.rate_limit_retries {
                        tracing::warn!("{detail} again; giving up");
                        return Err(Error::RateLimited(RATE_LIMITED.to_string()));
                    }
                    rate_attempt += 1;
                    tracing::warn!(
                        wait_secs = self.config.rate_limit_wait.as_secs_f64(),
                        "{detail}: pausing every request for {:.0?}, then retrying",
                        self.config.rate_limit_wait
                    );
                }
                Err(Failure::Transient(err)) => {
                    if options.quick
                        || !options.retry_transient
                        || transient_attempt >= self.config.transient_retries
                    {
                        return Err(err);
                    }
                    let wait = backoff(self.config.transient_wait, transient_attempt);
                    tracing::warn!(
                        error = %err,
                        attempt = transient_attempt + 1,
                        "request failed; retrying in {:.1?}",
                        wait
                    );
                    transient_attempt += 1;
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    /// Waits for the global pause (unless `quick`) and, for API calls, until
    /// `api_delay` + jitter has passed since the previous API call.
    async fn wait_turn(&self, kind: Kind, quick: bool) {
        if quick {
            // Interactive checks go straight through: no queue behind other
            // callers' pauses and delays (they still count as an API call).
            if kind == Kind::Api {
                self.mark_api_call();
            }
            return;
        }
        let _turn = self.turn.lock().await;
        while let Some(until) = self.pause_deadline() {
            tracing::info!(
                "waiting {:.0?} before the next request (Instagram rate limit)",
                until.saturating_duration_since(Instant::now())
            );
            tokio::time::sleep_until(until).await;
        }
        if kind != Kind::Api {
            return;
        }
        let delay = self.config.api_delay;
        let previous = *lock(&self.last_api);
        if !delay.is_zero()
            && let Some(previous) = previous
        {
            let jitter = delay.mul_f64(rand::random_range(0.0..=1.0));
            tokio::time::sleep_until(previous + delay + jitter).await;
        }
        self.mark_api_call();
    }

    fn mark_api_call(&self) {
        *lock(&self.last_api) = Some(Instant::now());
    }

    /// Records a 429 met while crawling; `true` when another one came less than
    /// ten pause lengths before it, which means the job should stop rather than
    /// keep provoking Instagram (gallery-dl #9540: accounts got locked that way).
    fn repeated_rate_limit(&self) -> bool {
        let now = Instant::now();
        let window = self.config.rate_limit_wait.saturating_mul(10);
        let previous = lock(&self.last_rate_limit).replace(now);
        previous.is_some_and(|at| now.saturating_duration_since(at) < window)
    }

    /// Starts (or extends) the global pause after a 429.
    fn pause_all(&self) {
        let until = Instant::now() + self.config.rate_limit_wait;
        let mut pause = lock(&self.pause_until);
        if pause.is_none_or(|current| current < until) {
            *pause = Some(until);
        }
    }

    /// The end of the global pause, if it is still running.
    fn pause_deadline(&self) -> Option<Instant> {
        lock(&self.pause_until).filter(|until| *until > Instant::now())
    }

    /// Headers every request to the API host carries: cookies, CSRF token.
    fn auth_headers(
        &self,
        headers: &mut HeaderMap,
        with_csrf: bool,
    ) -> std::result::Result<(), Failure> {
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
        if with_csrf && let Some(csrf) = csrf.filter(|token| !token.is_empty()) {
            let mut value = HeaderValue::from_str(&csrf).map_err(|_| {
                Failure::Fatal(Error::Cookies(
                    "the csrftoken cookie contains characters not allowed in an HTTP header".into(),
                ))
            })?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static("x-csrftoken"), value);
        }
        Ok(())
    }

    /// Headers of an API call: the web app's, plus cookies and CSRF token.
    fn api_headers(&self, request: &ApiRequest<'_>) -> std::result::Result<HeaderMap, Failure> {
        let mut headers = HeaderMap::new();
        let fixed: [(&str, &str); 8] = [
            ("accept", "*/*"),
            ("accept-language", "en-US,en;q=0.9"),
            ("x-ig-app-id", IG_APP_ID),
            ("x-asbd-id", ASBD_ID),
            ("x-ig-max-touch-points", "0"),
            ("sec-fetch-dest", "empty"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "same-origin"),
        ];
        for (name, value) in fixed {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        if !request.graphql {
            headers.insert(
                HeaderName::from_static("x-requested-with"),
                HeaderValue::from_static("XMLHttpRequest"),
            );
        }
        let claim = HeaderValue::from_str(&self.www_claim())
            .unwrap_or_else(|_| HeaderValue::from_static("0"));
        headers.insert(HeaderName::from_static("x-ig-www-claim"), claim);
        if let Ok(value) = HeaderValue::from_str(&self.base) {
            headers.insert(header::ORIGIN, value);
        }
        let referer = format!("{}{}", self.base, request.referer.unwrap_or("/"));
        if let Ok(value) = HeaderValue::from_str(&referer) {
            headers.insert(header::REFERER, value);
        }
        if request.form.is_some() {
            headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        for (name, value) in &request.extra_headers {
            headers.insert(name.clone(), value.clone());
        }
        self.auth_headers(&mut headers, true)?;
        Ok(headers)
    }

    /// Headers of a page fetch: a browser navigation, with the cookies.
    fn page_headers(&self) -> std::result::Result<HeaderMap, Failure> {
        let mut headers = HeaderMap::new();
        let fixed: [(&str, &str); 7] = [
            ("accept", NAVIGATION_ACCEPT),
            ("accept-language", "en-US,en;q=0.9"),
            ("sec-fetch-dest", "document"),
            ("sec-fetch-mode", "navigate"),
            ("sec-fetch-site", "none"),
            ("sec-fetch-user", "?1"),
            ("upgrade-insecure-requests", "1"),
        ];
        for (name, value) in fixed {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        self.auth_headers(&mut headers, false)?;
        Ok(headers)
    }

    /// Headers of a static script fetch. No cookies, ever.
    fn static_headers(&self) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let fixed: [(&str, &str); 5] = [
            ("accept", "*/*"),
            ("accept-language", "en-US,en;q=0.9"),
            ("sec-fetch-dest", "script"),
            ("sec-fetch-mode", "cors"),
            ("sec-fetch-site", "cross-site"),
        ];
        for (name, value) in fixed {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_static(value),
            );
        }
        if let Ok(value) = HeaderValue::from_str(&self.base) {
            headers.insert(header::ORIGIN, value);
        }
        if let Ok(value) = HeaderValue::from_str(&format!("{}/", self.base)) {
            headers.insert(header::REFERER, value);
        }
        headers
    }

    /// Sends a request and reads the response. Responses from the API host
    /// update the jar and the `X-IG-WWW-Claim`.
    async fn send(
        &self,
        kind: Kind,
        request: reqwest::RequestBuilder,
        label: &str,
    ) -> std::result::Result<Raw, Failure> {
        let response = request
            .send()
            .await
            .map_err(|err| Failure::Transient(Error::from(err)))?;
        let status = response.status();
        let version = response.version();
        if kind != Kind::Static {
            self.apply_set_cookies(response.headers());
            self.update_www_claim(response.headers());
        }
        let header_text = |name: header::HeaderName| {
            response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };
        let location = header_text(header::LOCATION);
        let content_type = header_text(header::CONTENT_TYPE).unwrap_or_default();
        let body = response
            .text()
            .await
            .map_err(|err| Failure::Transient(Error::from(err)))?;
        if status == StatusCode::TOO_MANY_REQUESTS || status.is_redirection() {
            // Never the body: it may be a page full of personal data.
            tracing::debug!(
                http_version = ?version,
                status = status.as_u16(),
                content_type = %content_type,
                body_bytes = body.len(),
                request = %label,
                "Instagram answered with a {}",
                if status.is_redirection() { "redirect" } else { "rate limit" }
            );
        }
        Ok(Raw {
            status,
            location,
            content_type,
            body,
        })
    }

    async fn api_once(
        &self,
        url: &str,
        request: &ApiRequest<'_>,
    ) -> std::result::Result<Value, Failure> {
        let headers = self.api_headers(request)?;
        let builder = match request.form {
            None => self.http.request(Method::GET, url),
            Some(body) => self.http.request(Method::POST, url).body(body.to_string()),
        };
        let label = short_path(request.path);
        tracing::debug!(
            method = if request.form.is_some() { "POST" } else { "GET" },
            path = %label,
            "API request"
        );
        let raw = self.send(Kind::Api, builder.headers(headers), &label).await;
        // The delay counts from the end of a call, too.
        self.mark_api_call();
        self.classify_api(&raw?, request)
    }

    async fn page_once(&self, url: &str, path: &str) -> std::result::Result<String, Failure> {
        let headers = self.page_headers()?;
        let label = short_path(path);
        tracing::debug!(page = %label, "page request");
        let raw = self
            .send(
                Kind::Page,
                self.http.request(Method::GET, url).headers(headers),
                &label,
            )
            .await?;
        self.classify_page(raw, path)
    }

    async fn static_once(&self, url: &str) -> std::result::Result<String, Failure> {
        let label = redact::redact_url(url);
        tracing::debug!(bundle = %label, "static script request");
        let request = self
            .static_http
            .request(Method::GET, url)
            .timeout(self.config.timeout.min(STATIC_TIMEOUT))
            .headers(self.static_headers());
        let raw = self.send(Kind::Static, request, &label).await?;
        if raw.status.is_success() {
            Ok(raw.body)
        } else {
            Err(Failure::Fatal(Error::Http {
                status: raw.status.as_u16(),
                message: format!("static script {label}"),
            }))
        }
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

    /// Adopts `x-ig-set-www-claim` (gallery-dl does the same).
    fn update_www_claim(&self, headers: &HeaderMap) {
        let Some(claim) = headers
            .get("x-ig-set-www-claim")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|claim| !claim.is_empty() && HeaderValue::from_str(claim).is_ok())
        else {
            return;
        };
        *lock(&self.www_claim) = claim.to_string();
    }

    /// The error for a redirect.
    fn redirect_error(
        &self,
        status: StatusCode,
        location: Option<&str>,
        path: &str,
        home_is_auth: bool,
    ) -> Error {
        let target = location.unwrap_or_default();
        if target.contains("/accounts/login") {
            Error::Auth(NOT_LOGGED_IN.into())
        } else if target.contains("/challenge") || target.contains("/checkpoint") {
            Error::Auth(CHECKPOINT.into())
        } else if is_home_redirect(target) {
            let shown = short_path(path);
            if home_is_auth {
                Error::Auth(format!(
                    "{NOT_LOGGED_IN} (Instagram redirected {shown} to the home page)"
                ))
            } else {
                Error::Http {
                    status: status.as_u16(),
                    message: format!("Instagram redirected {shown} to the home page"),
                }
            }
        } else {
            Error::Http {
                status: status.as_u16(),
                message: format!(
                    "unexpected redirect to {}",
                    self.server_text(&redact::redact_url(target))
                ),
            }
        }
    }

    /// Maps an API response to a JSON value or a [`Failure`].
    fn classify_api(
        &self,
        raw: &Raw,
        request: &ApiRequest<'_>,
    ) -> std::result::Result<Value, Failure> {
        let status = raw.status;
        let code = status.as_u16();
        let path = request.path;
        if status.is_redirection() {
            return Err(Failure::Fatal(self.redirect_error(
                status,
                raw.location.as_deref(),
                path,
                request.options.home_redirect_is_auth,
            )));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Failure::RateLimited(format!(
                "Instagram answered HTTP 429 on {}",
                short_path(path)
            )));
        }
        let json = parse_json_object(&raw.body);
        if let Some(json) = &json
            && !(status.is_success() && json.get("status").and_then(Value::as_str) == Some("ok"))
            && let Some(failure) = self.classify_json_signal(json, code, request.graphql)
        {
            return Err(failure);
        }
        if status.is_server_error() {
            return Err(Failure::Transient(Error::Http {
                status: code,
                message: self.error_message(json.as_ref(), &raw.body, "Instagram server error"),
            }));
        }
        // Only the login form itself; other HTML (the home page) is ambiguous.
        if json.is_none() && web::looks_like_login_page(&raw.body) {
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
            return Err(Failure::Fatal(if request.options.forbidden_is_auth {
                Error::Auth(format!(
                    "Instagram refused the request (HTTP {code}){detail}; the cookies may be expired — re-export them from your browser"
                ))
            } else {
                Error::Http {
                    status: code,
                    message: format!("Instagram refused {}{detail}", short_path(path)),
                }
            }));
        }
        if request.graphql
            && (status == StatusCode::OK || status == StatusCode::BAD_REQUEST)
            && let Some(json) = json
        {
            return Ok(json);
        }
        if !status.is_success() {
            return Err(Failure::Fatal(Error::Http {
                status: code,
                message: self.error_message(json.as_ref(), &raw.body, "request failed"),
            }));
        }
        let Some(json) = json else {
            let kind = if raw.content_type.is_empty() {
                "unknown content type".to_string()
            } else {
                self.server_text(&raw.content_type)
            };
            return Err(Failure::Fatal(Error::Parse(format!(
                "expected JSON from {} but got a non-JSON response ({kind}, {} bytes)",
                short_path(path),
                raw.body.len()
            ))));
        };
        if json.get("status").and_then(Value::as_str) == Some("fail") {
            return Err(Failure::Fatal(Error::Http {
                status: code,
                message: self.error_message(Some(&json), &raw.body, "Instagram reported a failure"),
            }));
        }
        Ok(json)
    }

    /// Maps a page response to its HTML or a [`Failure`].
    fn classify_page(&self, raw: Raw, path: &str) -> std::result::Result<String, Failure> {
        let status = raw.status;
        if status.is_redirection() {
            return Err(Failure::Fatal(self.redirect_error(
                status,
                raw.location.as_deref(),
                path,
                false,
            )));
        }
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(Failure::RateLimited(format!(
                "Instagram answered HTTP 429 on the page {}",
                short_path(path)
            )));
        }
        if status.is_server_error() {
            return Err(Failure::Transient(Error::Http {
                status: status.as_u16(),
                message: format!("Instagram server error on the page {}", short_path(path)),
            }));
        }
        if status == StatusCode::NOT_FOUND {
            return Err(Failure::Fatal(Error::NotFound(short_path(path))));
        }
        if !status.is_success() {
            return Err(Failure::Fatal(Error::Http {
                status: status.as_u16(),
                message: format!("Instagram refused the page {}", short_path(path)),
            }));
        }
        Ok(raw.body)
    }

    /// Login / checkpoint / restriction signals inside a JSON body (any status).
    /// For GraphQL answers `feedback_required` is left to the caller: a stale doc
    /// id gets that answer too (instaloader `structures.py`), so it first goes
    /// through the outdated-query retry.
    fn classify_json_signal(&self, json: &Value, code: u16, graphql: bool) -> Option<Failure> {
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
        if !graphql && has("feedback_required") {
            return Some(Failure::Fatal(Error::RateLimited(FEEDBACK_REQUIRED.into())));
        }
        // Before the login check: Instagram's throttle answer is
        // `{"message":"Please wait a few minutes…","require_login":true}`.
        if message.contains("please wait a few minutes") {
            return Some(Failure::Fatal(Error::RateLimited(format!(
                "{PLEASE_WAIT} (HTTP {code})"
            ))));
        }
        if has("login_required") || json.get("require_login") == Some(&Value::Bool(true)) {
            return Some(Failure::Fatal(Error::Auth(NOT_LOGGED_IN.into())));
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

/// Only paths: cookies must never be sent anywhere but the API base.
fn check_path(path: &str) -> Result<()> {
    if !path.starts_with('/') || path.starts_with("//") {
        return Err(Error::Config(format!(
            "API path must start with '/': {}",
            redact::redact_url(path)
        )));
    }
    Ok(())
}

/// Whether a `Location` is the home page (`/`, `https://www.instagram.com/`).
fn is_home_redirect(location: &str) -> bool {
    let target = location.split(['?', '#']).next().unwrap_or_default();
    if target == "/" {
        return true;
    }
    url::Url::parse(target).is_ok_and(|url| url.path() == "/")
}

/// A JSON object body; Facebook's `for (;;);` XHR prefix is skipped.
fn parse_json_object(body: &str) -> Option<Value> {
    let body = body.trim_start();
    let body = body.strip_prefix("for (;;);").unwrap_or(body);
    serde_json::from_str::<Value>(body)
        .ok()
        .filter(Value::is_object)
}

/// `base * 2^attempt`, saturating.
fn backoff(base: Duration, attempt: u32) -> Duration {
    base.saturating_mul(2u32.saturating_pow(attempt))
}

/// The path without its query (for messages and logs).
pub(super) fn short_path(path: &str) -> String {
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

/// Errors that end the login check at once.
fn ends_login_check(err: &Error) -> bool {
    matches!(err, Error::Auth(_) | Error::Cookies(_) | Error::Config(_))
}

/// `form_data.username` of `web_form_data`. Nothing else is read, and the
/// response is dropped here: it holds e-mail, phone and birthday. No serde
/// error text either (it could quote a value).
fn username_from_form(value: Value) -> Result<String> {
    value
        .get("form_data")
        .and_then(|form| form.get("username"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Parse("the account form named no username".into()))
}

/// `data.user.username` (and `id`) of the login probe; `data.user == null`
/// means the session is not logged in.
fn user_from_probe(value: Value) -> Result<(String, Option<String>)> {
    let Some(data) = value.get("data") else {
        return Err(Error::Parse("the login probe returned no data".into()));
    };
    match data.get("user") {
        Some(Value::Null) => Err(Error::Auth(NOT_LOGGED_IN.into())),
        Some(user) => {
            let username = user
                .get("username")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| Error::Parse("the login probe named no username".into()))?;
            let id = user
                .get("id")
                .or_else(|| user.get("pk"))
                .and_then(models::id_from_value);
            Ok((username.to_string(), id))
        }
        None => Err(Error::Parse("the login probe returned no user".into())),
    }
}

fn viewer_summary(id: String, username: String) -> UserSummary {
    UserSummary {
        id,
        username,
        full_name: None,
        is_private: false,
        followed_by_viewer: false,
    }
}

/// The login check's result when no step could tell: not a cookie problem.
fn unverified(failures: &[(&str, Error)]) -> Error {
    let details = failures
        .iter()
        .map(|(step, err)| format!("{step}: {err}"))
        .collect::<Vec<_>>()
        .join("; ");
    if failures
        .iter()
        .all(|(_, err)| matches!(err, Error::Network(_)))
    {
        Error::Network(format!(
            "could not reach Instagram to check the login ({details}); the cookies are probably fine — check the connection and try again"
        ))
    } else {
        Error::RateLimited(format!(
            "Instagram refused the login check ({details}); the cookies are probably fine — wait a while, or try grabbing a single post"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles() {
        let base = Duration::from_secs(2);
        assert_eq!(backoff(base, 0), Duration::from_secs(2));
        assert_eq!(backoff(base, 2), Duration::from_secs(8));
        assert_eq!(backoff(Duration::ZERO, 5), Duration::ZERO);
    }

    #[test]
    fn rejects_bad_bases() {
        let config = ClientConfig {
            api_base: "ftp://x".into(),
            ..ClientConfig::default()
        };
        assert!(matches!(Client::new(config, None), Err(Error::Config(_))));
        let config = ClientConfig {
            static_base: "not a url".into(),
            ..ClientConfig::default()
        };
        assert!(matches!(Client::new(config, None), Err(Error::Config(_))));
    }

    #[test]
    fn defaults_follow_the_web_app() {
        let config = ClientConfig::default();
        assert_eq!(config.api_delay, Duration::from_secs(6));
        assert_eq!(config.rate_limit_wait, Duration::from_secs(60));
        assert_eq!(config.rate_limit_retries, 1);
        assert!(config.user_agent.contains("Chrome/150.0.0.0"));
        assert!(config.user_agent.contains("Windows NT 10.0"));
    }

    #[tokio::test]
    async fn absolute_urls_are_refused() {
        let client = Client::new(ClientConfig::default(), None).unwrap();
        let err = client.get_json("https://evil.test/x").await.unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        let err = client.get_json("//evil.test/x").await.unwrap_err();
        assert!(matches!(err, Error::Config(_)));
        let err = client
            .fetch_static("https://evil.test/rsrc.php/x.js")
            .await
            .unwrap_err();
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
    fn home_redirects() {
        assert!(is_home_redirect("/"));
        assert!(is_home_redirect("https://www.instagram.com/"));
        assert!(is_home_redirect("https://www.instagram.com/?next=x"));
        assert!(!is_home_redirect("/accounts/login/"));
        assert!(!is_home_redirect("https://www.instagram.com/p/abc/"));
        assert!(!is_home_redirect(""));
    }

    #[test]
    fn json_bodies() {
        assert!(parse_json_object(r#"for (;;);{"a":1}"#).is_some());
        assert!(parse_json_object("[1]").is_none());
        assert!(parse_json_object("<html>").is_none());
    }

    #[test]
    fn form_and_probe_parsing() {
        let form = serde_json::json!({"form_data": {"username": "me", "email": "me@example.com"}});
        assert_eq!(username_from_form(form).unwrap(), "me");
        let err = username_from_form(serde_json::json!({"form_data": {"email": 5}})).unwrap_err();
        assert!(!err.to_string().contains('5'), "{err}");
        assert!(matches!(
            user_from_probe(serde_json::json!({"data": {"user": null}})),
            Err(Error::Auth(_))
        ));
        assert_eq!(
            user_from_probe(serde_json::json!({"data": {"user": {"username": "me", "id": 7}}}))
                .unwrap(),
            ("me".to_string(), Some("7".to_string()))
        );
    }
}
