//! GraphQL requests the way the Instagram web app (and gallery-dl 1.32.17's
//! `_prepare_graphql` / `_call_graphql` / `_pagination_graphql`) sends them:
//! a form-encoded POST to `/graphql/query` or `/api/graphql` with the
//! operation's `doc_id`, the variables as JSON and the page's `lsd` /
//! `fb_dtsg` tokens.
//!
//! An error inside an HTTP 200 (`errors[]`, `missing_required_variable_value`,
//! `execution error`, unknown persisted query) means the doc id or the
//! variables are outdated: the doc id is rediscovered once and the request
//! retried; otherwise it is [`Error::Parse`] ("Instagram query … is outdated").

use std::collections::BTreeMap;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;

use crate::error::{Error, Result};
use crate::instagram::client::{ApiRequest, CallOptions, Client, FEEDBACK_REQUIRED};
use crate::instagram::doc_ids::{DocIdState, MAX_BUNDLES, OPERATIONS, Operation, extract_doc_ids};

/// Doc id discovery gives up after this many script downloads fail in a row
/// (a blocked or unreachable static host must not stall a grab for minutes).
const DISCOVERY_MAX_FAILURES: usize = 3;

/// Doc id discovery stops after this long and keeps what it found.
const DISCOVERY_BUDGET: std::time::Duration = std::time::Duration::from_secs(60);

/// How often discovery reports its progress (in scripts).
const DISCOVERY_PROGRESS_EVERY: usize = 20;
use crate::instagram::web::Tokens;
use crate::redact;

/// One page of a GraphQL connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Connection {
    /// `edges[].node`.
    pub nodes: Vec<Value>,
    pub has_next: bool,
    pub end_cursor: Option<String>,
}

/// Where a doc id came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Config,
    Cache,
    Discovered,
    Default,
}

/// Why a GraphQL answer is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Problem {
    /// The query (doc id or variables) is outdated.
    Outdated(String),
    /// Instagram refused to show the data (private profile).
    Permission(String),
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

impl Client {
    /// Runs `op`; returns its result: `data.<root field>`, or all of `data`
    /// for operations without a root field. `username` picks the page the
    /// web app runs the operation on (`Referer`, tokens, doc id discovery).
    pub(crate) async fn graphql(
        &self,
        op: &'static Operation,
        variables: &Value,
        username: Option<&str>,
    ) -> Result<Value> {
        let page = op.page_path(username);
        let variables = serde_json::to_string(variables)
            .map_err(|err| Error::Parse(format!("cannot encode GraphQL variables: {err}")))?;
        let (mut doc_id, source) = self.doc_id(op, &page).await?;
        let tokens = self.tokens_for(&page).await?;
        let mut retried = false;
        loop {
            let json = self
                .graphql_request(op, &doc_id, &variables, &page, &tokens)
                .await?;
            match interpret(&json, op) {
                Ok(data) => return Ok(data),
                Err(Problem::Permission(text)) => {
                    return Err(Error::Private(format!(
                        "Instagram refused {}: {}",
                        op.name,
                        self.server_text(&text)
                    )));
                }
                Err(Problem::Outdated(text)) => {
                    let text = self.server_text(&text);
                    tracing::warn!(query = op.name, doc_id = %doc_id, "Instagram rejected the query: {text}");
                    if !retried
                        && let Some(next) = self.next_doc_id(op, &page, &doc_id, source).await?
                    {
                        tracing::info!(query = op.name, doc_id = %next, "retrying with another doc id");
                        doc_id = next;
                        retried = true;
                        continue;
                    }
                    if text.to_ascii_lowercase().contains("feedback_required") {
                        // Still refused after another doc id: the account, not the query.
                        return Err(Error::RateLimited(FEEDBACK_REQUIRED.to_string()));
                    }
                    let hint = if source == Source::Config {
                        "update or remove it in [doc_ids] of config.toml"
                    } else {
                        "a newer igdl, or a [doc_ids] entry in config.toml, may fix it"
                    };
                    return Err(Error::Parse(format!(
                        "Instagram query {} is outdated: {text} (doc id {doc_id}; {hint})",
                        op.name
                    )));
                }
            }
        }
    }

    /// One page of a connection operation (`edges[].node`, `page_info`).
    pub(crate) async fn graphql_connection(
        &self,
        op: &'static Operation,
        variables: &Value,
        username: Option<&str>,
    ) -> Result<Connection> {
        Ok(connection(self.graphql(op, variables, username).await?))
    }

    /// The doc id for `op`: config, else a fresh cache entry, else discovery
    /// on `page` (once per page and session), else the built-in default.
    async fn doc_id(&self, op: &'static Operation, page: &str) -> Result<(String, Source)> {
        let mut state = self.doc_ids.lock().await;
        if let Some(id) = state.overrides.get(op.name) {
            return Ok((id.clone(), Source::Config));
        }
        state.ensure_loaded();
        if let Some(id) = state.cache.fresh(op.name, unix_now()) {
            return Ok((id.to_string(), Source::Cache));
        }
        if !state.scanned_pages.contains(page) {
            let found = self.discover(page, &mut state).await?;
            if let Some(id) = found.get(op.name) {
                return Ok((id.clone(), Source::Discovered));
            }
        }
        Ok((op.default_doc_id.to_string(), Source::Default))
    }

    /// After `failed` was rejected as outdated: forgets it in the cache,
    /// rediscovers once per operation and session, else offers the built-in
    /// default. `None`: nothing else to try.
    async fn next_doc_id(
        &self,
        op: &'static Operation,
        page: &str,
        failed: &str,
        source: Source,
    ) -> Result<Option<String>> {
        if source == Source::Config {
            return Ok(None);
        }
        let mut state = self.doc_ids.lock().await;
        state.ensure_loaded();
        let mut dirty = false;
        if state
            .cache
            .queries
            .get(op.name)
            .is_some_and(|entry| entry.doc_id == failed)
        {
            state.cache.queries.remove(op.name);
            dirty = true;
        }
        let mut candidate = None;
        if state.rediscovered.insert(op.name) && !state.scanned_pages.contains(page) {
            tracing::info!(query = op.name, "looking for a newer doc id");
            let found = self.discover(page, &mut state).await?;
            dirty &= found.is_empty();
            candidate = found.get(op.name).filter(|id| *id != failed).cloned();
        }
        if dirty {
            save_cache(&state).await;
        }
        Ok(candidate
            .or_else(|| (op.default_doc_id != failed).then(|| op.default_doc_id.to_string())))
    }

    /// Scans the JS bundles referenced by `page` for doc ids (without cookies)
    /// and records what it finds. Failures only fall back to the defaults,
    /// except the ones the GraphQL call itself would share (rate limit, login,
    /// cancellation), which end the operation.
    async fn discover(
        &self,
        page: &str,
        state: &mut DocIdState,
    ) -> Result<BTreeMap<&'static str, String>> {
        state.scanned_pages.insert(page.to_string());
        let mut found: BTreeMap<&'static str, String> = BTreeMap::new();
        let info = match self.page_info(page, false).await {
            Ok(info) => info,
            Err(err @ (Error::Auth(_) | Error::RateLimited(_) | Error::Cancelled)) => {
                return Err(err);
            }
            Err(err) => {
                tracing::debug!(page = %page, error = %err, "doc id discovery: cannot load the page");
                return Ok(found);
            }
        };
        let bundles: Vec<&String> = info.bundles.iter().take(MAX_BUNDLES).collect();
        if !bundles.is_empty() {
            tracing::info!(
                "looking up Instagram's current query ids in {} web-app scripts (about once a day)",
                bundles.len()
            );
        }
        let started = std::time::Instant::now();
        let mut failures_in_a_row = 0;
        for (number, url) in bundles.iter().enumerate() {
            match self.fetch_static(url).await {
                Ok(script) => {
                    failures_in_a_row = 0;
                    for (name, id) in extract_doc_ids(&script) {
                        found.entry(name).or_insert(id);
                    }
                }
                Err(Error::Cancelled) => return Err(Error::Cancelled),
                Err(err) => {
                    failures_in_a_row += 1;
                    tracing::debug!(bundle = %redact::redact_url(url), error = %err, "doc id discovery: skipping a bundle");
                }
            }
            if OPERATIONS.iter().all(|op| found.contains_key(op.name)) {
                break;
            }
            if failures_in_a_row >= DISCOVERY_MAX_FAILURES {
                tracing::warn!(
                    "cannot load Instagram's scripts (static.cdninstagram.com); using the built-in query ids"
                );
                break;
            }
            if started.elapsed() >= DISCOVERY_BUDGET {
                tracing::info!(
                    "the query id lookup is taking too long; using what it found and the built-in ids"
                );
                break;
            }
            if (number + 1) % DISCOVERY_PROGRESS_EVERY == 0 {
                tracing::info!("query id lookup: {}/{} scripts", number + 1, bundles.len());
            }
        }
        if found.is_empty() {
            tracing::debug!(page = %page, bundles = info.bundles.len(), "doc id discovery found nothing");
            return Ok(found);
        }
        tracing::info!(page = %page, found = found.len(), "discovered GraphQL doc ids");
        state.cache.record(&found, unix_now());
        save_cache(state).await;
        Ok(found)
    }

    /// `lsd` / `fb_dtsg` for a request on `page`: that page's, else the latest
    /// page's, else fetched from `page`. A page that cannot be loaded gives no
    /// tokens, unless it fails for a reason the GraphQL call would share.
    async fn tokens_for(&self, page: &str) -> Result<Tokens> {
        if let Some(info) = self.cached_page(page)
            && !info.tokens.is_empty()
        {
            return Ok(info.tokens.clone());
        }
        if let Some(tokens) = self.latest_tokens() {
            return Ok(tokens);
        }
        match self.page_info(page, false).await {
            Ok(info) => Ok(info.tokens.clone()),
            Err(err @ (Error::Auth(_) | Error::RateLimited(_) | Error::Cancelled)) => Err(err),
            Err(err) => {
                tracing::debug!(page = %page, error = %err, "no GraphQL tokens");
                Ok(Tokens::default())
            }
        }
    }

    /// One GraphQL POST.
    async fn graphql_request(
        &self,
        op: &'static Operation,
        doc_id: &str,
        variables: &str,
        page: &str,
        tokens: &Tokens,
    ) -> Result<Value> {
        let number = self
            .request_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let body = form_body(op, doc_id, variables, tokens, number);
        let mut extra = HeaderMap::new();
        extra.insert(
            HeaderName::from_static("x-fb-friendly-name"),
            HeaderValue::from_static(op.name),
        );
        if let Some(field) = op.root_field {
            extra.insert(
                HeaderName::from_static("x-root-field-name"),
                HeaderValue::from_static(field),
            );
        }
        if let Some(lsd) = tokens.lsd.as_deref()
            && let Ok(mut value) = HeaderValue::from_str(lsd)
        {
            value.set_sensitive(true);
            extra.insert(HeaderName::from_static("x-fb-lsd"), value);
        }
        tracing::debug!(query = op.name, doc_id = %doc_id, "GraphQL request");
        tracing::trace!(query = op.name, variables = %variables, "GraphQL variables");
        let request = ApiRequest {
            path: op.endpoint.path(),
            form: Some(&body),
            referer: Some(page),
            extra_headers: extra,
            graphql: true,
            options: CallOptions::default(),
        };
        self.api(&request).await
    }
}

/// Writes the doc id cache when it has a file.
async fn save_cache(state: &DocIdState) {
    let Some(path) = state.cache_path.clone() else {
        return;
    };
    let cache = state.cache.clone();
    let shown = path.display().to_string();
    match tokio::task::spawn_blocking(move || cache.save(&path)).await {
        Ok(Ok(())) => tracing::debug!(path = %shown, "saved the doc id cache"),
        Ok(Err(err)) => tracing::warn!(path = %shown, error = %err, "cannot save the doc id cache"),
        Err(err) => tracing::warn!(error = %err, "cannot save the doc id cache"),
    }
}

/// The form body of a GraphQL POST. Like gallery-dl's, minus its hard-coded
/// `av`, `__hs`, `__rev` and `__hsi`: these are the page's own values (when it
/// has them; `av` is the viewer's `IG_USER_EIMU`, as instaloader reads it).
fn form_body(
    op: &Operation,
    doc_id: &str,
    variables: &str,
    tokens: &Tokens,
    request_number: u32,
) -> String {
    let mut form = url::form_urlencoded::Serializer::new(String::new());
    if let Some(av) = &tokens.av {
        form.append_pair("av", av);
    }
    form.append_pair("__d", "www")
        .append_pair("__user", "0")
        .append_pair("__a", "1")
        .append_pair("__req", &base36(request_number));
    if let Some(hs) = &tokens.haste_session {
        form.append_pair("__hs", hs);
    }
    form.append_pair("dpr", "1")
        .append_pair("__ccg", "EXCELLENT");
    if let Some(rev) = &tokens.revision {
        form.append_pair("__rev", rev);
    }
    if let Some(hsi) = &tokens.hsi {
        form.append_pair("__hsi", hsi);
    }
    form.append_pair("__comet_req", "7");
    if let Some(dtsg) = &tokens.fb_dtsg {
        form.append_pair("fb_dtsg", dtsg)
            .append_pair("jazoest", &jazoest(dtsg));
    }
    if let Some(lsd) = &tokens.lsd {
        form.append_pair("lsd", lsd);
    }
    form.append_pair("fb_api_caller_class", "RelayModern")
        .append_pair("fb_api_req_friendly_name", op.name)
        .append_pair("server_timestamps", "true")
        .append_pair("variables", variables)
        .append_pair("doc_id", doc_id);
    form.finish()
}

/// The web app's `jazoest` checksum: `2` followed by the sum of the token's bytes.
fn jazoest(token: &str) -> String {
    let sum: u64 = token.bytes().map(u64::from).sum();
    format!("2{sum}")
}

/// `n` in base 36 (the `__req` counter).
fn base36(mut n: u32) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// The error text of a GraphQL answer, if it reports one.
fn error_text(json: &Value) -> Option<String> {
    if let Some(errors) = json.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let texts: Vec<String> = errors
            .iter()
            .take(3)
            .map(|error| {
                ["message", "summary", "description"]
                    .iter()
                    .find_map(|key| error.get(*key).and_then(Value::as_str))
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| "unspecified error".to_string())
            })
            .collect();
        return Some(texts.join("; "));
    }
    if let Some(code) = json.get("error").filter(|error| !error.is_null()) {
        let summary = ["errorSummary", "errorDescription"]
            .iter()
            .find_map(|key| json.get(*key).and_then(Value::as_str))
            .unwrap_or("error");
        return Some(format!("{summary} ({code})"));
    }
    if json.get("status").and_then(Value::as_str) == Some("fail") {
        let message = json
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Instagram reported a failure");
        return Some(message.to_string());
    }
    None
}

/// Whether an error text says the viewer may not see the data.
fn is_permission_problem(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    [
        "not authorized",
        "not_authorized",
        "permission",
        "private",
        "not allowed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// The usable result of a GraphQL answer, or why there is none.
fn interpret(json: &Value, op: &Operation) -> std::result::Result<Value, Problem> {
    let errors = error_text(json);
    let data = json.get("data").filter(|data| data.is_object());
    let result = match op.root_field {
        Some(field) => data.and_then(|data| data.get(field)),
        None => data,
    };
    match result {
        Some(value) if value.is_object() => {
            if let Some(errors) = &errors {
                tracing::debug!(
                    query = op.name,
                    "GraphQL answer with partial errors: {}",
                    redact::redact(errors)
                );
            }
            Ok(value.clone())
        }
        // An explicit `null` without errors: nothing to show.
        Some(Value::Null) if errors.is_none() => Ok(Value::Null),
        _ => {
            let text = errors.unwrap_or_else(|| {
                format!("the answer has no {}", op.root_field.unwrap_or("data"))
            });
            if is_permission_problem(&text) {
                Err(Problem::Permission(text))
            } else {
                Err(Problem::Outdated(text))
            }
        }
    }
}

/// `edges[].node` and `page_info` of a connection result; the connection may
/// sit one level down (the reels operation's `fetch__XDTUserDict`).
fn connection(result: Value) -> Connection {
    let has_edges = |value: &Value| value.get("edges").is_some_and(Value::is_array);
    let conn = if has_edges(&result) {
        result
    } else {
        match result
            .as_object()
            .and_then(|map| map.values().find(|value| has_edges(value)))
        {
            Some(inner) => inner.clone(),
            None => return Connection::default(),
        }
    };
    let nodes = conn
        .get("edges")
        .and_then(Value::as_array)
        .map(|edges| {
            edges
                .iter()
                .filter_map(|edge| edge.get("node").filter(|node| !node.is_null()).cloned())
                .collect()
        })
        .unwrap_or_default();
    let info = conn.get("page_info");
    Connection {
        nodes,
        has_next: info
            .and_then(|info| info.get("has_next_page"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        end_cursor: info
            .and_then(|info| info.get("end_cursor"))
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instagram::doc_ids::{HIGHLIGHTS_TRAY, POSTS, REELS};
    use serde_json::json;

    #[test]
    fn form_body_has_the_web_app_fields() {
        let tokens = Tokens {
            lsd: Some("L".into()),
            fb_dtsg: Some("AB".into()),
            revision: Some("123".into()),
            ..Tokens::default()
        };
        let body = form_body(&POSTS, "999", r#"{"a":1}"#, &tokens, 37);
        let pairs: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(pairs["doc_id"], "999");
        assert_eq!(pairs["variables"], r#"{"a":1}"#);
        assert_eq!(pairs["fb_api_req_friendly_name"], POSTS.name);
        assert_eq!(pairs["fb_api_caller_class"], "RelayModern");
        assert_eq!(pairs["server_timestamps"], "true");
        assert_eq!(pairs["lsd"], "L");
        assert_eq!(pairs["fb_dtsg"], "AB");
        assert_eq!(pairs["jazoest"], "2131");
        assert_eq!(pairs["__a"], "1");
        assert_eq!(pairs["__comet_req"], "7");
        assert_eq!(pairs["__req"], "11");
        assert_eq!(pairs["__rev"], "123");
        assert!(!pairs.contains_key("av"));
        assert!(!pairs.contains_key("__hsi"));
        let bare = form_body(&POSTS, "1", "{}", &Tokens::default(), 0);
        assert!(
            !bare.contains("lsd=") && !bare.contains("fb_dtsg="),
            "{bare}"
        );
    }

    #[test]
    fn base36_counts() {
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
    }

    #[test]
    fn interprets_answers() {
        let ok = json!({"data": {POSTS.root_field.unwrap(): {"edges": []}}, "status": "ok"});
        assert!(interpret(&ok, &POSTS).is_ok());
        let outdated = json!({"data": null, "errors": [{"message": "missing_required_variable_value", "severity": "CRITICAL"}]});
        assert_eq!(
            interpret(&outdated, &POSTS),
            Err(Problem::Outdated("missing_required_variable_value".into()))
        );
        let fail = json!({"message": "execution error", "status": "fail"});
        assert_eq!(
            interpret(&fail, &POSTS),
            Err(Problem::Outdated("execution error".into()))
        );
        let missing = json!({"data": {"other": {}}});
        assert!(
            matches!(interpret(&missing, &POSTS), Err(Problem::Outdated(t)) if t.contains("has no"))
        );
        let private = json!({"data": {POSTS.root_field.unwrap(): null}, "errors": [{"message": "Not authorized to view user"}]});
        assert!(matches!(
            interpret(&private, &POSTS),
            Err(Problem::Permission(_))
        ));
        let null = json!({"data": {POSTS.root_field.unwrap(): null}});
        assert_eq!(interpret(&null, &POSTS), Ok(Value::Null));
        let partial = json!({"data": {POSTS.root_field.unwrap(): {"edges": []}}, "errors": [{"message": "x"}]});
        assert!(interpret(&partial, &POSTS).is_ok());
        let tray = json!({"data": {"highlights": {"edges": []}}});
        assert_eq!(interpret(&tray, &HIGHLIGHTS_TRAY).unwrap(), tray["data"]);
    }

    #[test]
    fn connections_flat_and_nested() {
        let flat = connection(json!({
            "edges": [{"node": {"pk": 1}}, {"node": null}, {"cursor": "x"}],
            "page_info": {"has_next_page": true, "end_cursor": "c1"}
        }));
        assert_eq!(flat.nodes, vec![json!({"pk": 1})]);
        assert!(flat.has_next);
        assert_eq!(flat.end_cursor.as_deref(), Some("c1"));
        let nested = connection(json!({"xdt_api__v1__clips__user__connection_v2": {
            "edges": [{"node": {"media": {"code": "abc"}}}],
            "page_info": {"has_next_page": false, "end_cursor": ""}
        }}));
        assert_eq!(nested.nodes.len(), 1);
        assert!(!nested.has_next && nested.end_cursor.is_none());
        assert_eq!(connection(Value::Null), Connection::default());
        assert_eq!(REELS.root_field, Some("fetch__XDTUserDict"));
    }

    #[test]
    fn permission_texts() {
        assert!(is_permission_problem("Not authorized to view user"));
        assert!(!is_permission_problem("missing_required_variable_value"));
    }
}
