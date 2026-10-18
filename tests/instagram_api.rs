//! Instagram client + crawler against a mocked API (wiremock). No real network.
//!
//! All ids, usernames and URLs are made up; "CDN" URLs point at the mock server.

use std::time::{Duration, Instant};

use igdl::cookies::CookieJar;
use igdl::error::Error;
use igdl::instagram::{Client, ClientConfig, CrawlOptions, Resolved, refresh_url, resolve};
use igdl::links::{ProfileSection, Target, shortcode_to_media_id};
use igdl::model::{Category, MediaKind};
use serde_json::{Value, json};
use wiremock::matchers::{body_string, header, method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// A distinctive fake session id that must never show up in errors.
const SESSION: &str = "FAKESESSION-7f3a9c2e-DO-NOT-LEAK";
const CSRF: &str = "csrf-token-one";
const VIEWER_ID: &str = "999";
const USER_ID: &str = "1001";
const POST_CODE: &str = "CfakePost01";
const CAROUSEL_CODE: &str = "CfakeCarou1";
const REEL_CODE: &str = "CfakeReel01";

fn config(server: &MockServer) -> ClientConfig {
    ClientConfig {
        api_base: server.uri(),
        user_agent: "igdl-test/1.0".into(),
        api_delay: Duration::ZERO,
        rate_limit_retries: 2,
        rate_limit_wait: Duration::from_millis(1),
        timeout: Duration::from_secs(5),
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

fn feed_page(items: Vec<Value>, next: Option<&str>) -> Value {
    json!({
        "items": items,
        "num_results": 0,
        "more_available": next.is_some(),
        "next_max_id": next,
        "status": "ok"
    })
}

fn ok(body: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(body)
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

async fn mount_profile(
    server: &MockServer,
    username: &str,
    id: &str,
    private: bool,
    followed: bool,
) {
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", username))
        .respond_with(ok(json!({
            "data": {"user": {
                "id": id,
                "username": username,
                "full_name": "Fake Person",
                "is_private": private,
                "followed_by_viewer": followed
            }},
            "status": "ok"
        })))
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

fn keys(resolved: &Resolved) -> Vec<&str> {
    resolved.files.iter().map(|f| f.key.as_str()).collect()
}

// ---------------------------------------------------------------- client basics

#[tokio::test]
async fn required_headers_cookie_and_csrf_are_sent_and_single_image_resolves() {
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
    assert_eq!(header_of(request, "x-asbd-id"), "129477");
    assert_eq!(header_of(request, "x-ig-www-claim"), "0");
    assert_eq!(header_of(request, "x-requested-with"), "XMLHttpRequest");
    assert_eq!(header_of(request, "x-csrftoken"), CSRF);
    assert_eq!(header_of(request, "user-agent"), "igdl-test/1.0");
    assert_eq!(header_of(request, "referer"), format!("{}/", server.uri()));
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
async fn post_form_sends_urlencoded_body_with_csrf() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/v1/echo/"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(header("x-csrftoken", CSRF))
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
async fn requests_are_spaced_by_the_api_delay() {
    let server = MockServer::start().await;
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
        "the first request is not delayed"
    );
    client.get_json("/api/v1/b/").await.unwrap();
    // 250 ms delay plus 0–50 % jitter.
    assert!(started.elapsed() >= Duration::from_millis(250));
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
        resolved.files[0].url,
        format!("{cdn}/cdn/501_l.jpg?oe=6700AAAA&_nc_sid=fake")
    );
    assert_eq!(
        resolved.files[1].url,
        format!("{cdn}/cdn/502_720.mp4?oe=6700AAAA")
    );
    assert_eq!(resolved.files[1].width, Some(720));
    assert!(
        resolved
            .files
            .iter()
            .all(|f| f.caption.as_deref() == Some("three things"))
    );
    assert!(
        resolved
            .files
            .iter()
            .all(|f| f.shortcode.as_deref() == Some(CAROUSEL_CODE))
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
    assert_eq!(resolved.files[0].index, 2);
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
    assert_eq!(resolved.files[0].caption, None);
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

// ---------------------------------------------------------------- profiles

fn posts_page_path() -> String {
    format!("/api/v1/feed/user/{USER_ID}/")
}

#[tokio::test]
async fn profile_posts_paginate_and_respect_limit() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, false, false).await;
    let page = |first: u64, next: Option<&str>| {
        feed_page(
            (first..first + 3)
                .map(|pk| image_item(&cdn, pk, &format!("Code{pk}"), 1_700_000_000 - pk as i64))
                .collect(),
            next,
        )
    };
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("count", "12"))
        .and(query_param_is_missing("max_id"))
        .respond_with(ok(page(1, Some("cursor-1"))))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "cursor-1"))
        .respond_with(ok(page(4, Some("cursor-2"))))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "cursor-2"))
        .respond_with(ok(page(7, None)))
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
    mount_profile(&server, "alice", USER_ID, false, false).await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param_is_missing("max_id"))
        .respond_with(ok(feed_page(
            vec![image_item(&cdn, 1, "C1", 10)],
            Some("n1"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "n1"))
        .respond_with(ok(feed_page(vec![video_item(&cdn, 2, "C2", 9)], None)))
        .mount(&server)
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
    mount_profile(&server, "alice", USER_ID, false, false).await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param_is_missing("max_id"))
        .respond_with(ok(feed_page(
            vec![
                pinned(image_item(&cdn, 10, "Pinned10", 100)), // old but pinned: skipped, no stop
                image_item(&cdn, 11, "New11", 5000),           // newer than until
                image_item(&cdn, 12, "In12", 3000),
                image_item(&cdn, 13, "In13", 2000),
            ],
            Some("p2"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "p2"))
        .respond_with(ok(feed_page(
            vec![
                image_item(&cdn, 14, "In14", 1500),
                image_item(&cdn, 15, "Old15", 500), // older than since: stop here
                image_item(&cdn, 16, "Old16", 400),
            ],
            Some("p3"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "p3"))
        .respond_with(ok(feed_page(vec![], None)))
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
    mount_profile(&server, "alice", USER_ID, false, false).await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param_is_missing("max_id"))
        .respond_with(ok(feed_page(
            vec![
                pinned(image_item(&cdn, 50, "Pin50", 900)), // archived but pinned
                image_item(&cdn, 51, "New51", 800),
                image_item(&cdn, 52, "Old52", 700), // archived: stop
                image_item(&cdn, 53, "Old53", 600),
            ],
            Some("next"),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .and(query_param("max_id", "next"))
        .respond_with(ok(feed_page(vec![], None)))
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
async fn reels_are_fetched_with_form_posts_and_deduped_against_posts() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, false, false).await;
    let clip = |pk: u64| {
        let mut item = video_item(&cdn, pk, &format!("Reel{pk}"), 1000 - pk as i64);
        item["product_type"] = json!("clips");
        item
    };
    // The posts feed also contains reel 201.
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .respond_with(ok(feed_page(
            vec![clip(201), image_item(&cdn, 1, "Img1", 900)],
            None,
        )))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/clips/user/"))
        .and(header("content-type", "application/x-www-form-urlencoded"))
        .and(body_string(format!(
            "target_user_id={USER_ID}&page_size=12"
        )))
        .respond_with(ok(json!({
            "items": [{"media": clip(201)}, {"media": clip(202)}],
            "paging_info": {"more_available": true, "max_id": "reels-2"},
            "status": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/v1/clips/user/"))
        .and(body_string(format!(
            "target_user_id={USER_ID}&page_size=12&max_id=reels-2"
        )))
        .respond_with(ok(json!({
            "items": [{"media": clip(203)}],
            "paging_info": {"more_available": false, "max_id": null},
            "status": "ok"
        })))
        .expect(1)
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
}

#[tokio::test]
async fn tagged_posts_keep_their_owner_and_category() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, false, false).await;
    let mut item = image_item(&cdn, 401, "Tag401", 1000);
    item["user"] = json!({"pk": 2002, "username": "carol"});
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/usertags/{USER_ID}/feed/")))
        .and(query_param("count", "12"))
        .respond_with(ok(feed_page(vec![item], None)))
        .expect(1)
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
    assert_eq!(resolved.files.len(), 1);
    assert_eq!(resolved.files[0].category, Category::Tagged);
    assert_eq!(resolved.files[0].owner, "carol");
    assert_eq!(resolved.files[0].owner_id.as_deref(), Some("2002"));
}

#[tokio::test]
async fn private_profile_is_refused() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, true, false).await;
    let err = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Private(m) if m.contains("@alice")),
        "{err:?}"
    );
}

#[tokio::test]
async fn private_profile_followed_is_crawled() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, true, true).await;
    Mock::given(method("GET"))
        .and(path(posts_page_path()))
        .respond_with(ok(feed_page(vec![], None)))
        .expect(1)
        .mount(&server)
        .await;
    let resolved = run(&client(&server), &profile_target(None), &opts())
        .await
        .unwrap();
    assert!(resolved.files.is_empty());
}

#[tokio::test]
async fn unknown_user_is_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "ghost"))
        .respond_with(ok(json!({"data": {"user": null}, "status": "ok"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "gone"))
        .respond_with(ResponseTemplate::new(404).set_body_string(""))
        .mount(&server)
        .await;
    let client = client(&server);
    for name in ["ghost", "gone"] {
        let err = client.user_by_username(name).await.unwrap_err();
        assert!(
            matches!(&err, Error::NotFound(m) if m == &format!("user @{name}")),
            "{err:?}"
        );
    }
}

// ---------------------------------------------------------------- stories & highlights

#[tokio::test]
async fn stories_resolve_filter_by_id_and_report_empty() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, false, false).await;
    mount_profile(&server, "quiet", "3003", false, false).await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/reels_media/"))
        .and(query_param("reel_ids", USER_ID))
        .respond_with(ok(json!({
            "reels_media": [{
                "id": 1001,
                "user": {"pk": 1001, "username": "alice"},
                "items": [image_item(&cdn, 601, "StoryCode1", 1_700_000_100),
                          video_item(&cdn, 602, "StoryCode2", 1_700_000_200)]
            }],
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
    assert_eq!(all.files[1].kind, MediaKind::Video);

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
}

#[tokio::test]
async fn stories_accept_the_reels_map_shape() {
    let server = MockServer::start().await;
    let cdn = server.uri();
    mount_profile(&server, "alice", USER_ID, false, false).await;
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

/// Answers `reels_media` requests for highlights with one image per requested id.
fn highlight_responder(cdn: String) -> impl Fn(&Request) -> ResponseTemplate + Send + Sync {
    move |request: &Request| {
        let mut reels = serde_json::Map::new();
        for (key, id) in request.url.query_pairs() {
            if key != "reel_ids" {
                continue;
            }
            let number: u64 = id.trim_start_matches("highlight:").parse().unwrap();
            reels.insert(
                id.to_string(),
                json!({
                    "id": id,
                    "title": format!("server title {number}"),
                    "user": {"pk": 1001, "username": "alice"},
                    "items": [image_item(&cdn, 9000 + number, "HlCode", 1)]
                }),
            );
        }
        ok(json!({"reels": reels, "status": "ok"}))
    }
}

#[tokio::test]
async fn highlights_tray_is_fetched_in_chunks_with_titles() {
    let server = MockServer::start().await;
    mount_profile(&server, "alice", USER_ID, false, false).await;
    let tray: Vec<Value> = (1..=7)
        .map(|n| json!({"id": format!("highlight:{n}"), "title": format!("Trip {n}")}))
        .collect();
    Mock::given(method("GET"))
        .and(path(format!(
            "/api/v1/highlights/{USER_ID}/highlights_tray/"
        )))
        .respond_with(ok(json!({"tray": tray, "status": "ok"})))
        .mount(&server)
        .await;
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
    let chunks: Vec<usize> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == "/api/v1/feed/reels_media/")
        .map(|r| r.url.query_pairs().filter(|(k, _)| k == "reel_ids").count())
        .collect();
    assert_eq!(chunks, vec![5, 2]);

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

// ---------------------------------------------------------------- error classification

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

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", "/challenge/?next=/"))
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
async fn rate_limit_then_success_is_retried() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429).set_body_string("Too Many Requests"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ok(
            json!({"message": "Please wait a few minutes before you try again.", "status": "fail"}),
        ))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ok(json!({"status": "ok", "value": 7})))
        .with_priority(3)
        .mount(&server)
        .await;
    let value = client(&server).get_json("/api/v1/thing/").await.unwrap();
    assert_eq!(value["value"], json!(7));
    assert_eq!(server.received_requests().await.unwrap().len(), 3);
}

#[tokio::test]
async fn persistent_rate_limit_gives_up() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(429))
        .expect(3) // first try + rate_limit_retries (2)
        .mount(&server)
        .await;
    let err = get_err(&server).await;
    assert!(matches!(err, Error::RateLimited(_)), "{err:?}");
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
        .expect(3)
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

// ---------------------------------------------------------------- current user, refresh

/// Config whose rate-limit backoff would take minutes: "who am I" must not use it.
fn slow_backoff_config(server: &MockServer) -> ClientConfig {
    ClientConfig {
        rate_limit_wait: Duration::from_secs(30),
        rate_limit_retries: 3,
        ..config(server)
    }
}

async fn mount_viewer_endpoints(
    server: &MockServer,
    current: ResponseTemplate,
    by_id: ResponseTemplate,
    by_id_calls: u64,
) {
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/current_user/"))
        .and(query_param("edit", "true"))
        .respond_with(current)
        .expect(1)
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/users/{VIEWER_ID}/info/")))
        .respond_with(by_id)
        .expect(by_id_calls)
        .mount(server)
        .await;
}

#[tokio::test]
async fn current_user_prefers_the_current_user_endpoint() {
    let server = MockServer::start().await;
    mount_viewer_endpoints(
        &server,
        ok(json!({"user": {"pk": "999", "username": "viewer", "full_name": "View Er"}, "status": "ok"})),
        ok(json!({"user": {"pk": 999, "username": "unused"}, "status": "ok"})),
        0,
    )
    .await;
    let me = client(&server).current_user().await.unwrap();
    assert_eq!((me.id.as_str(), me.username.as_str()), ("999", "viewer"));
    assert_eq!(me.full_name.as_deref(), Some("View Er"));
}

#[tokio::test]
async fn current_user_falls_back_to_the_user_id_endpoint_without_backoff() {
    let server = MockServer::start().await;
    mount_viewer_endpoints(
        &server,
        ResponseTemplate::new(429),
        ok(json!({"user": {"pk": 999, "username": "viewer"}, "status": "ok"})),
        1,
    )
    .await;
    let client = Client::new(slow_backoff_config(&server), Some(jar())).unwrap();
    let started = std::time::Instant::now();
    let me = client.current_user().await.unwrap();
    assert_eq!(me.username, "viewer");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no rate-limit backoff"
    );
}

#[tokio::test]
async fn current_user_reports_a_rate_limit_quickly_when_every_endpoint_is_throttled() {
    let server = MockServer::start().await;
    mount_viewer_endpoints(
        &server,
        ResponseTemplate::new(429),
        ResponseTemplate::new(429),
        1,
    )
    .await;
    let client = Client::new(slow_backoff_config(&server), Some(jar())).unwrap();
    let started = std::time::Instant::now();
    let err = client.current_user().await.unwrap_err();
    assert!(matches!(err, Error::RateLimited(_)), "{err:?}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "no rate-limit backoff"
    );
    assert!(!format!("{err} {err:?}").contains(SESSION));
}

#[tokio::test]
async fn current_user_stops_at_an_authentication_failure() {
    let server = MockServer::start().await;
    mount_viewer_endpoints(
        &server,
        ResponseTemplate::new(401).set_body_json(
            json!({"message": "login_required", "require_login": true, "status": "fail"}),
        ),
        ok(json!({"user": {"pk": 999, "username": "unused"}, "status": "ok"})),
        0,
    )
    .await;
    let err = client(&server).current_user().await.unwrap_err();
    assert!(matches!(err, Error::Auth(_)), "{err:?}");
}

#[tokio::test]
async fn current_user_without_ds_user_id_uses_only_the_current_user_endpoint() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/accounts/current_user/"))
        .and(query_param("edit", "true"))
        .respond_with(ok(
            json!({"user": {"pk": "998", "username": "fallback"}, "status": "ok"}),
        ))
        .expect(1)
        .mount(&server)
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
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
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
}
