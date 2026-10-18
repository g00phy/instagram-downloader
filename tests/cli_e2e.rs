//! End-to-end tests: the `igdl` binary against a mocked Instagram API + CDN.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SESSION: &str = "SESSION-SECRET-0123456789";
const CSRF: &str = "CSRF-SECRET-abcdef";
const SHORTCODE: &str = "C0ffeeXYZ12";
/// 2024-05-01 12:00:00 UTC.
const TAKEN_AT: i64 = 1_714_564_800;

struct Env {
    server: MockServer,
    dir: TempDir,
}

impl Env {
    async fn new() -> Env {
        let env = Env {
            server: MockServer::start().await,
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(env.out()).unwrap();
        std::fs::write(env.config(), "").unwrap();
        std::fs::write(
            env.cookies(),
            format!(
                "# Netscape HTTP Cookie File\n\
                 .instagram.com\tTRUE\t/\tTRUE\t4102444800\tsessionid\t{SESSION}\n\
                 .instagram.com\tTRUE\t/\tTRUE\t4102444800\tcsrftoken\t{CSRF}\n\
                 .instagram.com\tTRUE\t/\tTRUE\t4102444800\tds_user_id\t999\n\
                 .example.com\tTRUE\t/\tFALSE\t4102444800\tother\tnot-for-instagram\n"
            ),
        )
        .unwrap();
        env
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }
    fn out(&self) -> PathBuf {
        self.root().join("out dir")
    }
    fn data(&self) -> PathBuf {
        self.root().join("data")
    }
    fn config(&self) -> PathBuf {
        self.root().join("config.toml")
    }
    fn cookies(&self) -> PathBuf {
        self.root().join("cookies.txt")
    }
    fn cdn(&self, file: &str) -> String {
        format!(
            "{}/cdn/{file}?oe=6650A1B2&oh=00_SIGNATURE&_nc_sid=abc",
            self.server.uri()
        )
    }

    /// Runs igdl with the mock API, isolated data/config dirs and no delays.
    async fn igdl(&self, args: &[&str]) -> Run {
        let mut command = Command::new(env!("CARGO_BIN_EXE_igdl"));
        command
            .args(args)
            .arg("--config")
            .arg(self.config())
            .arg("--api-delay")
            .arg("0")
            .arg("--no-progress")
            .env("IGDL_API_BASE", self.server.uri())
            .env("IGDL_DATA_DIR", self.data())
            .env_remove("IGDL_COOKIES")
            .env_remove("IGDL_LOG")
            .current_dir(self.root());
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        Run::from(output)
    }

    async fn igdl_auth(&self, args: &[&str]) -> Run {
        let cookies = self.cookies();
        let mut full: Vec<&str> = args.to_vec();
        full.push("--cookies");
        full.push(cookies.to_str().unwrap());
        self.igdl(&full).await
    }

    async fn cdn_file(&self, file: &str, body: &[u8], content_type: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/cdn/{file}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", content_type)
                    .set_body_bytes(body.to_vec()),
            )
            .mount(&self.server)
            .await;
    }

    fn queue(&self) -> Value {
        let text = std::fs::read_to_string(self.data().join("queue.json")).unwrap_or_default();
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// Every file igdl wrote, for secret scanning.
    fn written_text(&self) -> String {
        let mut text = String::new();
        for name in ["queue.json", "archive.txt"] {
            text.push_str(&std::fs::read_to_string(self.data().join(name)).unwrap_or_default());
        }
        text
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl From<Output> for Run {
    fn from(output: Output) -> Self {
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Run {
    fn assert_no_secrets(&self) {
        for secret in [SESSION, CSRF, "00_SIGNATURE"] {
            assert!(
                !self.stdout.contains(secret) && !self.stderr.contains(secret),
                "secret {secret} leaked:\nstdout: {}\nstderr: {}",
                self.stdout,
                self.stderr
            );
        }
    }

    fn debug(&self) -> String {
        format!(
            "exit {}\n--- stdout\n{}\n--- stderr\n{}",
            self.code, self.stdout, self.stderr
        )
    }
}

fn media_id(shortcode: &str) -> u64 {
    igdl::links::shortcode_to_media_id(shortcode).unwrap()
}

fn image(pk: &str, url: &str) -> Value {
    json!({
        "pk": pk,
        "media_type": 1,
        "image_versions2": {"candidates": [
            {"url": url.replace("/cdn/", "/cdn/small_"), "width": 320, "height": 320},
            {"url": url, "width": 1080, "height": 1350}
        ]}
    })
}

fn carousel_post(env: &Env, shortcode: &str, pk: &str, taken_at: i64) -> Value {
    let video_pk = format!("{pk}2");
    json!({
        "pk": pk,
        "id": format!("{pk}_123"),
        "code": shortcode,
        "media_type": 8,
        "taken_at": taken_at,
        "user": {"pk": "123", "username": "testuser"},
        "caption": {"text": "A caption"},
        "carousel_media": [
            image(&format!("{pk}1"), &env.cdn(&format!("{shortcode}_1.jpg"))),
            {
                "pk": video_pk,
                "media_type": 2,
                "image_versions2": {"candidates": [
                    {"url": env.cdn(&format!("{shortcode}_cover.jpg")), "width": 720, "height": 1280}
                ]},
                "video_versions": [
                    {"url": env.cdn(&format!("{shortcode}_2_low.mp4")), "width": 480, "height": 854, "type": 102},
                    {"url": env.cdn(&format!("{shortcode}_2.mp4")), "width": 720, "height": 1280, "type": 101}
                ]
            }
        ]
    })
}

fn single_post(env: &Env, shortcode: &str, pk: &str, taken_at: i64) -> Value {
    let mut post = image(pk, &env.cdn(&format!("{shortcode}.jpg")));
    let object = post.as_object_mut().unwrap();
    object.insert("code".into(), json!(shortcode));
    object.insert("taken_at".into(), json!(taken_at));
    object.insert("user".into(), json!({"pk": "123", "username": "testuser"}));
    post
}

async fn mount_media_info(env: &Env, shortcode: &str, item: Value) {
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/media/{}/info/", media_id(shortcode))))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"items": [item], "status": "ok"})),
        )
        .mount(&env.server)
        .await;
}

async fn mount_carousel(env: &Env) {
    mount_media_info(
        env,
        SHORTCODE,
        carousel_post(env, SHORTCODE, "3300000000000000001", TAKEN_AT),
    )
    .await;
    env.cdn_file(&format!("{SHORTCODE}_1.jpg"), b"JPEG-DATA-1", "image/jpeg")
        .await;
    env.cdn_file(&format!("{SHORTCODE}_2.mp4"), b"MP4-DATA-2", "video/mp4")
        .await;
}

fn post_url() -> String {
    format!("https://www.instagram.com/p/{SHORTCODE}/?igsh=tracking")
}

#[tokio::test(flavor = "multi_thread")]
async fn get_downloads_a_carousel_and_rerun_downloads_nothing() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let out = env.out();

    let run = env
        .igdl_auth(&["get", &post_url(), "-o", out.to_str().unwrap()])
        .await;
    assert_eq!(run.code, 0, "{}", run.debug());
    run.assert_no_secrets();

    let image = out.join("testuser/posts/2024-05-01_C0ffeeXYZ12_01.jpg");
    let video = out.join("testuser/posts/2024-05-01_C0ffeeXYZ12_02.mp4");
    assert_eq!(std::fs::read(&image).unwrap(), b"JPEG-DATA-1");
    assert_eq!(std::fs::read(&video).unwrap(), b"MP4-DATA-2");
    let printed: Vec<&str> = run.stdout.lines().collect();
    assert_eq!(printed.len(), 2, "{}", run.debug());
    assert!(printed.contains(&image.to_str().unwrap()));

    // The post date becomes the file's modification time.
    let mtime = std::fs::metadata(&image).unwrap().modified().unwrap();
    let secs = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert_eq!(secs as i64, TAKEN_AT);

    // Finished packages leave the queue; the archive remembers the files.
    assert_eq!(env.queue()["packages"].as_array().map(Vec::len), Some(0));
    let archive = std::fs::read_to_string(env.data().join("archive.txt")).unwrap();
    assert!(archive.contains("33000000000000000011"));
    assert!(!env.written_text().contains(SESSION));

    let again = env
        .igdl_auth(&["get", &post_url(), "-o", out.to_str().unwrap()])
        .await;
    assert_eq!(again.code, 0, "{}", again.debug());
    assert!(again.stdout.trim().is_empty(), "{}", again.debug());
    assert!(
        again.stderr.contains("already downloaded"),
        "{}",
        again.debug()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_prints_plan_and_writes_nothing() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let out = env.out();
    let run = env
        .igdl_auth(&[
            "get",
            &post_url(),
            "--dry-run",
            "-o",
            out.to_str().unwrap(),
            "--template",
            "{owner}/{shortcode}-{index}.{ext}",
        ])
        .await;
    assert_eq!(run.code, 0, "{}", run.debug());
    assert!(run.stdout.contains("C0ffeeXYZ12-01.jpg"), "{}", run.debug());
    assert!(run.stdout.contains("C0ffeeXYZ12-02.mp4"), "{}", run.debug());
    assert!(!out.join("testuser").exists());
    assert!(!env.data().join("queue.json").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn grab_list_start_through_the_queue() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/web_profile_info/"))
        .and(query_param("username", "testuser"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {"user": {"id": "123", "username": "testuser", "full_name": "Test",
                              "is_private": false, "followed_by_viewer": false}},
            "status": "ok"
        })))
        .mount(&env.server)
        .await;
    let posts = [
        single_post(&env, "Aaaaaaaaaa1", "3300000000000000101", TAKEN_AT + 300),
        single_post(&env, "Aaaaaaaaaa2", "3300000000000000102", TAKEN_AT + 200),
        single_post(&env, "Aaaaaaaaaa3", "3300000000000000103", TAKEN_AT + 100),
    ];
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/user/123/"))
        .and(query_param_is_missing("max_id"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [posts[0], posts[1]], "more_available": true, "next_max_id": "cursor-2",
            "status": "ok"
        })))
        .mount(&env.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/v1/feed/user/123/"))
        .and(query_param("max_id", "cursor-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "items": [posts[2]], "more_available": false, "status": "ok"
        })))
        .mount(&env.server)
        .await;
    for n in 1..=3 {
        env.cdn_file(
            &format!("Aaaaaaaaaa{n}.jpg"),
            format!("IMG{n}").as_bytes(),
            "image/jpeg",
        )
        .await;
    }
    let out = env.out();

    let grab = env
        .igdl_auth(&[
            "grab",
            "@TestUser",
            "--limit",
            "2",
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert_eq!(grab.code, 0, "{}", grab.debug());
    grab.assert_no_secrets();
    let queue = env.queue();
    let items = queue["packages"][0]["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().all(|i| i["status"] == "queued"));
    assert!(!out.join("testuser").exists(), "grab must not download");

    let list = env.igdl(&["list", "--items"]).await;
    assert_eq!(list.code, 0, "{}", list.debug());
    assert!(list.stdout.contains("testuser"), "{}", list.debug());
    assert!(list.stdout.contains("queued"), "{}", list.debug());

    let start = env.igdl_auth(&["start", "-j", "2"]).await;
    assert_eq!(start.code, 0, "{}", start.debug());
    assert_eq!(start.stdout.lines().count(), 2, "{}", start.debug());
    assert!(
        out.join("testuser/posts/2024-05-01_Aaaaaaaaaa1_01.jpg")
            .exists()
    );
    assert!(
        out.join("testuser/posts/2024-05-01_Aaaaaaaaaa2_01.jpg")
            .exists()
    );
    assert!(
        !out.join("testuser/posts/2024-05-01_Aaaaaaaaaa3_01.jpg")
            .exists()
    );
    let queue = env.queue();
    let items = queue["packages"][0]["items"].as_array().unwrap();
    assert!(items.iter().all(|i| i["status"] == "done"), "{queue:#}");

    let clear = env.igdl(&["clear"]).await;
    assert_eq!(clear.code, 0, "{}", clear.debug());
    assert_eq!(env.queue()["packages"].as_array().map(Vec::len), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_cdn_url_is_refreshed() {
    let env = Env::new().await;
    let code = "Bbbbbbbbbb1";
    // Refresh looks the post up by pk, which is the decoded shortcode.
    let pk = media_id(code).to_string();
    let stale = single_post(&env, code, &pk, TAKEN_AT);
    let mut fresh = stale.clone();
    fresh["image_versions2"]["candidates"][1]["url"] = json!(env.cdn("fresh.jpg"));
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/media/{}/info/", media_id(code))))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"items": [stale]})))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&env.server)
        .await;
    mount_media_info(&env, code, fresh).await;
    Mock::given(method("GET"))
        .and(path(format!("/cdn/{code}.jpg")))
        .respond_with(ResponseTemplate::new(403).set_body_string("URL signature expired"))
        .mount(&env.server)
        .await;
    env.cdn_file("fresh.jpg", b"FRESH", "image/jpeg").await;
    let out = env.out();

    let run = env
        .igdl_auth(&[
            "get",
            &format!("https://instagram.com/p/{code}"),
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert_eq!(run.code, 0, "{}", run.debug());
    let file = out.join(format!("testuser/posts/2024-05-01_{code}_01.jpg"));
    assert_eq!(std::fs::read(file).unwrap(), b"FRESH");
}

#[tokio::test(flavor = "multi_thread")]
async fn login_required_fails_with_hint_and_no_secrets() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/media/{}/info/", media_id(SHORTCODE))))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "message": "login_required", "require_login": true, "status": "fail"
        })))
        .mount(&env.server)
        .await;
    let out = env.out();
    let run = env
        .igdl_auth(&["get", &post_url(), "@second", "-o", out.to_str().unwrap()])
        .await;
    assert_eq!(run.code, 1, "{}", run.debug());
    assert!(run.stderr.contains("authentication"), "{}", run.debug());
    assert!(run.stderr.contains("hint:"), "{}", run.debug());
    assert!(run.stderr.contains("remaining link"), "{}", run.debug());
    run.assert_no_secrets();
    // The second link is never requested once authentication failed.
    let requests = env.server.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .all(|r| !r.url.path().contains("web_profile_info"))
    );
    // The API saw the cookies; the example.com cookie was never sent.
    let cookie_header = requests[0]
        .headers
        .get("cookie")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(cookie_header.contains(&format!("sessionid={SESSION}")));
    assert!(!cookie_header.contains("not-for-instagram"));
}

#[tokio::test(flavor = "multi_thread")]
async fn cookies_are_never_sent_to_the_cdn() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let out = env.out();
    let run = env
        .igdl_auth(&["get", &post_url(), "-o", out.to_str().unwrap()])
        .await;
    assert_eq!(run.code, 0, "{}", run.debug());
    let requests = env.server.received_requests().await.unwrap();
    let cdn: Vec<_> = requests
        .iter()
        .filter(|r| r.url.path().starts_with("/cdn/"))
        .collect();
    assert!(!cdn.is_empty());
    assert!(cdn.iter().all(|r| r.headers.get("cookie").is_none()));
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_cookie_file_is_a_usage_error() {
    let env = Env::new().await;
    std::fs::write(
        env.cookies(),
        format!(".instagram.com\tTRUE\t/\tTRUE\t1000000000\tsessionid\t{SESSION}\n"),
    )
    .unwrap();
    let run = env.igdl_auth(&["get", &post_url()]).await;
    assert_eq!(run.code, 2, "{}", run.debug());
    assert!(run.stderr.contains("expired"), "{}", run.debug());
    run.assert_no_secrets();
    assert!(env.server.received_requests().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn cookies_check_reports_names_only() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/999/info/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {"pk": "999", "username": "me_myself", "is_private": true},
            "status": "ok"
        })))
        .mount(&env.server)
        .await;
    let offline = env.igdl_auth(&["cookies", "check"]).await;
    assert_eq!(offline.code, 0, "{}", offline.debug());
    assert!(offline.stdout.contains("sessionid"), "{}", offline.debug());
    assert!(!offline.stdout.contains("other"), "{}", offline.debug());
    offline.assert_no_secrets();

    let online = env.igdl_auth(&["cookies", "check", "--online"]).await;
    assert_eq!(online.code, 0, "{}", online.debug());
    assert!(online.stdout.contains("@me_myself"), "{}", online.debug());
    online.assert_no_secrets();
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_links_fail_but_others_still_download() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let out = env.out();
    let links = env.root().join("links.txt");
    std::fs::write(
        &links,
        format!(
            "# my links\n- [post]({})\nhttps://example.com/p/abc/\n",
            post_url()
        ),
    )
    .unwrap();
    let run = env
        .igdl_auth(&[
            "get",
            "-i",
            links.to_str().unwrap(),
            "-o",
            out.to_str().unwrap(),
        ])
        .await;
    assert_eq!(run.code, 1, "{}", run.debug());
    assert!(run.stderr.contains("example.com"), "{}", run.debug());
    assert_eq!(run.stdout.lines().count(), 2, "{}", run.debug());
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_items_stay_queued_for_retry() {
    let env = Env::new().await;
    let code = "Cccccccccc1";
    mount_media_info(
        &env,
        code,
        single_post(&env, code, "3300000000000000301", TAKEN_AT),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("/cdn/{code}.jpg")))
        .respond_with(ResponseTemplate::new(404))
        .mount(&env.server)
        .await;
    let out = env.out();
    let url = format!("https://www.instagram.com/p/{code}/");
    let run = env
        .igdl_auth(&["get", &url, "-o", out.to_str().unwrap(), "--retries", "0"])
        .await;
    assert_eq!(run.code, 1, "{}", run.debug());
    let failed = env.igdl(&["list", "--failed"]).await;
    assert!(failed.stdout.contains("failed"), "{}", failed.debug());
    assert!(failed.stdout.contains("not_found"), "{}", failed.debug());

    env.server.reset().await;
    env.cdn_file(&format!("{code}.jpg"), b"NOW-OK", "image/jpeg")
        .await;
    mount_media_info(
        &env,
        code,
        single_post(&env, code, "3300000000000000301", TAKEN_AT),
    )
    .await;
    let retry = env.igdl(&["retry"]).await;
    assert_eq!(retry.code, 0, "{}", retry.debug());
    let start = env.igdl_auth(&["start"]).await;
    assert_eq!(start.code, 0, "{}", start.debug());
    assert_eq!(
        std::fs::read(out.join(format!("testuser/posts/2024-05-01_{code}_01.jpg"))).unwrap(),
        b"NOW-OK"
    );
}
