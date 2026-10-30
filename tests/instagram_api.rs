//! Instagram client + crawler against a mocked web app (wiremock). No real network.
//!
//! The mocks are shaped like what the web app (and gallery-dl 1.32.17) sees:
//! profile pages with `profile_id`, `__eqmc` tokens and `rsrc.php` bundles,
//! GraphQL connections with `page_info`, `media/{id}/info/`, `feed/reels_media/`,
//! `web_form_data` and instaloader's login probe. All ids, usernames, tokens and
//! URLs are made up; "CDN" and "static" URLs point at the mock server.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use igdl::cookies::CookieJar;
use igdl::error::Error;
use igdl::instagram::doc_ids::{self, CACHE_FILE, DocIdCache};
use igdl::instagram::{
    Client, ClientConfig, CrawlOptions, DEFAULT_USER_AGENT, Resolved, refresh_url, resolve,
};
use igdl::links::{ProfileSection, Target, shortcode_to_media_id};
use igdl::model::{Category, MediaKind};
use serde_json::{Value, json};
use wiremock::matchers::{body_string, header, method, path, path_regex, query_param};
use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate};

/// A distinctive fake session id that must never show up in errors or logs.
const SESSION: &str = "FAKESESSION-7f3a9c2e-DO-NOT-LEAK";
const CSRF: &str = "csrf-token-one";
const VIEWER_ID: &str = "999";
const USER_ID: &str = "1001";
const POST_CODE: &str = "CfakePost01";
const CAROUSEL_CODE: &str = "CfakeCarou1";
const REEL_CODE: &str = "CfakeReel01";
/// Page tokens (`fb_dtsg` is session-bound: it must never be logged).
const LSD: &str = "FakeLsdToken";
const DTSG: &str = "FAKE-DTSG-SECRET:77";
/// Private fields of `web_form_data` that must never be logged or kept.
const EMAIL: &str = "private.person@example.invalid";
const PHONE: &str = "+1 555 0100 777";
const BIRTHDAY: &str = "1990-02-03";

const POSTS_ROOT: &str = "xdt_api__v1__feed__user_timeline_graphql_connection";
const TAGGED_ROOT: &str = "xdt_api__v1__usertags__user_id__feed_connection";
const STORIES_ROOT: &str = "xdt_api__v1__feed__reels_media__connection";

fn config(server: &MockServer) -> ClientConfig {
    ClientConfig {
        api_base: server.uri(),
        user_agent: "igdl-test/1.0".into(),
        api_delay: Duration::ZERO,
        rate_limit_retries: 1,
        rate_limit_wait: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
        transient_retries: 2,
        transient_wait: Duration::from_millis(1),
        static_base: server.uri(),
        cache_dir: None,
        doc_ids: BTreeMap::new(),
    }
}

fn jar() -> CookieJar {
    CookieJar::from_pairs([
        ("sessionid", SESSION),
        ("csrftoken", CSRF),
        ("ds_user_id", VIEWER_ID),
    ])
}

fn client(server: &MockServer) -> Client {
    Client::new(config(server), Some(jar())).unwrap()
}

fn never_archived(_: &str) -> bool {
    false
}

fn opts() -> CrawlOptions {
    CrawlOptions::default()
}

fn post_target(code: &str, index: Option<u32>) -> Target {
    Target::Post {
        shortcode: code.into(),
        index,
    }
}

fn profile_target(section: Option<ProfileSection>) -> Target {
    Target::Profile {
        username: "alice".into(),
        section,
    }
}

async fn run(client: &Client, target: &Target, options: &CrawlOptions) -> igdl::Result<Resolved> {
    resolve(client, target, options, &never_archived).await
}

fn media_info_path(code: &str) -> String {
    format!(
        "/api/v1/media/{}/info/",
        shortcode_to_media_id(code).unwrap()
    )
}

fn image_item(cdn: &str, pk: u64, code: &str, taken_at: i64) -> Value {
    json!({
        "pk": pk,
        "id": format!("{pk}_{USER_ID}"),
        "code": code,
        "media_type": 1,
        "taken_at": taken_at,
        "user": {"pk": 1001, "username": "alice"},
        "caption": {"text": format!("caption {pk}")},
        "image_versions2": {"candidates": [
            {"url": format!("{cdn}/cdn/{pk}_s.jpg?oe=6700AAAA&_nc_sid=fake"), "width": 320, "height": 400},
            {"url": format!("{cdn}/cdn/{pk}_l.jpg?oe=6700AAAA&_nc_sid=fake"), "width": 1080, "height": 1350},
            {"url": format!("{cdn}/cdn/{pk}_m.jpg?oe=6700AAAA&_nc_sid=fake"), "width": 640, "height": 800}
        ]}
    })
}

fn video_item(cdn: &str, pk: u64, code: &str, taken_at: i64) -> Value {
    json!({
        "pk": pk.to_string(),
        "id": format!("{pk}_{USER_ID}"),
        "code": code,
        "media_type": 2,
        "taken_at": taken_at,
        "user": {"pk": "1001", "username": "alice"},
        "caption": null,
        "image_versions2": {"candidates": [
            {"url": format!("{cdn}/cdn/{pk}_cover.jpg"), "width": 1080, "height": 1920}
        ]},
        "video_versions": [
            {"url": format!("{cdn}/cdn/{pk}_480.mp4?oe=6700AAAA"), "width": 480, "height": 854},
            {"url": format!("{cdn}/cdn/{pk}_720.mp4?oe=6700AAAA"), "width": 720, "height": 1280}
        ]
    })
}

fn clip_item(cdn: &str, pk: u64) -> Value {
    let mut item = video_item(cdn, pk, &format!("Reel{pk}"), 1000 - pk as i64);
    item["product_type"] = json!("clips");
    item
}

fn carousel_item(cdn: &str) -> Value {
    let mut first = image_item(cdn, 501, CAROUSEL_CODE, 0);
    first["media_type"] = json!(1);
    let second = video_item(cdn, 502, CAROUSEL_CODE, 0);
    let third = image_item(cdn, 503, CAROUSEL_CODE, 0);
    json!({
        "pk": 500,
        "id": format!("500_{USER_ID}"),
        "code": CAROUSEL_CODE,
        "media_type": 8,
        "taken_at": 1_700_000_000,
        "user": {"pk": 1001, "username": "alice"},
        "caption": {"text": "three things"},
        "carousel_media": [first, second, third]
    })
}

fn pinned(mut item: Value) -> Value {
    item["timeline_pinned_user_ids"] = json!([1001]);
    item
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
}

fn html(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/html; charset=utf-8")
}

async fn mount_media(server: &MockServer, code: &str, item: Value) {
    Mock::given(method("GET"))
        .and(path(media_info_path(code)))
        .respond_with(ok(
            json!({"items": [item], "num_results": 1, "status": "ok"}),
        ))
        .mount(server)
        .await;
}

/// A profile page like the web app's: `__eqmc` tokens, the profile id, and
/// `<link rel="preload">` / `<script src>` tags for `bundles` under `/rsrc.php/`.
fn profile_html(server: &MockServer, username: &str, id: &str, private: Option<bool>) -> String {
    profile_html_with(server, username, id, private, &[])
}

fn profile_html_with(
    server: &MockServer,
    username: &str,
    id: &str,
    private: Option<bool>,
    bundles: &[&str],
) -> String {
    let base = server.uri();
    let links: String = bundles
        .iter()
        .enumerate()
        .map(|(n, bundle)| {
            if n % 2 == 0 {
                format!(
                    "<link rel=\"preload\" href=\"{base}/rsrc.php/{bundle}\" as=\"script\" crossorigin=\"anonymous\" />\n"
                )
            } else {
                format!("<script src=\"{base}/rsrc.php/{bundle}\" async=\"1\"></script>\n")
            }
        })
        .collect();
    let private = private
        .map(|p| format!(",\"is_private\":{p}"))
        .unwrap_or_default();
    format!(
        r#"<!DOCTYPE html><html lang="en"><head><meta charset="utf-8" />
<link rel="stylesheet" href="{base}/rsrc.php/v4/yS/l/Style.css" />
{links}<script type="application/json" id="__eqmc" nonce="n">{{"u":"\/ajax\/qm\/?__a=1&__user=0&__hs=20000.HYP%3Ainstagram_web_pkg.2.1...0&__rev=1000001&__hsi=7000000000000000001&__comet_req=7","e":"x","f":"{DTSG}","l":"{LSD}"}}</script>
</head><body><script type="application/json" data-sjs>{{"require":[["ScheduledServerJS","handle",null,[{{"__bbox":{{"require":[["PolarisProfilePageContentQuery",{{"params":{{"username":"{username}"}}}},{{"profile_id":"{id}"{private}}}]]}}}}]]]}}</script></body></html>"#
    )
}

async fn mount_profile(server: &MockServer, username: &str, id: &str, private: Option<bool>) {
    Mock::given(method("GET"))
        .and(path(format!("/{username}/")))
        .respond_with(html(profile_html(server, username, id, private)))
        .mount(server)
        .await;
}

/// A JS bundle defining relay operations (`name → doc id`).
fn bundle_js(entries: &[(&str, &str)]) -> String {
    let mut js = String::from(
        "/*FB_PKG_DELIM*/\n__d(\"PolarisSomething\",[],(function(a,b,c,d,e,f){e.exports={}}),null);\n",
    );
    for (name, id) in entries {
        js.push_str(&format!(
            "__d(\"{name}_instagramRelayOperation\",[],(function(a,b,c,d,e,f){{e.exports=\"{id}\"}}),null);\n"
        ));
    }
    js
}

async fn mount_bundle(server: &MockServer, bundle: &str, entries: &[(&str, &str)]) {
    Mock::given(method("GET"))
        .and(path(format!("/rsrc.php/{bundle}")))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(bundle_js(entries), "application/javascript"),
        )
        .mount(server)
        .await;
}

fn header_of(request: &Request, name: &str) -> String {
    request
        .headers
        .get(name)
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default()
}

fn form_of(request: &Request) -> HashMap<String, String> {
    url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect()
}

fn variables_of(request: &Request) -> Value {
    form_of(request)
        .get("variables")
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(Value::Null)
}

/// Matches a GraphQL POST by operation name, and optionally doc id and `after`.
#[derive(Clone)]
struct Gql {
    name: &'static str,
    doc_id: Option<String>,
    after: Option<Value>,
}

fn gql(op: &doc_ids::Operation) -> Gql {
    Gql {
        name: op.name,
        doc_id: None,
        after: None,
    }
}

impl Gql {
    fn doc(mut self, id: &str) -> Self {
        self.doc_id = Some(id.to_string());
        self
    }
    fn after(mut self, after: Value) -> Self {
        self.after = Some(after);
        self
    }
}

impl Match for Gql {
    fn matches(&self, request: &Request) -> bool {
        let form = form_of(request);
        if form.get("fb_api_req_friendly_name").map(String::as_str) != Some(self.name) {
            return false;
        }
        if let Some(id) = &self.doc_id
            && form.get("doc_id") != Some(id)
        {
            return false;
        }
        if let Some(after) = &self.after
            && variables_of(request).get("after") != Some(after)
        {
            return false;
        }
        true
    }
}

fn is_graphql(request: &Request, op: &doc_ids::Operation) -> bool {
    form_of(request)
        .get("fb_api_req_friendly_name")
        .map(String::as_str)
        == Some(op.name)
}

/// A GraphQL connection answer: `data.<root>.{edges[].node, page_info}`.
fn connection(root: &str, nodes: Vec<Value>, next: Option<&str>) -> Value {
    let edges: Vec<Value> = nodes
        .into_iter()
        .enumerate()
        .map(|(n, node)| json!({"node": node, "cursor": format!("edge-{n}")}))
        .collect();
    json!({
        "data": {root: {
            "edges": edges,
            "page_info": {"has_next_page": next.is_some(), "end_cursor": next, "has_previous_page": false}
        }},
        "extensions": {"is_final": true},
        "status": "ok"
    })
}

fn posts_page(nodes: Vec<Value>, next: Option<&str>) -> ResponseTemplate {
    ok(connection(POSTS_ROOT, nodes, next))
}

async fn mount_posts(server: &MockServer, after: Value, nodes: Vec<Value>, next: Option<&str>) {
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(after))
        .respond_with(posts_page(nodes, next))
        .mount(server)
        .await;
}

fn keys(resolved: &Resolved) -> Vec<&str> {
    resolved.files.iter().map(|f| f.key.as_str()).collect()
}

async fn requests_to(server: &MockServer, prefix: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().starts_with(prefix))
        .collect()
}

/// Captures every log line (all levels) of the current thread.
#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuffer {
    type Writer = LogBuffer;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// The log of every test in this binary (`igdl` targets, all levels). A
/// global subscriber: thread-local ones miss events when tests run in parallel.
/// Every test shares the same fake secrets, so the whole log must be free of them.
fn capture_logs() -> LogBuffer {
    static LOGS: OnceLock<LogBuffer> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buffer = LogBuffer::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buffer.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::new("igdl=trace"))
            .with_ansi(false)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        buffer
    })
    .clone()
}

fn assert_no_secrets(text: &str) {
    for secret in [SESSION, CSRF, DTSG, EMAIL, PHONE, BIRTHDAY] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
}

// ---------------------------------------------------------------- client basics

#[tokio::test]
async fn api_headers_cookie_and_csrf_are_sent_and_single_image_resolves() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_media(
        &server,
        POST_CODE,
        image_item(&cdn, 101, POST_CODE, 1_700_000_000),
    )
    .await;
    let client = client(&server);
    assert!(client.is_authenticated());

    let resolved = run(&client, &post_target(POST_CODE, None), &opts())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let request = &requests[0];
    assert_eq!(header_of(request, "x-ig-app-id"), "936619743392459");
    assert_eq!(header_of(request, "x-asbd-id"), "359341");
    assert_eq!(header_of(request, "x-ig-www-claim"), "0");
    assert_eq!(header_of(request, "x-requested-with"), "XMLHttpRequest");
    assert_eq!(header_of(request, "x-csrftoken"), CSRF);
    assert_eq!(header_of(request, "user-agent"), "igdl-test/1.0");
    assert_eq!(header_of(request, "origin"), server.uri());
    assert_eq!(header_of(request, "referer"), format!("{}/", server.uri()));
    assert_eq!(header_of(request, "accept"), "*/*");
    assert_eq!(header_of(request, "sec-fetch-mode"), "cors");
    assert_eq!(
        header_of(request, "cookie"),
        format!("sessionid={SESSION}; csrftoken={CSRF}; ds_user_id={VIEWER_ID}")
    );

    assert_eq!(resolved.name, format!("alice · post {POST_CODE}"));
    assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
    assert_eq!(resolved.files.len(), 1);
    let file = &resolved.files[0];
    assert_eq!(file.key, "101");
    assert_eq!(file.post_id, "101");
    assert_eq!(file.shortcode.as_deref(), Some(POST_CODE));
    assert_eq!((file.index, file.count), (1, 1));
    assert_eq!(file.kind, MediaKind::Image);
    assert_eq!(file.category, Category::Post);
    assert_eq!(
        file.url,
        format!("{cdn}/cdn/101_l.jpg?oe=6700AAAA&_nc_sid=fake")
    );
    assert_eq!((file.width, file.height), (Some(1080), Some(1350)));
    assert_eq!(file.ext, "jpg");
    assert_eq!(file.owner, "alice");
    assert_eq!(file.owner_id.as_deref(), Some("1001"));
    assert_eq!(file.taken_at, 1_700_000_000);
    assert_eq!(file.caption.as_deref(), Some("caption 101"));
    assert_eq!(file.highlight, None);
}

#[tokio::test]
async fn default_user_agent_is_chrome_150_on_windows() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ok(json!({"status": "ok"})))
        .mount(&server)
        .await;
    let client = Client::new(
        ClientConfig {
            user_agent: DEFAULT_USER_AGENT.to_string(),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    client.get_json("/api/v1/x/").await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        header_of(&requests[0], "user-agent"),
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36"
    );
}

#[tokio::test]
async fn set_cookie_rotates_csrftoken_for_the_next_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/first/"))
        .respond_with(
            ok(json!({"status": "ok"}))
                .append_header("set-cookie", "csrftoken=csrf-token-two; Path=/; Secure")
                .append_header("set-cookie", "rur=\"FAKE\\054rur\"; Path=/; HttpOnly"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/second/"))
        .and(header("x-csrftoken", "csrf-token-two"))
        .respond_with(ok(json!({"status": "ok", "second": true})))
        .expect(1)
        .mount(&server)
        .await;
    let client = client(&server);
    client.get_json("/api/v1/first/").await.unwrap();
    let value = client.get_json("/api/v1/second/").await.unwrap();
    assert_eq!(value["second"], json!(true));
    let requests = server.received_requests().await.unwrap();
    let cookie = header_of(&requests[1], "cookie");
    assert!(cookie.contains("csrftoken=csrf-token-two"), "{cookie}");
    assert!(!cookie.contains(CSRF));
}

#[tokio::test]
async fn www_claim_is_adopted_from_responses() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/first/"))
        .respond_with(
            ok(json!({"status": "ok"})).insert_header("x-ig-set-www-claim", "hmac.AR-fake-claim-1"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(
            html(profile_html(&server, "alice", USER_ID, None))
                .insert_header("x-ig-set-www-claim", "hmac.AR-fake-claim-2"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/second/"))
        .respond_with(ok(json!({"status": "ok"})))
        .mount(&server)
        .await;
    let client = client(&server);
    assert_eq!(client.www_claim(), "0");
    client.get_json("/api/v1/first/").await.unwrap();
    assert_eq!(client.www_claim(), "hmac.AR-fake-claim-1");
    client.get_json("/api/v1/second/").await.unwrap();
    client.user_by_username("alice").await.unwrap();
    assert_eq!(client.www_claim(), "hmac.AR-fake-claim-2");
    client.get_json("/api/v1/second/").await.unwrap();
    let claims: Vec<String> = requests_to(&server, "/api/v1/")
        .await
        .iter()
        .map(|r| header_of(r, "x-ig-www-claim"))
        .collect();
    assert_eq!(
        claims,
        ["0", "hmac.AR-fake-claim-1", "hmac.AR-fake-claim-2"]
    );
}

#[tokio::test]
async fn post_form_sends_urlencoded_body_with_csrf() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/echo/"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(header("x-csrftoken", CSRF))
        .and(header("origin", server.uri().as_str()))
        .and(body_string("a=1&b=x+y%26z"))
        .respond_with(ok(json!({"status": "ok"})))
        .expect(1)
        .mount(&server)
        .await;
    client(&server)
        .post_form("/api/v1/echo/", &[("a", "1"), ("b", "x y&z")])
        .await
        .unwrap();
}

#[tokio::test]
async fn api_calls_are_spaced_but_pages_are_not() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html(&server, "alice", USER_ID, None)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ok(json!({"status": "ok"})))
        .mount(&server)
        .await;
    let client = Client::new(
        ClientConfig {
            api_delay: Duration::from_millis(250),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    let started = Instant::now();
    client.get_json("/api/v1/a/").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "the first call is not delayed"
    );
    client.user_by_username("alice").await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(250),
        "page fetches are exempt from the API delay"
    );
    client.get_json("/api/v1/b/").await.unwrap();
    // 250 ms delay plus 0–100 % jitter.
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(250), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(2000), "{elapsed:?}");
}

// ---------------------------------------------------------------- posts

#[tokio::test]
async fn carousel_yields_one_file_per_child_with_best_renditions() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_media(&server, CAROUSEL_CODE, carousel_item(&cdn)).await;
    let resolved = run(&client(&server), &post_target(CAROUSEL_CODE, None), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["501", "502", "503"]);
    let summary: Vec<_> = resolved
        .files
        .iter()
        .map(|f| (f.index, f.count, f.kind, f.post_id.as_str(), f.ext.as_str()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (1, 3, MediaKind::Image, "500", "jpg"),
            (2, 3, MediaKind::Video, "500", "mp4"),
            (3, 3, MediaKind::Image, "500", "jpg"),
        ]
    );
    assert_eq!(
        resolved.files[1].url,
        format!("{cdn}/cdn/502_720.mp4?oe=6700AAAA")
    );
    assert!(
        resolved
            .files
            .iter()
            .all(|f| f.caption.as_deref() == Some("three things")
                && f.shortcode.as_deref() == Some(CAROUSEL_CODE))
    );
}

#[tokio::test]
async fn img_index_selects_one_carousel_item() {
    let server = MockServer::start().await;
    mount_media(&server, CAROUSEL_CODE, carousel_item(&server.uri())).await;
    let client = client(&server);
    let resolved = run(&client, &post_target(CAROUSEL_CODE, Some(2)), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["502"]);
    let err = run(&client, &post_target(CAROUSEL_CODE, Some(9)), &opts())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

#[tokio::test]
async fn only_filter_drops_files_with_a_note() {
    let server = MockServer::start().await;
    mount_media(&server, CAROUSEL_CODE, carousel_item(&server.uri())).await;
    let options = CrawlOptions {
        only: Some(MediaKind::Video),
        ..opts()
    };
    let resolved = run(
        &client(&server),
        &post_target(CAROUSEL_CODE, None),
        &options,
    )
    .await
    .unwrap();
    assert_eq!(keys(&resolved), vec!["502"]);
    assert!(
        resolved
            .notes
            .iter()
            .any(|n| n.contains("left out 2 image")),
        "{:?}",
        resolved.notes
    );
}

#[tokio::test]
async fn reel_post_is_categorised_as_reel() {
    let server = MockServer::start().await;
    let mut reel = video_item(&server.uri(), 301, REEL_CODE, 1_700_000_000);
    reel["product_type"] = json!("clips");
    mount_media(&server, REEL_CODE, reel).await;
    let resolved = run(&client(&server), &post_target(REEL_CODE, None), &opts())
        .await
        .unwrap();
    assert_eq!(resolved.files.len(), 1);
    assert_eq!(resolved.files[0].category, Category::Reel);
    assert_eq!(resolved.files[0].kind, MediaKind::Video);
}

#[tokio::test]
async fn missing_post_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(
                json!({"message": "Media not found or unavailable", "status": "fail"}),
            ),
        )
        .mount(&server)
        .await;
    let err = run(&client(&server), &post_target(POST_CODE, None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::NotFound(m) if m.contains(POST_CODE)),
        "{err:?}"
    );
}

#[tokio::test]
async fn media_info_redirect_to_home_or_login_is_auth() {
    for location in [
        "https://www.instagram.com/",
        "/",
        "/accounts/login/?next=/p/x/",
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(media_info_path(POST_CODE)))
            .respond_with(ResponseTemplate::new(302).insert_header("location", location))
            .expect(1)
            .mount(&server)
            .await;
        let err = run(&client(&server), &post_target(POST_CODE, None), &opts())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, Error::Auth(m) if m.contains("not logged in")),
            "{location}: {err:?}"
        );
    }
    // Elsewhere a redirect to the home page is not a verdict on the cookies.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/"))
        .mount(&server)
        .await;
    let err = client(&server).get_json("/api/v1/x/").await.unwrap_err();
    assert!(
        matches!(&err, Error::Http { status: 302, message } if message.contains("home page")),
        "{err:?}"
    );
}

// ---------------------------------------------------------------- profile pages

#[tokio::test]
async fn profile_page_is_fetched_like_a_browser_and_parsed() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, Some(false)).await;
    let user = client(&server).user_by_username("@Alice").await.unwrap();
    assert_eq!(user.id, USER_ID);
    assert_eq!(user.username, "alice");
    assert!(!user.is_private);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let page = &requests[0];
    assert_eq!(page.url.path(), "/alice/");
    assert!(header_of(page, "accept").starts_with("text/html,"));
    assert_eq!(header_of(page, "sec-fetch-dest"), "document");
    assert_eq!(header_of(page, "sec-fetch-mode"), "navigate");
    assert_eq!(header_of(page, "sec-fetch-site"), "none");
    assert_eq!(header_of(page, "sec-fetch-user"), "?1");
    assert!(header_of(page, "cookie").contains(SESSION));
    assert!(page.headers.get("x-ig-app-id").is_none());
    assert!(page.headers.get("x-csrftoken").is_none());

    let server = MockServer::start().await;
    mount_profile(&server, "bob", "2002", Some(true)).await;
    let bob = client(&server).user_by_username("bob").await.unwrap();
    assert_eq!((bob.id.as_str(), bob.is_private), ("2002", true));
}

#[tokio::test]
async fn profile_page_failures() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/ghost/"))
        .respond_with(ResponseTemplate::new(404).set_body_raw("<html>Sorry</html>", "text/html"))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gone/"))
        .respond_with(html(
            "<html><body>Sorry, this page isn't available.</body></html>".into(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/locked/"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            "https://www.instagram.com/accounts/login/?next=%2Flocked%2F",
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/wall/"))
        .respond_with(html(
            "<html><form id=\"loginForm\" action=\"/accounts/login/ajax/\"></form></html>".into(),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/confirm/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/challenge/?next=/"))
        .mount(&server)
        .await;
    let client = client(&server);
    for name in ["ghost", "gone", "bad!name"] {
        let err = client.user_by_username(name).await.unwrap_err();
        assert!(
            matches!(&err, Error::NotFound(m) if m == &format!("user @{name}")),
            "{name}: {err:?}"
        );
    }
    for name in ["locked", "wall"] {
        let err = client.user_by_username(name).await.unwrap_err();
        assert!(
            matches!(&err, Error::Auth(m) if m.contains("not logged in")),
            "{name}: {err:?}"
        );
    }
    let err = client.user_by_username("confirm").await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("confirm this login")),
        "{err:?}"
    );
    assert!(requests_to(&server, "/bad").await.is_empty());
}

// ---------------------------------------------------------------- posts grid (GraphQL)

#[tokio::test]
async fn graphql_requests_look_like_the_web_app() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, Some(false)).await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 1, "C1", 10)],
        None,
    )
    .await;
    let resolved = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["1"]);

    let posts = requests_to(&server, "/graphql/query").await;
    assert_eq!(posts.len(), 1);
    let request = &posts[0];
    assert_eq!(request.method.as_str(), "POST");
    assert_eq!(
        header_of(request, "x-fb-friendly-name"),
        doc_ids::POSTS.name
    );
    assert_eq!(header_of(request, "x-root-field-name"), POSTS_ROOT);
    assert_eq!(header_of(request, "x-fb-lsd"), LSD);
    assert_eq!(header_of(request, "x-csrftoken"), CSRF);
    assert_eq!(header_of(request, "x-ig-app-id"), "936619743392459");
    assert_eq!(header_of(request, "x-asbd-id"), "359341");
    assert_eq!(header_of(request, "origin"), server.uri());
    assert_eq!(
        header_of(request, "referer"),
        format!("{}/alice/", server.uri())
    );
    assert_eq!(
        header_of(request, "content-type"),
        "application/x-www-form-urlencoded"
    );
    assert!(header_of(request, "cookie").contains(SESSION));
    assert!(request.headers.get("x-requested-with").is_none());

    let form = form_of(request);
    assert_eq!(form["doc_id"], doc_ids::POSTS.default_doc_id);
    assert_eq!(form["fb_api_req_friendly_name"], doc_ids::POSTS.name);
    assert_eq!(form["fb_api_caller_class"], "RelayModern");
    assert_eq!(form["server_timestamps"], "true");
    assert_eq!(form["lsd"], LSD);
    assert_eq!(form["fb_dtsg"], DTSG);
    let jazoest: u64 = DTSG.bytes().map(u64::from).sum();
    assert_eq!(form["jazoest"], format!("2{jazoest}"));
    assert_eq!(form["__a"], "1");
    assert_eq!(form["__comet_req"], "7");
    assert_eq!(form["__rev"], "1000001");
    assert_eq!(form["__hsi"], "7000000000000000001");
    assert_eq!(form["__hs"], "20000.HYP:instagram_web_pkg.2.1...0");
    assert!(!form.contains_key("av"), "no hard-coded viewer id");
    let variables = variables_of(request);
    assert_eq!(variables["username"], "alice");
    assert_eq!(variables["first"], 12);
    assert_eq!(variables["data"]["count"], 12);
    assert_eq!(variables["after"], Value::Null);
}

#[tokio::test]
async fn profile_posts_paginate_and_respect_limit() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    let page = |first: u64| -> Vec<Value> {
        (first..first + 3)
            .map(|pk| image_item(&cdn, pk, &format!("Code{pk}"), 1_700_000_000 - pk as i64))
            .collect()
    };
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(Value::Null))
        .respond_with(posts_page(page(1), Some("cursor-1")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(json!("cursor-1")))
        .respond_with(posts_page(page(4), Some("cursor-2")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(json!("cursor-2")))
        .respond_with(posts_page(page(7), None))
        .expect(0)
        .mount(&server)
        .await;
    let options = CrawlOptions {
        limit: Some(5),
        ..opts()
    };
    let resolved = run(&client(&server), &profile_target(None), &options)
        .await
        .unwrap();
    assert_eq!(resolved.name, "alice · posts");
    assert_eq!(keys(&resolved), vec!["1", "2", "3", "4", "5"]);
    assert!(resolved.files.iter().all(|f| f.category == Category::Post));
    assert!(
        resolved.notes.iter().any(|n| n.contains("limited to 5")),
        "{:?}",
        resolved.notes
    );
}

#[tokio::test]
async fn profile_posts_without_limit_read_every_page() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 1, "C1", 10)],
        Some("n1"),
    )
    .await;
    mount_posts(
        &server,
        json!("n1"),
        vec![video_item(&cdn, 2, "C2", 9)],
        None,
    )
    .await;
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Posts)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(keys(&resolved), vec!["1", "2"]);
    assert!(resolved.notes.is_empty(), "{:?}", resolved.notes);
}

#[tokio::test]
async fn since_and_until_skip_pinned_and_stop_at_older_posts() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_posts(
        &server,
        Value::Null,
        vec![
            pinned(image_item(&cdn, 10, "Pinned10", 100)), // old but pinned: skipped, no stop
            image_item(&cdn, 11, "New11", 5000),           // newer than until
            image_item(&cdn, 12, "In12", 3000),
            image_item(&cdn, 13, "In13", 2000),
        ],
        Some("p2"),
    )
    .await;
    mount_posts(
        &server,
        json!("p2"),
        vec![
            image_item(&cdn, 14, "In14", 1500),
            image_item(&cdn, 15, "Old15", 500), // older than since: stop here
            image_item(&cdn, 16, "Old16", 400),
        ],
        Some("p3"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(json!("p3")))
        .respond_with(posts_page(vec![], None))
        .expect(0)
        .mount(&server)
        .await;
    let options = CrawlOptions {
        since: Some(1000),
        until: Some(4000),
        ..opts()
    };
    let resolved = run(&client(&server), &profile_target(None), &options)
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["12", "13", "14"]);
}

#[tokio::test]
async fn stop_at_archived_stops_at_first_known_non_pinned_post() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_posts(
        &server,
        Value::Null,
        vec![
            pinned(image_item(&cdn, 50, "Pin50", 900)), // archived but pinned
            image_item(&cdn, 51, "New51", 800),
            image_item(&cdn, 52, "Old52", 700), // archived: stop
            image_item(&cdn, 53, "Old53", 600),
        ],
        Some("next"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(json!("next")))
        .respond_with(posts_page(vec![], None))
        .expect(0)
        .mount(&server)
        .await;
    let options = CrawlOptions {
        stop_at_archived: true,
        ..opts()
    };
    let archived = |key: &str| matches!(key, "50" | "52" | "53");
    let resolved = resolve(&client(&server), &profile_target(None), &options, &archived)
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["50", "51"]);
    assert!(
        resolved
            .notes
            .iter()
            .any(|n| n.contains("stopped at Old52")),
        "{:?}",
        resolved.notes
    );
}

#[tokio::test]
async fn private_profile_with_nothing_visible_is_private() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, Some(true)).await;
    mount_posts(&server, Value::Null, vec![], None).await;
    let err = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Private(m) if m.contains("@alice") && m.contains("don't follow")),
        "{err:?}"
    );

    // A permission error inside the GraphQL answer.
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .respond_with(ok(json!({
            "data": {POSTS_ROOT: null},
            "errors": [{"message": "Not authorized to view user", "severity": "CRITICAL"}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let err = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Private(m) if m.contains("@alice") && m.contains("Not authorized")),
        "{err:?}"
    );
}

#[tokio::test]
async fn private_profile_followed_is_crawled() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, Some(true)).await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 7, "C7", 1)],
        None,
    )
    .await;
    let resolved = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["7"]);
}

#[tokio::test]
async fn public_profile_without_posts_is_empty() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, Some(false)).await;
    mount_posts(&server, Value::Null, vec![], None).await;
    let resolved = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    assert!(resolved.files.is_empty());
}

// ---------------------------------------------------------------- reels & tagged

#[tokio::test]
async fn reels_come_from_the_reels_tab_then_media_info_and_dedupe_against_posts() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    // The posts grid also contains reel 201: no media/info call for it.
    mount_posts(
        &server,
        Value::Null,
        vec![clip_item(&cdn, 201), image_item(&cdn, 1, "Img1", 900)],
        None,
    )
    .await;
    let reels_page = |codes: &[&str], next: Option<&str>| {
        let edges: Vec<Value> = codes
            .iter()
            .map(|code| json!({"node": {"media": {"code": code, "pk": "x", "__typename": "XDTMediaDict"}}}))
            .collect();
        ok(json!({"data": {"fetch__XDTUserDict": {
            "id": USER_ID,
            "xdt_api__v1__clips__user__connection_v2": {
                "edges": edges,
                "page_info": {"has_next_page": next.is_some(), "end_cursor": next}
            }
        }}, "status": "ok"}))
    };
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::REELS).after(Value::Null))
        .respond_with(reels_page(&["Reel201", "Reel202"], Some("reels-2")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::REELS).after(json!("reels-2")))
        .respond_with(reels_page(&["Reel203"], None))
        .expect(1)
        .mount(&server)
        .await;
    for pk in [202, 203] {
        Mock::given(method("GET"))
            .and(path(media_info_path(&format!("Reel{pk}"))))
            .respond_with(ok(json!({"items": [clip_item(&cdn, pk)], "status": "ok"})))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path(media_info_path("Reel201")))
        .respond_with(ok(json!({"items": [clip_item(&cdn, 201)], "status": "ok"})))
        .expect(0)
        .mount(&server)
        .await;
    let options = CrawlOptions {
        sections: vec![ProfileSection::Posts, ProfileSection::Reels],
        ..opts()
    };
    let resolved = run(&client(&server), &profile_target(None), &options)
        .await
        .unwrap();
    assert_eq!(resolved.name, "alice · posts, reels");
    assert_eq!(keys(&resolved), vec!["201", "1", "202", "203"]);
    assert_eq!(resolved.files[0].category, Category::Reel);
    assert_eq!(resolved.files[1].category, Category::Post);
    assert!(
        resolved.files[2..]
            .iter()
            .all(|f| f.category == Category::Reel)
    );
    let reels: Vec<Request> = requests_to(&server, "/graphql/query")
        .await
        .into_iter()
        .filter(|r| is_graphql(r, &doc_ids::REELS))
        .collect();
    assert_eq!(
        header_of(&reels[0], "referer"),
        format!("{}/alice/reels/", server.uri())
    );
    let variables = variables_of(&reels[0]);
    assert_eq!(variables["data"]["target_user_id"], USER_ID);
    assert_eq!(variables["id"], USER_ID);
}

#[tokio::test]
async fn reels_limit_stops_fetching_media_info() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    let edges: Vec<Value> = (301..=305)
        .map(|pk| json!({"node": {"media": {"code": format!("Reel{pk}")}}}))
        .collect();
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::REELS))
        .respond_with(ok(json!({"data": {"fetch__XDTUserDict": {"conn": {
            "edges": edges, "page_info": {"has_next_page": true, "end_cursor": "more"}
        }}}})))
        .expect(1)
        .mount(&server)
        .await;
    for pk in 301..=305 {
        Mock::given(method("GET"))
            .and(path(media_info_path(&format!("Reel{pk}"))))
            .respond_with(ok(json!({"items": [clip_item(&cdn, pk)], "status": "ok"})))
            .expect(if pk <= 302 { 1 } else { 0 })
            .mount(&server)
            .await;
    }
    let options = CrawlOptions {
        limit: Some(2),
        ..opts()
    };
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Reels)),
        &options,
    )
    .await
    .unwrap();
    assert_eq!(keys(&resolved), vec!["301", "302"]);
    assert!(
        resolved
            .notes
            .iter()
            .any(|n| n.contains("reels: limited to 2")),
        "{:?}",
        resolved.notes
    );
}

#[tokio::test]
async fn tagged_posts_come_from_the_tagged_tab_then_media_info() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::TAGGED).after(Value::Null))
        .respond_with(ok(connection(
            TAGGED_ROOT,
            vec![
                json!({"code": "Tag401", "pk": "401", "taken_at": 1000}),
                json!({"code": "Gone402"}),
            ],
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let mut item = image_item(&cdn, 401, "Tag401", 1000);
    item["user"] = json!({"pk": 2002, "username": "carol"});
    mount_media(&server, "Tag401", item).await;
    Mock::given(method("GET"))
        .and(path(media_info_path("Gone402")))
        .respond_with(
            ResponseTemplate::new(404)
                .set_body_json(json!({"message": "Media not found", "status": "fail"})),
        )
        .mount(&server)
        .await;
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Tagged)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.name, "alice · tagged");
    assert_eq!(keys(&resolved), vec!["401"]);
    assert_eq!(resolved.files[0].category, Category::Tagged);
    assert_eq!(resolved.files[0].owner, "carol");
    assert_eq!(resolved.files[0].owner_id.as_deref(), Some("2002"));
    assert!(
        resolved
            .notes
            .iter()
            .any(|n| n.contains("1 post(s) that are no longer available")),
        "{:?}",
        resolved.notes
    );
    let tagged = requests_to(&server, "/graphql/query").await;
    assert_eq!(variables_of(&tagged[0])["user_id"], USER_ID);
    assert_eq!(
        header_of(&tagged[0], "referer"),
        format!("{}/alice/tagged/", server.uri())
    );
}

// ---------------------------------------------------------------- stories & highlights

fn story_reel(user_id: &str, username: &str, items: Vec<Value>) -> Value {
    json!({
        "id": user_id,
        "reel_type": "user_reel",
        "user": {"pk": user_id, "username": username},
        "items": items
    })
}

#[tokio::test]
async fn stories_resolve_filter_by_id_and_report_empty() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_profile(&server, "quiet", "3003", None).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", USER_ID))
        .respond_with(ok(json!({
            "reels_media": [story_reel(USER_ID, "alice", vec![
                image_item(&cdn, 601, "StoryCode1", 1_700_000_100),
                video_item(&cdn, 602, "StoryCode2", 1_700_000_200),
            ])],
            "status": "ok"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "3003"))
        .respond_with(ok(json!({"reels_media": [], "reels": {}, "status": "ok"})))
        .mount(&server)
        .await;
    let client = client(&server);
    let stories = |user: &str, id: Option<&str>| Target::Stories {
        username: user.into(),
        story_id: id.map(str::to_string),
    };

    let all = run(&client, &stories("alice", None), &opts())
        .await
        .unwrap();
    assert_eq!(all.name, "alice · stories");
    assert_eq!(keys(&all), vec!["601", "602"]);
    assert!(
        all.files
            .iter()
            .all(|f| f.category == Category::Story && f.shortcode.is_none())
    );

    let one = run(&client, &stories("alice", Some("602")), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&one), vec!["602"]);

    let err = run(&client, &stories("alice", Some("123456")), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::NotFound(m) if m.contains("24 hours")),
        "{err:?}"
    );

    let quiet = run(
        &client,
        &Target::Profile {
            username: "quiet".into(),
            section: Some(ProfileSection::Stories),
        },
        &opts(),
    )
    .await
    .unwrap();
    assert!(quiet.files.is_empty());
    assert!(
        quiet.notes.iter().any(|n| n.contains("no current stories")),
        "{:?}",
        quiet.notes
    );
    assert!(requests_to(&server, "/graphql/query").await.is_empty());
}

#[tokio::test]
async fn stories_accept_the_reels_map_shape() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ok(json!({
            "reels": {USER_ID: {"id": USER_ID, "items": [image_item(&cdn, 701, "S", 1)]}},
            "status": "ok"
        })))
        .mount(&server)
        .await;
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Stories)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(keys(&resolved), vec!["701"]);
}

#[tokio::test]
async fn stories_fall_back_to_graphql_when_reels_media_fails() {
    for failure in [
        ResponseTemplate::new(500).set_body_string("Internal Server Error"),
        ResponseTemplate::new(302).insert_header("location", "https://www.instagram.com/"),
        ResponseTemplate::new(200).set_body_raw("<html><body>home</body></html>", "text/html"),
        // The retired endpoint refusing is not a cookie problem.
        ResponseTemplate::new(403)
            .set_body_json(json!({"message": "useragent mismatch", "status": "fail"})),
        ResponseTemplate::new(400)
            .set_body_json(json!({"message": "bad request", "status": "fail"})),
    ] {
        let server = MockServer::start().await;
        let cdn = server.uri();
        mount_profile(&server, "alice", USER_ID, None).await;
        Mock::given(method("GET"))
            .and(path("/api/v1/feed/reels_media/"))
            .respond_with(failure)
            .expect(1) // no retries: the fallback is the retry
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql/query"))
            .and(gql(&doc_ids::STORIES).after(Value::Null))
            .respond_with(ok(connection(
                STORIES_ROOT,
                vec![story_reel(
                    USER_ID,
                    "alice",
                    vec![image_item(&cdn, 611, "S1", 5)],
                )],
                Some("more"),
            )))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql/query"))
            .and(gql(&doc_ids::STORIES).after(json!("more")))
            .respond_with(ok(connection(STORIES_ROOT, vec![], None)))
            .expect(1)
            .mount(&server)
            .await;
        let resolved = run(
            &client(&server),
            &Target::Stories {
                username: "alice".into(),
                story_id: None,
            },
            &opts(),
        )
        .await
        .unwrap();
        assert_eq!(keys(&resolved), vec!["611"]);
        assert_eq!(resolved.files[0].category, Category::Story);
        let gql_requests = requests_to(&server, "/graphql/query").await;
        let variables = variables_of(&gql_requests[0]);
        assert_eq!(variables["reel_ids"], json!([USER_ID]));
        assert_eq!(variables["initial_reel_id"], USER_ID);
        assert_eq!(
            header_of(&gql_requests[0], "referer"),
            format!("{}/alice/", server.uri())
        );
    }
}

/// Answers `reels_media` requests for highlights with one image per requested id.
fn highlight_responder(cdn: String) -> impl Fn(&Request) -> ResponseTemplate + Send + Sync {
    move |request: &Request| {
        let mut reels = serde_json::Map::new();
        for (key, id) in request.url.query_pairs() {
            if key != "reel_ids" {
                continue;
            }
            let number: u64 = id.trim_start_matches("highlight:").parse().unwrap();
            reels.insert(id.to_string(), highlight_reel(&cdn, number));
        }
        ok(json!({"reels": reels, "status": "ok"}))
    }
}

fn highlight_reel(cdn: &str, number: u64) -> Value {
    json!({
        "id": format!("highlight:{number}"),
        "title": format!("server title {number}"),
        "reel_type": "highlight_reel",
        "user": {"pk": 1001, "username": "alice"},
        "items": [image_item(cdn, 9000 + number, "HlCode", 1)]
    })
}

async fn mount_tray(server: &MockServer, count: u64) {
    let edges: Vec<Value> = (1..=count)
        .map(|n| json!({"node": {"id": format!("highlight:{n}"), "title": format!("Trip {n}"), "cover_media": {}}}))
        .collect();
    Mock::given(method("POST"))
        .and(path("/api/graphql"))
        .and(gql(&doc_ids::HIGHLIGHTS_TRAY))
        .respond_with(ok(json!({"data": {"highlights": {"edges": edges, "page_info": {"has_next_page": false}}}, "extensions": {}})))
        .mount(server)
        .await;
}

#[tokio::test]
async fn highlights_tray_is_fetched_in_chunks_with_titles() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_tray(&server, 7).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(highlight_responder(server.uri()))
        .mount(&server)
        .await;
    let client = client(&server);
    let resolved = run(
        &client,
        &profile_target(Some(ProfileSection::Highlights)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.name, "alice · highlights");
    assert_eq!(resolved.files.len(), 7);
    for (n, file) in (1..=7).zip(&resolved.files) {
        assert_eq!(file.key, (9000 + n).to_string());
        assert_eq!(
            file.highlight.as_deref(),
            Some(format!("Trip {n}").as_str())
        );
        assert_eq!(file.category, Category::Highlight);
        assert_eq!(file.shortcode, None);
    }
    let chunks: Vec<usize> = requests_to(&server, "/api/v1/feed/reels_media/")
        .await
        .iter()
        .map(|r| r.url.query_pairs().filter(|(k, _)| k == "reel_ids").count())
        .collect();
    assert_eq!(chunks, vec![5, 2]);
    let tray = requests_to(&server, "/api/graphql").await;
    assert_eq!(tray.len(), 1);
    assert_eq!(variables_of(&tray[0]), json!({"user_id": USER_ID}));
    assert!(tray[0].headers.get("x-root-field-name").is_none());
    assert_eq!(
        header_of(&tray[0], "x-fb-friendly-name"),
        doc_ids::HIGHLIGHTS_TRAY.name
    );

    // `limit` caps the number of highlights.
    let options = CrawlOptions {
        limit: Some(3),
        ..opts()
    };
    let limited = run(
        &client,
        &profile_target(Some(ProfileSection::Highlights)),
        &options,
    )
    .await
    .unwrap();
    assert_eq!(limited.files.len(), 3);
    assert!(
        limited
            .notes
            .iter()
            .any(|n| n.contains("limited to 3 of 7")),
        "{:?}",
        limited.notes
    );
}

#[tokio::test]
async fn highlight_items_switch_to_graphql_after_one_failure() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_tray(&server, 7).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&server)
        .await;
    let responder_cdn = cdn.clone();
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::STORIES))
        .respond_with(move |request: &Request| {
            let variables = variables_of(request);
            let nodes: Vec<Value> = variables["reel_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| {
                    let number = id.as_str().unwrap().trim_start_matches("highlight:");
                    highlight_reel(&responder_cdn, number.parse().unwrap())
                })
                .collect();
            ok(connection(STORIES_ROOT, nodes, None))
        })
        .expect(2)
        .mount(&server)
        .await;
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Highlights)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.files.len(), 7);
    assert_eq!(resolved.files[6].highlight.as_deref(), Some("Trip 7"));
    let chunks: Vec<Value> = requests_to(&server, "/graphql/query")
        .await
        .iter()
        .map(|r| variables_of(r)["initial_reel_id"].clone())
        .collect();
    assert_eq!(chunks, vec![json!("highlight:1"), json!("highlight:6")]);
}

#[tokio::test]
async fn highlight_reels_with_bare_numeric_ids_are_matched() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, None).await;
    mount_tray(&server, 2).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    let responder_cdn = cdn.clone();
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::STORIES))
        .respond_with(move |request: &Request| {
            let nodes: Vec<Value> = variables_of(request)["reel_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| {
                    let number = id.as_str().unwrap().trim_start_matches("highlight:");
                    let mut reel = highlight_reel(&responder_cdn, number.parse().unwrap());
                    // The web app's answer may name the reel by its bare number.
                    reel["id"] = json!(number);
                    reel
                })
                .collect();
            ok(connection(STORIES_ROOT, nodes, None))
        })
        .mount(&server)
        .await;
    let resolved = run(
        &client(&server),
        &profile_target(Some(ProfileSection::Highlights)),
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.files.len(), 2, "{:?}", resolved.notes);
    assert_eq!(resolved.files[1].highlight.as_deref(), Some("Trip 2"));
}

#[tokio::test]
async fn graphql_feedback_required_from_a_stale_doc_id_is_retried_as_outdated() {
    for still_refused in [false, true] {
        let server = MockServer::start().await;
        let cdn = server.uri();
        let cache_dir = tempfile::tempdir().unwrap();
        write_cache(
            cache_dir.path(),
            doc_ids::POSTS.name,
            "1110001",
            now() - 3600,
        );
        mount_profile(&server, "alice", USER_ID, None).await;
        let refused = ResponseTemplate::new(400).set_body_json(json!({
            "message": "feedback_required", "spam": true, "status": "fail"
        }));
        Mock::given(method("POST"))
            .and(path("/graphql/query"))
            .and(gql(&doc_ids::POSTS).doc("1110001"))
            .respond_with(refused.clone())
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/graphql/query"))
            .and(gql(&doc_ids::POSTS).doc(doc_ids::POSTS.default_doc_id))
            .respond_with(if still_refused {
                refused
            } else {
                posts_page(vec![image_item(&cdn, 1, "C1", 10)], None)
            })
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new(
            ClientConfig {
                cache_dir: Some(cache_dir.path().to_path_buf()),
                ..config(&server)
            },
            Some(jar()),
        )
        .unwrap();
        let result = run(&client, &profile_target(None), &opts()).await;
        if still_refused {
            let err = result.unwrap_err();
            assert!(
                matches!(&err, Error::RateLimited(m) if m.contains("feedback_required")),
                "{err:?}"
            );
        } else {
            assert_eq!(keys(&result.unwrap()), vec!["1"]);
        }
    }
}

#[tokio::test]
async fn graphql_sends_the_viewer_id_as_av_when_the_page_has_it() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    let page = profile_html(&server, "alice", USER_ID, None).replace(
        "</head>",
        r#"<script>{"IG_USER_EIMU":"17841400000000042"}</script></head>"#,
    );
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(page))
        .mount(&server)
        .await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 1, "C1", 10)],
        None,
    )
    .await;
    run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    let request = &requests_to(&server, "/graphql/query").await[0];
    let form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
        .into_owned()
        .collect();
    assert_eq!(
        form.get("av").map(String::as_str),
        Some("17841400000000042")
    );
}

#[tokio::test]
async fn highlight_link_resolves_title_and_owner() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "highlight:17900000000000777"))
        .respond_with(ok(json!({
            "reels_media": [{
                "id": "highlight:17900000000000777",
                "title": "Summer",
                "user": {"pk": 1001, "username": "alice"},
                "items": [image_item(&cdn, 801, "H1", 1), video_item(&cdn, 802, "H2", 2)]
            }],
            "status": "ok"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", "highlight:404"))
        .respond_with(ok(json!({"reels_media": [], "status": "ok"})))
        .mount(&server)
        .await;
    let client = client(&server);
    let resolved = run(
        &client,
        &Target::Highlight {
            id: "17900000000000777".into(),
        },
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.name, "alice · highlight Summer");
    assert_eq!(keys(&resolved), vec!["801", "802"]);
    assert!(resolved.files.iter().all(|f| {
        f.highlight.as_deref() == Some("Summer")
            && f.category == Category::Highlight
            && f.owner == "alice"
    }));
    let err = run(&client, &Target::Highlight { id: "404".into() }, &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::NotFound(m) if m.contains("highlight 404")),
        "{err:?}"
    );
}

#[tokio::test]
async fn highlight_link_falls_back_to_graphql_with_home_page_tokens() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/"))
        .expect(1)
        .mount(&server)
        .await;
    // The home page provides the tokens when no profile page was loaded.
    Mock::given(method("GET"))
        .and(path("/"))
        .respond_with(html(profile_html(&server, "me", VIEWER_ID, None)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::STORIES))
        .respond_with(ok(connection(
            STORIES_ROOT,
            vec![highlight_reel(&cdn, 55)],
            None,
        )))
        .expect(1)
        .mount(&server)
        .await;
    let resolved = run(
        &client(&server),
        &Target::Highlight { id: "55".into() },
        &opts(),
    )
    .await
    .unwrap();
    assert_eq!(resolved.name, "alice · highlight server title 55");
    assert_eq!(keys(&resolved), vec!["9055"]);
    let gql_request = &requests_to(&server, "/graphql/query").await[0];
    assert_eq!(header_of(gql_request, "x-fb-lsd"), LSD);
    assert_eq!(
        header_of(gql_request, "referer"),
        format!("{}/", server.uri())
    );
    assert_eq!(
        variables_of(gql_request)["reel_ids"],
        json!(["highlight:55"])
    );
}

// ---------------------------------------------------------------- doc ids

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn write_cache(dir: &std::path::Path, name: &str, id: &str, found_at: i64) {
    std::fs::write(
        dir.join(CACHE_FILE),
        json!({"queries": {name: {"doc_id": id, "found_at": found_at}}}).to_string(),
    )
    .unwrap();
}

fn read_cache(dir: &std::path::Path) -> DocIdCache {
    DocIdCache::load(&dir.join(CACHE_FILE))
}

#[tokio::test]
async fn doc_ids_are_discovered_from_bundles_without_cookies_and_cached() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    let cache_dir = tempfile::tempdir().unwrap();
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/yA/r/Common.js", "v4/yB/r/Posts.js?_nc_x=Ij3Wp8lg5Kz"],
        )))
        .mount(&server)
        .await;
    mount_bundle(&server, "v4/yA/r/Common.js", &[("SomethingElse", "1")]).await;
    mount_bundle(
        &server,
        "v4/yB/r/Posts.js",
        &[
            (doc_ids::POSTS.name, "3330001"),
            (doc_ids::STORIES.name, "4440001"),
        ],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).doc("3330001"))
        .respond_with(posts_page(vec![image_item(&cdn, 1, "C1", 10)], None))
        .expect(2)
        .mount(&server)
        .await;
    let config = ClientConfig {
        cache_dir: Some(cache_dir.path().to_path_buf()),
        ..config(&server)
    };
    let first = Client::new(config.clone(), Some(jar())).unwrap();
    let resolved = run(&first, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(keys(&resolved), vec!["1"]);

    let bundles = requests_to(&server, "/rsrc.php/").await;
    assert_eq!(bundles.len(), 2, "the stylesheet is not fetched");
    for bundle in &bundles {
        assert!(bundle.headers.get("cookie").is_none(), "{bundle:?}");
        assert!(bundle.headers.get("x-csrftoken").is_none());
    }
    let cache = read_cache(cache_dir.path());
    assert_eq!(cache.fresh(doc_ids::POSTS.name, now()), Some("3330001"));
    assert_eq!(cache.fresh(doc_ids::STORIES.name, now()), Some("4440001"));

    // A new client (a later run) uses the cache: no bundle is fetched again.
    let second = Client::new(config, Some(jar())).unwrap();
    run(&second, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(requests_to(&server, "/rsrc.php/").await.len(), 2);
}

#[tokio::test]
async fn stale_doc_id_cache_is_rediscovered() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    let cache_dir = tempfile::tempdir().unwrap();
    write_cache(
        cache_dir.path(),
        doc_ids::POSTS.name,
        "1110001",
        now() - 2 * 86_400,
    );
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/a/Posts.js"],
        )))
        .mount(&server)
        .await;
    mount_bundle(
        &server,
        "v4/a/Posts.js",
        &[(doc_ids::POSTS.name, "2220001")],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).doc("2220001"))
        .respond_with(posts_page(vec![image_item(&cdn, 1, "C1", 10)], None))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(
        ClientConfig {
            cache_dir: Some(cache_dir.path().to_path_buf()),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    run(&client, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(
        read_cache(cache_dir.path()).fresh(doc_ids::POSTS.name, now()),
        Some("2220001")
    );
}

#[tokio::test]
async fn config_doc_ids_win_over_discovery() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/a/Posts.js"],
        )))
        .mount(&server)
        .await;
    mount_bundle(
        &server,
        "v4/a/Posts.js",
        &[(doc_ids::POSTS.name, "2220001")],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).doc("5550001"))
        .respond_with(posts_page(vec![image_item(&cdn, 1, "C1", 10)], None))
        .expect(1)
        .mount(&server)
        .await;
    let doc_ids = BTreeMap::from([(doc_ids::POSTS.name.to_string(), "5550001".to_string())]);
    let client = Client::new(
        ClientConfig {
            doc_ids: doc_ids.clone(),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    let resolved = run(&client, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(keys(&resolved), vec!["1"]);
    assert!(requests_to(&server, "/rsrc.php/").await.is_empty());

    // An outdated override is reported, not second-guessed.
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .respond_with(ok(
            json!({"data": null, "errors": [{"message": "PersistedQueryNotFound"}]}),
        ))
        .expect(1)
        .mount(&server)
        .await;
    let client = Client::new(
        ClientConfig {
            doc_ids,
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    let err = run(&client, &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Parse(m) if m.contains("is outdated") && m.contains("[doc_ids]") && m.contains("5550001")),
        "{err:?}"
    );
}

#[tokio::test]
async fn outdated_query_triggers_one_rediscovery() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    let cache_dir = tempfile::tempdir().unwrap();
    // Yesterday's discovery (still fresh) found 1110001; Instagram moved on.
    write_cache(
        cache_dir.path(),
        doc_ids::POSTS.name,
        "1110001",
        now() - 3600,
    );
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/a/Posts.js"],
        )))
        .mount(&server)
        .await;
    mount_bundle(
        &server,
        "v4/a/Posts.js",
        &[(doc_ids::POSTS.name, "2220001")],
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).doc("1110001"))
        .respond_with(ok(json!({
            "data": null,
            "errors": [{"message": "missing_required_variable_value", "severity": "CRITICAL", "code": 1675012}]
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).doc("2220001"))
        .respond_with(posts_page(vec![image_item(&cdn, 1, "C1", 10)], None))
        .expect(2)
        .mount(&server)
        .await;
    let client = Client::new(
        ClientConfig {
            cache_dir: Some(cache_dir.path().to_path_buf()),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    let resolved = run(&client, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(keys(&resolved), vec!["1"]);
    assert_eq!(
        read_cache(cache_dir.path()).fresh(doc_ids::POSTS.name, now()),
        Some("2220001")
    );
    // The same client keeps the new id; nothing is rediscovered again.
    run(&client, &profile_target(None), &opts()).await.unwrap();
    assert_eq!(requests_to(&server, "/rsrc.php/").await.len(), 1);
}

#[tokio::test]
async fn outdated_query_without_a_newer_id_is_a_parse_error() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/a/Posts.js"],
        )))
        .mount(&server)
        .await;
    mount_bundle(
        &server,
        "v4/a/Posts.js",
        &[(doc_ids::POSTS.name, "1110001")],
    )
    .await;
    // Discovery finds 1110001; it fails, the built-in default is tried once, and fails too.
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .respond_with(ok(json!({"message": "execution error", "status": "fail"})))
        .expect(2)
        .mount(&server)
        .await;
    let client = client(&server);
    let err = run(&client, &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Parse(m) if m.starts_with(&format!("Instagram query {} is outdated: execution error", doc_ids::POSTS.name))),
        "{err:?}"
    );
    let ids: Vec<String> = requests_to(&server, "/graphql/query")
        .await
        .iter()
        .map(|r| form_of(r)["doc_id"].clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            "1110001".to_string(),
            doc_ids::POSTS.default_doc_id.to_string()
        ]
    );
    assert_eq!(requests_to(&server, "/rsrc.php/").await.len(), 1);

    // With other sections, an outdated one is skipped with a note.
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .respond_with(ok(
            json!({"data": null, "errors": [{"message": "Query error"}]}),
        ))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .respond_with(ok(json!({"reels_media": [], "status": "ok"})))
        .mount(&server)
        .await;
    let options = CrawlOptions {
        sections: vec![ProfileSection::Posts, ProfileSection::Stories],
        ..opts()
    };
    let resolved = run(&client_for(&server), &profile_target(None), &options)
        .await
        .unwrap();
    assert!(
        resolved
            .notes
            .iter()
            .any(|n| n.starts_with("posts: skipped") && n.contains("outdated")),
        "{:?}",
        resolved.notes
    );
}

fn client_for(server: &MockServer) -> Client {
    client(server)
}

// ---------------------------------------------------------------- error classification & rate limits

async fn get_err(server: &MockServer) -> Error {
    client(server).get_json("/api/v1/thing/").await.unwrap_err()
}

#[tokio::test]
async fn login_required_json_is_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/thing/"))
        .respond_with(ok(
            json!({"message": "", "require_login": true, "status": "fail"}),
        ))
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("not logged in")),
        "{err:?}"
    );

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_json(json!({"message": "login_required", "status": "fail"})),
        )
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("not logged in")),
        "{err:?}"
    );
}

#[tokio::test]
async fn redirect_to_login_is_auth_and_not_followed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/thing/"))
        .respond_with(ResponseTemplate::new(302).insert_header(
            "location",
            "https://www.instagram.com/accounts/login/?next=/api/v1/thing/",
        ))
        .expect(1)
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("not logged in")),
        "{err:?}"
    );
}

#[tokio::test]
async fn checkpoint_is_auth_with_confirm_hint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "message": "checkpoint_required",
            "checkpoint_url": "/challenge/FAKE/",
            "status": "fail"
        })))
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("confirm this login")),
        "{err:?}"
    );
}

#[tokio::test]
async fn forbidden_is_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(json!({"message": "nope", "status": "fail"})),
        )
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("HTTP 403") && m.contains("nope")),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_429_pauses_every_request_then_retries_once() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/a/"))
        .respond_with(ResponseTemplate::new(429).set_body_string(""))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/a/"))
        .respond_with(ok(json!({"status": "ok", "value": 7})))
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html(&server, "alice", USER_ID, None)))
        .mount(&server)
        .await;
    let wait = Duration::from_millis(400);
    let client = Arc::new(
        Client::new(
            ClientConfig {
                rate_limit_wait: wait,
                ..config(&server)
            },
            Some(jar()),
        )
        .unwrap(),
    );
    let started = Instant::now();
    let first = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get_json("/api/v1/a/").await })
    };
    while client.paused_for().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no pause started"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // Another request (a page, even) waits for the pause too.
    let remaining = client.paused_for().unwrap_or_default();
    let other_started = Instant::now();
    client.user_by_username("alice").await.unwrap();
    assert!(
        other_started.elapsed() + Duration::from_millis(30) >= remaining,
        "{:?} < {remaining:?}",
        other_started.elapsed()
    );
    let value = first.await.unwrap().unwrap();
    assert_eq!(value["value"], json!(7));
    assert!(started.elapsed() >= wait);
    assert_eq!(requests_to(&server, "/api/v1/a/").await.len(), 2);
}

#[tokio::test]
async fn a_second_429_is_rate_limited() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .expect(2) // first try + one retry after the pause
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::RateLimited(m) if m.contains("wait 10-30 minutes") && m.contains("larger API delay")),
        "{err:?}"
    );
}

#[tokio::test]
async fn please_wait_and_feedback_required_are_not_retried() {
    let cases = [
        (
            ResponseTemplate::new(401).set_body_json(json!({
                "message": "Please wait a few minutes before you try again.",
                "require_login": false,
                "status": "fail"
            })),
            "re-export cookies or wait",
        ),
        (
            // The body Instagram is reported to send: a throttle, not bad cookies.
            ResponseTemplate::new(401).set_body_json(json!({
                "message": "Please wait a few minutes before you try again.",
                "require_login": true,
                "status": "fail"
            })),
            "re-export cookies or wait",
        ),
        (
            ok(
                json!({"message": "Please wait a few minutes before you try again.", "status": "fail"}),
            ),
            "re-export cookies or wait",
        ),
        (
            ResponseTemplate::new(400).set_body_json(json!({
                "message": "feedback_required",
                "spam": true,
                "feedback_title": "Try Again Later",
                "status": "fail"
            })),
            "feedback_required",
        ),
    ];
    for (template, expected) in cases {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(template)
            .expect(1)
            .mount(&server)
            .await;
        let client = client(&server);
        let err = client.get_json("/api/v1/thing/").await.unwrap_err();
        assert!(
            matches!(&err, Error::RateLimited(m) if m.contains(expected)),
            "{err:?}"
        );
        assert!(client.paused_for().is_none(), "not a 429: no global pause");
    }
}

#[tokio::test]
async fn server_error_then_success_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500).set_body_string("oops"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ok(json!({"status": "ok"})))
        .with_priority(2)
        .mount(&server)
        .await;
    client(&server).get_json("/api/v1/thing/").await.unwrap();

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(3) // first try + transient_retries (2)
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(matches!(err, Error::Http { status: 503, .. }), "{err:?}");
}

#[tokio::test]
async fn network_errors_are_retried_then_reported() {
    // Reserve a free port, then close it so connections are refused.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let server = MockServer::start().await;
    let config = ClientConfig {
        api_base: format!("http://127.0.0.1:{port}"),
        ..config(&server)
    };
    let client = Client::new(config, Some(jar())).unwrap();
    let err = client.get_json("/api/v1/thing/").await.unwrap_err();
    assert!(matches!(err, Error::Network(_)), "{err:?}");
}

#[tokio::test]
async fn html_bodies_are_auth_or_parse() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<html><body><form id=\"loginForm\" action=\"/accounts/login/ajax/\"></form></body></html>",
            "text/html",
        ))
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(matches!(err, Error::Auth(_)), "{err:?}");

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html><body>Hello</body></html>", "text/html"),
        )
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(
        matches!(&err, Error::Parse(m) if m.contains("text/html")),
        "{err:?}"
    );
}

#[tokio::test]
async fn login_check_does_not_queue_behind_another_requests_pause() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/a/"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    mount_form(&server, ok(web_form_data("viewer")), 1).await;
    let client = Arc::new(Client::new(slow_pause_config(&server), Some(jar())).unwrap());
    let crawl = {
        let client = Arc::clone(&client);
        tokio::spawn(async move { client.get_json("/api/v1/a/").await })
    };
    let started = Instant::now();
    while client.paused_for().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "no pause started"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // The crawl now waits two minutes for the pause; the check must not.
    let checked = Instant::now();
    let me = client.current_user().await.unwrap();
    assert_eq!(me.username, "viewer");
    assert!(
        checked.elapsed() < Duration::from_secs(5),
        "{:?}",
        checked.elapsed()
    );
    crawl.abort();
}

#[tokio::test]
async fn a_429_soon_after_another_one_stops_without_a_retry() {
    let server = MockServer::start().await;
    for route in ["/api/v1/a/", "/api/v1/b/"] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ok(json!({"status": "ok"})))
            .with_priority(2)
            .mount(&server)
            .await;
    }
    let client = Client::new(
        ClientConfig {
            rate_limit_wait: Duration::from_millis(50),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    client.get_json("/api/v1/a/").await.unwrap();
    let err = client.get_json("/api/v1/b/").await.unwrap_err();
    assert!(
        matches!(&err, Error::RateLimited(m) if m.contains("wait 10-30 minutes")),
        "{err:?}"
    );
    assert_eq!(
        requests_to(&server, "/api/v1/b/").await.len(),
        1,
        "no retry"
    );
}

#[tokio::test]
async fn a_rate_limited_discovery_page_ends_the_crawl() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, None).await;
    Mock::given(method("GET"))
        .and(path("/alice/reels/"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ok(json!({"data": {}, "status": "ok"})))
        .expect(0)
        .mount(&server)
        .await;
    let client = client(&server);
    let err = run(
        &client,
        &profile_target(Some(ProfileSection::Reels)),
        &opts(),
    )
    .await
    .unwrap_err();
    assert!(matches!(err, Error::RateLimited(_)), "{err:?}");
    assert_eq!(
        requests_to(&server, "/alice/reels/").await.len(),
        2,
        "first try + one retry"
    );
}

// ---------------------------------------------------------------- login check

/// Config whose rate-limit pause would take minutes: the login check must not wait.
fn slow_pause_config(server: &MockServer) -> ClientConfig {
    ClientConfig {
        rate_limit_wait: Duration::from_secs(120),
        api_delay: Duration::from_secs(30),
        ..config(server)
    }
}

const FORM_PATH: &str = "/api/v1/accounts/edit/web_form_data/";
const PROBE_HASH: &str = "d6f4427fbe92d846298cf93df0b937d3";

fn web_form_data(username: &str) -> Value {
    json!({
        "form_data": {
            "first_name": "Private Person",
            "email": EMAIL,
            "phone_number": PHONE,
            "birthday": BIRTHDAY,
            "username": username,
            "biography": "",
            "external_url": "",
            "is_email_confirmed": true
        },
        "status": "ok"
    })
}

async fn mount_form(server: &MockServer, template: ResponseTemplate, calls: u64) {
    Mock::given(method("GET"))
        .and(path(FORM_PATH))
        .respond_with(template)
        .expect(calls)
        .mount(server)
        .await;
}

async fn mount_probe(server: &MockServer, template: ResponseTemplate, calls: u64) {
    Mock::given(method("GET"))
        .and(path("/graphql/query"))
        .and(query_param("query_hash", PROBE_HASH))
        .and(query_param("variables", "{}"))
        .respond_with(template)
        .expect(calls)
        .mount(server)
        .await;
}

fn probe_user(username: &str) -> ResponseTemplate {
    ok(json!({"data": {"user": {"username": username, "profile_pic_url": "x"}}, "status": "ok"}))
}

#[tokio::test]
async fn login_check_reads_only_the_username_of_the_account_form() {
    let server = MockServer::start().await;
    mount_form(&server, ok(web_form_data("viewer")), 1).await;
    mount_probe(&server, probe_user("unused"), 0).await;
    let logs = capture_logs();
    let me = client(&server).current_user().await.unwrap();
    assert_eq!(
        (me.id.as_str(), me.username.as_str()),
        (VIEWER_ID, "viewer")
    );
    assert_eq!(me.full_name, None, "nothing else of the form is kept");
    let shown = format!("{me:?}");
    assert_no_secrets(&shown);
    assert_no_secrets(&logs.text());
    let request = &server.received_requests().await.unwrap()[0];
    assert_eq!(header_of(request, "x-asbd-id"), "359341");
    assert_eq!(header_of(request, "origin"), server.uri());
}

#[tokio::test]
async fn login_check_falls_back_to_the_probe_without_waiting() {
    let failures = [
        ResponseTemplate::new(429),
        ResponseTemplate::new(500).set_body_string("oops"),
        ResponseTemplate::new(302).insert_header("location", "https://www.instagram.com/"),
        ResponseTemplate::new(401).set_body_json(
            json!({"message": "Please wait a few minutes before you try again.", "status": "fail"}),
        ),
        ResponseTemplate::new(401).set_body_json(json!({
            "message": "Please wait a few minutes before you try again.",
            "require_login": true,
            "status": "fail"
        })),
        ResponseTemplate::new(403)
            .set_body_json(json!({"message": "useragent mismatch", "status": "fail"})),
        ok(json!({"form_data": {"email": EMAIL}, "status": "ok"})),
    ];
    for failure in failures {
        let server = MockServer::start().await;
        mount_form(&server, failure, 1).await;
        mount_probe(&server, probe_user("viewer"), 1).await;
        let client = Client::new(slow_pause_config(&server), Some(jar())).unwrap();
        let started = Instant::now();
        let me = client.current_user().await.unwrap();
        assert_eq!(
            (me.id.as_str(), me.username.as_str()),
            (VIEWER_ID, "viewer")
        );
        assert!(started.elapsed() < Duration::from_secs(5), "no waiting");
    }
}

#[tokio::test]
async fn login_check_ends_at_an_authentication_signal() {
    let auth_failures = [
        (
            ResponseTemplate::new(302).insert_header("location", "/accounts/login/?next=%2F"),
            "not logged in",
        ),
        (
            ResponseTemplate::new(401).set_body_json(
                json!({"message": "login_required", "require_login": true, "status": "fail"}),
            ),
            "not logged in",
        ),
        (
            ResponseTemplate::new(400).set_body_json(
                json!({"message": "challenge_required", "challenge": {"url": "/challenge/x/"}, "status": "fail"}),
            ),
            "confirm this login",
        ),
    ];
    for (failure, expected) in auth_failures {
        let server = MockServer::start().await;
        mount_form(&server, failure, 1).await;
        mount_probe(&server, probe_user("unused"), 0).await;
        let err = client(&server).current_user().await.unwrap_err();
        assert!(
            matches!(&err, Error::Auth(m) if m.contains(expected)),
            "{err:?}"
        );
    }

    // The probe's `data.user == null` means the session is not logged in.
    let server = MockServer::start().await;
    mount_form(&server, ResponseTemplate::new(429), 1).await;
    mount_probe(
        &server,
        ok(json!({"data": {"user": null}, "status": "ok"})),
        1,
    )
    .await;
    let err = client(&server).current_user().await.unwrap_err();
    assert!(
        matches!(&err, Error::Auth(m) if m.contains("not logged in")),
        "{err:?}"
    );
}

#[tokio::test]
async fn login_check_that_cannot_tell_says_the_cookies_are_probably_fine() {
    let server = MockServer::start().await;
    mount_form(&server, ResponseTemplate::new(429), 1).await;
    mount_probe(
        &server,
        ResponseTemplate::new(302).insert_header("location", "https://www.instagram.com/"),
        1,
    )
    .await;
    let client = Client::new(slow_pause_config(&server), Some(jar())).unwrap();
    let started = Instant::now();
    let err = client.current_user().await.unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5), "no waiting");
    assert!(
        matches!(&err, Error::RateLimited(m) if m.contains("429") && m.contains("probably fine")),
        "{err:?}"
    );
    assert!(!format!("{err} {err:?}").contains(SESSION));
    // The quick check still recorded the global pause for later requests.
    assert!(client.paused_for().is_some());

    // feedback_required is named.
    let server = MockServer::start().await;
    mount_form(
        &server,
        ResponseTemplate::new(400)
            .set_body_json(json!({"message": "feedback_required", "status": "fail"})),
        1,
    )
    .await;
    mount_probe(&server, ResponseTemplate::new(500), 1).await;
    let err = client_for(&server).current_user().await.unwrap_err();
    assert!(
        matches!(&err, Error::RateLimited(m) if m.contains("feedback_required") && m.contains("probably fine")),
        "{err:?}"
    );

    // Unreachable: a network error, still "probably fine".
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let offline = Client::new(
        ClientConfig {
            api_base: format!("http://127.0.0.1:{port}"),
            ..config(&server)
        },
        Some(jar()),
    )
    .unwrap();
    let err = offline.current_user().await.unwrap_err();
    assert!(
        matches!(&err, Error::Network(m) if m.contains("probably fine")),
        "{err:?}"
    );
}

#[tokio::test]
async fn login_check_without_ds_user_id_or_session() {
    let server = MockServer::start().await;
    mount_form(&server, ResponseTemplate::new(429), 1).await;
    mount_probe(
        &server,
        ok(json!({"data": {"user": {"username": "fallback", "id": "998"}}, "status": "ok"})),
        1,
    )
    .await;
    let no_ds = Client::new(
        config(&server),
        Some(CookieJar::from_pairs([
            ("sessionid", SESSION),
            ("csrftoken", CSRF),
        ])),
    )
    .unwrap();
    let me = no_ds.current_user().await.unwrap();
    assert_eq!((me.id.as_str(), me.username.as_str()), ("998", "fallback"));

    let anonymous = Client::new(config(&server), None).unwrap();
    assert!(!anonymous.is_authenticated());
    assert!(matches!(
        anonymous.current_user().await,
        Err(Error::Auth(_))
    ));
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn refresh_url_finds_the_file_by_key() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_media(&server, CAROUSEL_CODE, carousel_item(&cdn)).await;
    let client = client(&server);
    let resolved = run(&client, &post_target(CAROUSEL_CODE, None), &opts())
        .await
        .unwrap();
    // Refreshing goes through the parent's pk (`post_id`).
    Mock::given(method("GET"))
        .and(path("/api/v1/media/500/info/"))
        .respond_with(ok(json!({"items": [carousel_item(&cdn)], "status": "ok"})))
        .expect(2)
        .mount(&server)
        .await;
    let mut video = resolved.files[1].clone();
    video.url = format!("{cdn}/cdn/expired.mp4?oe=00000001");
    let fresh = refresh_url(&client, &video).await.unwrap();
    assert_eq!(fresh, format!("{cdn}/cdn/502_720.mp4?oe=6700AAAA"));

    // Unknown key: falls back to the same index.
    let mut renamed = resolved.files[2].clone();
    renamed.key = "000".into();
    let fresh = refresh_url(&client, &renamed).await.unwrap();
    assert_eq!(
        fresh,
        format!("{cdn}/cdn/503_l.jpg?oe=6700AAAA&_nc_sid=fake")
    );

    // Gone from Instagram.
    let mut gone = resolved.files[0].clone();
    gone.post_id = "424242".into();
    let err = refresh_url(&client, &gone).await.unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err:?}");
}

// ---------------------------------------------------------------- secrets

#[tokio::test]
async fn session_id_never_appears_in_errors() {
    let server = MockServer::start().await;
    let echo = format!("invalid session {SESSION} for csrftoken={CSRF}");
    let routes: Vec<(&str, ResponseTemplate)> = vec![
        (
            "/e/403/",
            ResponseTemplate::new(403).set_body_json(json!({"message": echo, "status": "fail"})),
        ),
        (
            "/e/400/",
            ResponseTemplate::new(400).set_body_json(json!({"message": echo, "status": "fail"})),
        ),
        (
            "/e/400text/",
            ResponseTemplate::new(400).set_body_string(echo.clone()),
        ),
        (
            "/e/500/",
            ResponseTemplate::new(500).set_body_json(json!({"message": echo})),
        ),
        ("/e/fail/", ok(json!({"message": echo, "status": "fail"}))),
        (
            "/e/wait/",
            ok(
                json!({"message": format!("Please wait a few minutes {SESSION}"), "status": "fail"}),
            ),
        ),
        (
            "/e/429/",
            ResponseTemplate::new(429).set_body_string(echo.clone()),
        ),
        (
            "/e/html/",
            ResponseTemplate::new(200).set_body_raw(format!("<p>{echo}</p>"), "text/html"),
        ),
        (
            "/e/login/",
            ResponseTemplate::new(302)
                .insert_header("location", format!("/accounts/login/?s={SESSION}")),
        ),
        (
            "/e/redirect/",
            ResponseTemplate::new(302)
                .insert_header("location", format!("/elsewhere/{SESSION}?s={SESSION}")),
        ),
        (
            "/e/404/",
            ResponseTemplate::new(404).set_body_string(echo.clone()),
        ),
    ];
    for (route, template) in &routes {
        Mock::given(method("GET"))
            .and(path(*route))
            .respond_with(template.clone())
            .mount(&server)
            .await;
    }
    let client = client(&server);
    let client_debug = format!("{client:?}");
    assert!(!client_debug.contains(SESSION), "{client_debug}");
    let logs = capture_logs();
    for (route, _) in &routes {
        let err = client.get_json(route).await.unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains(SESSION), "{route}: {shown}");
        assert!(!shown.contains(CSRF), "{route}: {shown}");
    }
    let err = client
        .post_form("/e/400/", &[("x", SESSION)])
        .await
        .unwrap_err();
    assert!(!format!("{err} {err:?}").contains(SESSION));
    assert_no_secrets(&logs.text());
}

#[tokio::test]
async fn graphql_crawl_and_login_check_logs_hold_no_secrets() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &["v4/a/Posts.js"],
        )))
        .mount(&server)
        .await;
    mount_bundle(
        &server,
        "v4/a/Posts.js",
        &[(doc_ids::POSTS.name, "1110001")],
    )
    .await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 1, "C1", 10)],
        Some("c2"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/graphql/query"))
        .and(gql(&doc_ids::POSTS).after(json!("c2")))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;
    // The account form answers, but without a username: only an error remains.
    mount_form(&server, ok(json!({"form_data": {"email": EMAIL, "phone_number": PHONE, "birthday": BIRTHDAY}, "status": "ok"})), 1).await;
    mount_probe(&server, ResponseTemplate::new(429), 1).await;
    let logs = capture_logs();
    let client = client(&server);
    let err = run(&client, &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(matches!(err, Error::RateLimited(_)), "{err:?}");
    let err = client.current_user().await.unwrap_err();
    let shown = format!("{err} {err:?}");
    assert_no_secrets(&shown);
    let text = logs.text();
    assert!(
        text.contains("GraphQL request"),
        "debug logs were captured: {text}"
    );
    assert_no_secrets(&text);
    assert!(!text.contains(LSD), "{text}");
}

#[test]
fn client_and_futures_are_send_and_sync() {
    fn assert_send<T: Send>(_: &T) {}
    fn assert_sync<T: Send + Sync>() {}
    assert_sync::<Client>();
    let client = Client::new(ClientConfig::default(), None).unwrap();
    let target = post_target(POST_CODE, None);
    let options = opts();
    // Created but never polled: only the types are checked.
    assert_send(&resolve(&client, &target, &options, &never_archived));
    assert_send(&client.get_json("/api/v1/x/"));
    assert_send(&client.post_form("/api/v1/x/", &[]));
    assert_send(&client.current_user());
    assert_send(&client.user_by_username("x"));
}

#[tokio::test]
async fn unreachable_scripts_end_the_doc_id_lookup_early() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    let bundles: Vec<String> = (1..=30).map(|n| format!("v4/x/B{n}.js")).collect();
    let bundle_refs: Vec<&str> = bundles.iter().map(String::as_str).collect();
    Mock::given(method("GET"))
        .and(path("/alice/"))
        .respond_with(html(profile_html_with(
            &server,
            "alice",
            USER_ID,
            None,
            &bundle_refs,
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path_regex("^/rsrc.php/"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    mount_posts(
        &server,
        Value::Null,
        vec![image_item(&cdn, 1, "C1", 10)],
        None,
    )
    .await;
    let resolved = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    assert_eq!(keys(&resolved), vec!["1"], "the built-in doc id was used");
    assert_eq!(
        requests_to(&server, "/rsrc.php/").await.len(),
        3,
        "gave up after three failed scripts"
    );
}
