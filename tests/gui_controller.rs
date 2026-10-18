#![cfg(feature = "gui")]
//! The GUI controller (no window) against a mocked Instagram API + CDN: a real
//! `ControllerHandle` is driven through its channels like the window does.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use igdl::cli::GlobalArgs;
use igdl::config::{self, Settings};
use igdl::gui::controller::{ControllerConfig, ControllerHandle, UiLink};
use igdl::gui::protocol::{
    AccountStatus, Command, GrabRequest, LogLevel, LogLine, StagedPackage, Update,
};
use igdl::links::ProfileSection;
use igdl::model::ItemStatus;
use igdl::naming::DEFAULT_TEMPLATE;
use igdl::queue::{Queue, QueueStore};
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param, query_param_is_missing};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A distinctive fake session cookie: it must never show up in the window.
const SESSION: &str = "SESSIONID-DISTINCTIVE-7f3a9c";
const CSRF: &str = "CSRF-SECRET-abcdef";
/// Signature parameter of the fake signed CDN URLs.
const SIGNATURE: &str = "00_SIGNATURE";
const SHORTCODE: &str = "C0ffeeXYZ12";
/// 2024-05-01 12:00:00 UTC.
const TAKEN_AT: i64 = 1_714_564_800;
/// Generous: the controller usually answers within milliseconds.
const WAIT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Harness

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cookies {
    None,
    Valid,
    Expired,
}

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
    fn config_path(&self) -> PathBuf {
        self.root().join("config").join("config.toml")
    }
    fn cookies(&self) -> PathBuf {
        self.root().join("cookies.txt")
    }
    fn cdn(&self, file: &str) -> String {
        format!(
            "{}/cdn/{file}?oe=6650A1B2&oh={SIGNATURE}&_nc_sid=abc",
            self.server.uri()
        )
    }

    fn config(&self, cookies: Cookies) -> ControllerConfig {
        let expires: i64 = if cookies == Cookies::Expired {
            1_000_000_000
        } else {
            4_102_444_800
        };
        std::fs::write(
            self.cookies(),
            format!(
                "# Netscape HTTP Cookie File\n\
                 .instagram.com\tTRUE\t/\tTRUE\t{expires}\tsessionid\t{SESSION}\n\
                 .instagram.com\tTRUE\t/\tTRUE\t{expires}\tcsrftoken\t{CSRF}\n\
                 .instagram.com\tTRUE\t/\tTRUE\t{expires}\tds_user_id\t999\n"
            ),
        )
        .unwrap();
        ControllerConfig {
            global: GlobalArgs {
                data_dir: Some(self.data()),
                cookies: (cookies != Cookies::None).then(|| self.cookies()),
                api_base: Some(self.server.uri()),
                api_delay: Some(0.0),
                ..GlobalArgs::default()
            },
            settings: Settings::default(),
            config_path: Some(self.config_path()),
        }
    }

    fn start(&self, cookies: Cookies) -> Ui {
        Ui::start(self.config(cookies))
    }

    fn request(&self, text: &str) -> GrabRequest {
        GrabRequest {
            text: text.to_string(),
            sections: vec![ProfileSection::Posts],
            limit: None,
            only: None,
            since: String::new(),
            until: String::new(),
            update: false,
            out_dir: self.out(),
            template: DEFAULT_TEMPLATE.to_string(),
        }
    }

    async fn cdn_file(&self, file: &str, body: &[u8]) {
        Mock::given(method("GET"))
            .and(path(format!("/cdn/{file}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/octet-stream")
                    .set_body_bytes(body.to_vec()),
            )
            .mount(&self.server)
            .await;
    }

    async fn requests_to(&self, prefix: &str) -> usize {
        self.server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|request| request.url.path().starts_with(prefix))
            .count()
    }
}

/// The window's side: sends commands and records every update.
struct Ui {
    handle: Option<ControllerHandle>,
    link: UiLink,
    history: Vec<Update>,
    /// Waits look at updates from here on (set by `send` and `next`).
    mark: usize,
}

impl Ui {
    fn start(config: ControllerConfig) -> Ui {
        let (handle, link) = ControllerHandle::spawn(config, Arc::new(|| {}));
        Ui {
            handle: Some(handle),
            link,
            history: Vec::new(),
            mark: 0,
        }
    }

    /// Sends a command; later waits only see updates that arrive after it.
    fn send(&mut self, command: Command) {
        self.receive_pending();
        self.mark = self.history.len();
        self.link.commands.send(command).unwrap();
    }

    fn receive_pending(&mut self) {
        while let Ok(update) = self.link.updates.try_recv() {
            self.history.push(update);
        }
    }

    /// Index of the first update since the mark that matches.
    fn find(&mut self, what: &str, matches: impl Fn(&Update) -> bool) -> usize {
        if let Some(pos) = self.history[self.mark..].iter().position(&matches) {
            return self.mark + pos;
        }
        let deadline = Instant::now() + WAIT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.link.updates.recv_timeout(remaining) {
                Ok(update) => {
                    let found = matches(&update);
                    self.history.push(update);
                    if found {
                        return self.history.len() - 1;
                    }
                }
                Err(_) => panic!(
                    "timed out waiting for {what}; updates since the last command:\n{}",
                    self.history[self.mark..]
                        .iter()
                        .map(describe)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
            }
        }
    }

    /// The first matching update since the mark (order-independent checks).
    fn wait_for(&mut self, what: &str, matches: impl Fn(&Update) -> bool) -> Update {
        let index = self.find(what, matches);
        self.history[index].clone()
    }

    /// Like `wait_for`, then only later updates are considered (ordered checks).
    fn next(&mut self, what: &str, matches: impl Fn(&Update) -> bool) -> Update {
        let index = self.find(what, matches);
        self.mark = index + 1;
        self.history[index].clone()
    }

    fn wait_log(&mut self, level: LogLevel, needle: &str) -> LogLine {
        let what = format!("{level:?} log containing {needle:?}");
        match self.wait_for(&what, |u| is_log(u, level, needle)) {
            Update::Log(line) => line,
            _ => unreachable!(),
        }
    }

    /// The first `Queue` update since the mark that satisfies `check`.
    fn wait_queue(&mut self, what: &str, check: impl Fn(&Queue) -> bool) -> Arc<Queue> {
        match self.wait_for(what, |u| matches!(u, Update::Queue(q) if check(q))) {
            Update::Queue(queue) => queue,
            _ => unreachable!(),
        }
    }

    fn wait_staged(
        &mut self,
        what: &str,
        check: impl Fn(&[StagedPackage]) -> bool,
    ) -> Vec<StagedPackage> {
        match self.wait_for(what, |u| matches!(u, Update::Staged(s) if check(s))) {
            Update::Staged(staged) => staged,
            _ => unreachable!(),
        }
    }

    /// The most recent `Queue` update received so far.
    fn latest_queue(&self) -> Arc<Queue> {
        self.history
            .iter()
            .rev()
            .find_map(|u| match u {
                Update::Queue(queue) => Some(Arc::clone(queue)),
                _ => None,
            })
            .expect("a Queue update")
    }

    /// Records whatever arrives within `duration`.
    fn settle(&mut self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            match self.link.updates.recv_timeout(remaining) {
                Ok(update) => self.history.push(update),
                Err(_) => break,
            }
        }
    }

    /// Updates since the mark.
    fn recent(&self) -> &[Update] {
        &self.history[self.mark..]
    }

    /// Grabs `request` and returns the staged package it produced.
    fn grab_one(&mut self, request: GrabRequest) -> StagedPackage {
        self.send(Command::Grab(request));
        self.wait_for("idle LinkGrabber", |u| {
            matches!(u, Update::Grabbing { pending_links: 0 })
        });
        let staged = self.wait_staged("a staged package", |s| !s.is_empty());
        staged.last().cloned().unwrap()
    }

    fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown();
        }
        self.receive_pending();
    }

    /// Shuts down and checks that no secret reached the window.
    fn finish(mut self) {
        self.shutdown();
        for update in &self.history {
            let text = match update {
                Update::Log(line) => line.text.clone(),
                Update::Account(AccountStatus::Invalid(message)) | Update::Fatal(message) => {
                    message.clone()
                }
                _ => continue,
            };
            for secret in [SESSION, CSRF, SIGNATURE] {
                assert!(!text.contains(secret), "{secret} leaked: {text}");
            }
        }
    }
}

fn is_log(update: &Update, level: LogLevel, needle: &str) -> bool {
    matches!(update, Update::Log(line) if line.level == level && line.text.contains(needle))
}

fn describe(update: &Update) -> String {
    match update {
        Update::Queue(queue) => {
            let packages: Vec<String> = queue
                .packages
                .iter()
                .map(|package| {
                    let items: Vec<String> = package
                        .items
                        .iter()
                        .map(|item| format!("{}:{:?}", item.id, item.status))
                        .collect();
                    format!("#{} [{}]", package.id, items.join(" "))
                })
                .collect();
            format!("Queue({})", packages.join(", "))
        }
        Update::Staged(staged) => {
            let packages: Vec<String> = staged
                .iter()
                .map(|p| format!("#{} {} ({} files)", p.id, p.name, p.files.len()))
                .collect();
            format!("Staged([{}])", packages.join(", "))
        }
        Update::Settings { .. } => "Settings".to_string(),
        other => format!("{other:?}"),
    }
}

fn kind(update: &Update) -> &'static str {
    match update {
        Update::Settings { .. } => "Settings",
        Update::Queue(_) => "Queue",
        Update::Progress { .. } => "Progress",
        Update::Staged(_) => "Staged",
        Update::Grabbing { .. } => "Grabbing",
        Update::Downloading(_) => "Downloading",
        Update::Account(_) => "Account",
        Update::Log(_) => "Log",
        Update::Fatal(_) => "Fatal",
    }
}

/// Waits for the start-up updates and returns the non-log ones in order.
fn startup(ui: &mut Ui) -> Vec<Update> {
    ui.next("start-up", |u| matches!(u, Update::Downloading(false)));
    ui.history
        .iter()
        .filter(|u| !matches!(u, Update::Log(_)))
        .cloned()
        .collect()
}

fn all_items_are(queue: &Queue, status: ItemStatus) -> bool {
    let mut items = queue.items().peekable();
    items.peek().is_some() && items.all(|item| item.status == status)
}

// ---------------------------------------------------------------------------
// Fixtures (same shapes as tests/cli_e2e.rs)

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
                "pk": format!("{pk}2"),
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

fn single_post_at(shortcode: &str, pk: &str, taken_at: i64, url: &str) -> Value {
    let mut post = image(pk, url);
    let object = post.as_object_mut().unwrap();
    object.insert("code".into(), json!(shortcode));
    object.insert("taken_at".into(), json!(taken_at));
    object.insert("user".into(), json!({"pk": "123", "username": "testuser"}));
    post
}

fn single_post(env: &Env, shortcode: &str, pk: &str, taken_at: i64) -> Value {
    single_post_at(
        shortcode,
        pk,
        taken_at,
        &env.cdn(&format!("{shortcode}.jpg")),
    )
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
    env.cdn_file(&format!("{SHORTCODE}_1.jpg"), b"JPEG-DATA-1")
        .await;
    env.cdn_file(&format!("{SHORTCODE}_2.mp4"), b"MP4-DATA-2")
        .await;
}

fn post_url(shortcode: &str) -> String {
    format!("https://www.instagram.com/p/{shortcode}/?igsh=tracking")
}

fn carousel_paths(env: &Env) -> [PathBuf; 2] {
    [
        env.out()
            .join("testuser/posts/2024-05-01_C0ffeeXYZ12_01.jpg"),
        env.out()
            .join("testuser/posts/2024-05-01_C0ffeeXYZ12_02.mp4"),
    ]
}

/// A CDN that sends the headers and 64 KiB of a 1 MB body, then stalls.
fn stalling_cdn() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => request.extend_from_slice(&buffer[..n]),
                    }
                }
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: image/jpeg\r\ncontent-length: 1000000\r\n\r\n",
                );
                let _ = stream.write_all(&[b'x'; 64 * 1024]);
                let _ = stream.flush();
                std::thread::sleep(Duration::from_secs(60));
            });
        }
    });
    format!("http://{address}")
}

/// Mounts a post whose image is served by [`stalling_cdn`].
async fn mount_stalling_post(env: &Env, shortcode: &str) {
    let url = format!("{}/stall.jpg?oe=6650A1B2&oh={SIGNATURE}", stalling_cdn());
    mount_media_info(
        env,
        shortcode,
        single_post_at(shortcode, "3300000000000000401", TAKEN_AT, &url),
    )
    .await;
}

// ---------------------------------------------------------------------------
// Start-up

#[tokio::test]
async fn startup_sends_state_in_order_without_cookies() {
    let env = Env::new().await;
    let mut ui = env.start(Cookies::None);
    let updates = startup(&mut ui);
    let kinds: Vec<&str> = updates.iter().map(kind).collect();
    assert_eq!(
        kinds,
        [
            "Settings",
            "Account",
            "Queue",
            "Staged",
            "Grabbing",
            "Downloading"
        ]
    );
    match &updates[0] {
        Update::Settings {
            settings,
            config_path,
            default_out_dir,
        } => {
            assert_eq!(**settings, Settings::default());
            assert_eq!(config_path.as_deref(), Some(env.config_path().as_path()));
            assert!(default_out_dir.is_absolute());
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        updates[1],
        Update::Account(AccountStatus::NoCookies)
    ));
    assert!(matches!(&updates[2], Update::Queue(q) if q.packages.is_empty()));
    assert!(matches!(&updates[3], Update::Staged(s) if s.is_empty()));
    assert!(matches!(updates[4], Update::Grabbing { pending_links: 0 }));
    ui.mark = 0;
    ui.wait_log(LogLevel::Warn, "no cookies configured");

    ui.send(Command::CheckAccount);
    ui.wait_for("NoCookies", |u| {
        matches!(u, Update::Account(AccountStatus::NoCookies))
    });
    ui.finish();
}

#[tokio::test]
async fn startup_with_cookies_reports_them_loaded() {
    let env = Env::new().await;
    let mut ui = env.start(Cookies::Valid);
    let updates = startup(&mut ui);
    assert!(matches!(updates[1], Update::Account(AccountStatus::Loaded)));
    ui.finish();
}

#[tokio::test]
async fn startup_with_expired_cookies_reports_them_invalid() {
    let env = Env::new().await;
    let mut ui = env.start(Cookies::Expired);
    let updates = startup(&mut ui);
    match &updates[1] {
        Update::Account(AccountStatus::Invalid(message)) => {
            assert!(message.contains("expired"), "{message}");
        }
        other => panic!("{}", describe(other)),
    }
    ui.mark = 0;
    ui.wait_log(LogLevel::Error, "expired");
    ui.finish();
}

#[tokio::test]
async fn locked_queue_is_fatal() {
    let env = Env::new().await;
    let _other = QueueStore::open(&env.data()).unwrap();
    let mut ui = env.start(Cookies::Valid);
    let update = ui.wait_for("Fatal", |u| matches!(u, Update::Fatal(_)));
    let Update::Fatal(message) = update else {
        unreachable!()
    };
    assert!(
        message.starts_with("Another igdl is using the download list in"),
        "{message}"
    );
    assert!(message.contains(&env.data().display().to_string()));
    assert!(
        ui.history
            .iter()
            .all(|u| !matches!(u, Update::Settings { .. }))
    );
    // It keeps draining commands and stops on Shutdown.
    ui.send(Command::Start);
    let started = Instant::now();
    ui.finish();
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Saves a queue with one package of one item (media key `key`) in `status`.
fn seed_queue(env: &Env, key: &str, status: ItemStatus) -> u64 {
    let mut queue = Queue::default();
    let media: igdl::model::MediaFile = serde_json::from_value(json!({
        "key": key, "post_id": key, "shortcode": null, "index": 1, "count": 1,
        "kind": "image", "category": "post", "url": env.cdn("x.jpg"),
        "width": null, "height": null, "owner": "testuser", "owner_id": null,
        "taken_at": TAKEN_AT, "caption": null, "highlight": null, "ext": "jpg"
    }))
    .unwrap();
    let package = queue
        .add_package(
            "p".into(),
            "src".into(),
            env.out(),
            vec![(media, PathBuf::from("x.jpg"))],
        )
        .unwrap();
    queue.packages[0].items[0].status = status;
    QueueStore::open(&env.data()).unwrap().save(&queue).unwrap();
    package
}

#[tokio::test]
async fn interrupted_items_are_requeued_at_startup() {
    let env = Env::new().await;
    let package = seed_queue(&env, "1", ItemStatus::Downloading);

    let mut ui = env.start(Cookies::Valid);
    let updates = startup(&mut ui);
    let Update::Queue(queue) = &updates[2] else {
        panic!()
    };
    assert_eq!(queue.packages[0].id, package);
    assert!(all_items_are(queue, ItemStatus::Queued));
    ui.mark = 0;
    ui.wait_log(LogLevel::Info, "1 interrupted download(s)");
    ui.finish();
}

#[tokio::test]
async fn archived_items_are_skipped_instead_of_downloaded() {
    let env = Env::new().await;
    seed_queue(&env, "4400000000000000001", ItemStatus::Queued);
    std::fs::write(env.data().join("archive.txt"), "4400000000000000001\n").unwrap();
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    ui.send(Command::Start);
    let queue = ui.wait_queue("a skipped item", |q| all_items_are(q, ItemStatus::Skipped));
    assert_eq!(
        queue.packages[0].items[0].note.as_deref(),
        Some("already downloaded (archive)")
    );
    ui.wait_log(LogLevel::Info, "1 file(s) skipped");
    ui.settle(Duration::from_millis(200));
    assert!(
        ui.recent()
            .iter()
            .all(|u| !matches!(u, Update::Downloading(true)))
    );
    assert_eq!(env.requests_to("/cdn/").await, 0);
    ui.finish();
}

// ---------------------------------------------------------------------------
// LinkGrabber

#[tokio::test]
async fn grabbing_a_carousel_stages_its_files() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    let link = post_url(SHORTCODE);
    let package = ui.grab_one(env.request(&format!("look: {link}\n{link}")));
    assert!(
        ui.recent()
            .iter()
            .any(|u| matches!(u, Update::Grabbing { pending_links: 1 })),
        "the duplicate link is grabbed once"
    );
    assert_eq!(package.id, 1);
    assert_eq!(package.source, link);
    assert_eq!(package.out_dir, env.out());
    assert_eq!((package.archived, package.already_queued), (0, 0));
    let rel_paths: Vec<PathBuf> = package.files.iter().map(|f| f.rel_path.clone()).collect();
    assert_eq!(
        rel_paths,
        [
            PathBuf::from("testuser/posts/2024-05-01_C0ffeeXYZ12_01.jpg"),
            PathBuf::from("testuser/posts/2024-05-01_C0ffeeXYZ12_02.mp4"),
        ]
    );
    assert_eq!(package.files[0].media.key, "33000000000000000011");
    ui.wait_log(LogLevel::Info, "2 file(s)");
    assert_eq!(
        env.requests_to("/cdn/").await,
        0,
        "grabbing must not download"
    );

    ui.send(Command::Discard { staged: vec![] });
    ui.wait_staged("an empty LinkGrabber", <[StagedPackage]>::is_empty);
    ui.finish();
}

#[tokio::test]
async fn profile_grab_respects_the_limit() {
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
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    let mut request = env.request("@TestUser");
    request.limit = Some(2);
    let package = ui.grab_one(request);
    assert!(package.name.contains("testuser"), "{}", package.name);
    let rel_paths: Vec<String> = package
        .files
        .iter()
        .map(|f| f.rel_path.to_string_lossy().replace('\\', "/"))
        .collect();
    assert_eq!(
        rel_paths,
        [
            "testuser/posts/2024-05-01_Aaaaaaaaaa1_01.jpg",
            "testuser/posts/2024-05-01_Aaaaaaaaaa2_01.jpg"
        ]
    );
    ui.finish();
}

#[tokio::test]
async fn bad_links_are_logged_and_the_others_staged() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    let text = format!(
        "https://example.com/p/abc/\n@bad!name\n- [post]({})",
        post_url(SHORTCODE)
    );
    let package = ui.grab_one(env.request(&text));
    assert_eq!(package.files.len(), 2);
    ui.wait_log(
        LogLevel::Error,
        "https://example.com/p/abc/: unsupported link",
    );
    ui.wait_log(LogLevel::Error, "@bad!name: unsupported link");
    assert!(
        ui.recent()
            .iter()
            .any(|u| matches!(u, Update::Grabbing { pending_links: 3 }))
    );

    ui.send(Command::Grab(env.request("   \n# only a comment\n")));
    ui.wait_log(LogLevel::Warn, "no links found");

    let mut request = env.request("natgeo");
    request.since = "2024-13-45".to_string();
    ui.send(Command::Grab(request));
    ui.wait_log(LogLevel::Error, "invalid date '2024-13-45'");
    ui.finish();
}

#[tokio::test]
async fn login_required_invalidates_the_account_and_skips_the_rest() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path(format!("/api/v1/media/{}/info/", media_id(SHORTCODE))))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "message": "login_required", "require_login": true, "status": "fail"
        })))
        .mount(&env.server)
        .await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    ui.send(Command::Grab(
        env.request(&format!("{}\n@second\n@third", post_url(SHORTCODE))),
    ));
    ui.wait_for("idle LinkGrabber", |u| {
        matches!(u, Update::Grabbing { pending_links: 0 })
    });
    ui.wait_log(LogLevel::Error, "authentication failed");
    ui.wait_log(LogLevel::Warn, "2 remaining link(s) skipped");
    ui.wait_for(
        "Invalid account",
        |u| matches!(u, Update::Account(AccountStatus::Invalid(m)) if m.contains("authentication")),
    );
    assert!(
        ui.recent()
            .iter()
            .all(|u| !matches!(u, Update::Staged(s) if !s.is_empty()))
    );
    assert_eq!(env.requests_to("/api/v1/users/web_profile_info").await, 0);
    // The API did receive the cookies.
    let requests = env.server.received_requests().await.unwrap();
    let cookie = requests[0].headers.get("cookie").unwrap().to_str().unwrap();
    assert!(cookie.contains(SESSION));
    ui.finish();
}

// ---------------------------------------------------------------------------
// Downloads

#[tokio::test]
async fn accept_and_start_downloads_then_clear_finished() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);
    let package = ui.grab_one(env.request(&post_url(SHORTCODE)));

    ui.send(Command::Accept {
        staged: vec![package.id],
        start: true,
    });
    ui.wait_staged(
        "the package leaves the LinkGrabber",
        <[StagedPackage]>::is_empty,
    );
    let queued = ui.wait_queue("the new package", |q| q.packages.len() == 1);
    assert_eq!(queued.packages[0].items.len(), 2);
    ui.next("Downloading(true)", |u| {
        matches!(u, Update::Downloading(true))
    });
    ui.next("Downloading(false)", |u| {
        matches!(u, Update::Downloading(false))
    });
    // The final snapshot is sent before `Downloading(false)`.
    let done = ui.latest_queue();
    assert!(
        all_items_are(&done, ItemStatus::Done),
        "{}",
        describe(&Update::Queue(done.clone()))
    );
    ui.wait_log(LogLevel::Info, "2 downloaded");

    let item_ids: Vec<u64> = done.items().map(|item| item.id).collect();
    let progressed: Vec<u64> = ui
        .history
        .iter()
        .filter_map(|u| match u {
            Update::Progress { item_id, .. } => Some(*item_id),
            _ => None,
        })
        .collect();
    assert!(
        item_ids.iter().all(|id| progressed.contains(id)),
        "{progressed:?}"
    );
    let [image, video] = carousel_paths(&env);
    assert_eq!(std::fs::read(&image).unwrap(), b"JPEG-DATA-1");
    assert_eq!(std::fs::read(&video).unwrap(), b"MP4-DATA-2");
    let archive = std::fs::read_to_string(env.data().join("archive.txt")).unwrap();
    assert!(archive.contains("33000000000000000011"));
    let saved = QueueStore::read_unlocked(&env.data()).unwrap();
    assert!(all_items_are(&saved, ItemStatus::Done));

    // Grabbing it again finds nothing new.
    ui.send(Command::Grab(env.request(&post_url(SHORTCODE))));
    ui.wait_log(LogLevel::Info, "0 file(s) (2 already downloaded)");

    ui.send(Command::ClearFinished);
    ui.wait_queue("an empty list", |q| q.packages.is_empty());
    ui.wait_log(LogLevel::Info, "2 finished item(s) removed");
    ui.finish();
}

#[tokio::test]
async fn accept_without_start_then_start() {
    let env = Env::new().await;
    mount_carousel(&env).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);
    ui.grab_one(env.request(&post_url(SHORTCODE)));

    // Empty = every staged package.
    ui.send(Command::Accept {
        staged: vec![],
        start: false,
    });
    ui.wait_queue("queued items", |q| all_items_are(q, ItemStatus::Queued));
    ui.wait_log(LogLevel::Info, "added");
    ui.settle(Duration::from_millis(300));
    assert!(
        ui.recent()
            .iter()
            .all(|u| !matches!(u, Update::Downloading(true))),
        "accept without start must not download"
    );
    assert_eq!(env.requests_to("/cdn/").await, 0);

    ui.send(Command::Start);
    ui.next("Downloading(true)", |u| {
        matches!(u, Update::Downloading(true))
    });
    ui.next("Downloading(false)", |u| {
        matches!(u, Update::Downloading(false))
    });
    assert!(all_items_are(&ui.latest_queue(), ItemStatus::Done));
    let [image, video] = carousel_paths(&env);
    assert!(image.exists() && video.exists());

    ui.send(Command::Start);
    ui.wait_log(LogLevel::Info, "nothing to download");
    ui.finish();
}

#[tokio::test]
async fn stop_requeues_running_items_and_keeps_the_partial_file() {
    let env = Env::new().await;
    let code = "Dddddddddd1";
    mount_stalling_post(&env, code).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);
    ui.grab_one(env.request(&post_url(code)));

    ui.send(Command::Accept {
        staged: vec![],
        start: true,
    });
    let queue = ui.wait_queue("the new package", |q| q.packages.len() == 1);
    let package_id = queue.packages[0].id;
    let item_id = queue.packages[0].items[0].id;
    ui.wait_for(
        "bytes on disk",
        |u| matches!(u, Update::Progress { done, .. } if *done > 0),
    );

    // Running items cannot be removed.
    for id in [item_id, package_id] {
        ui.send(Command::Remove(vec![id]));
        ui.wait_log(
            LogLevel::Warn,
            "stop the downloads before removing running items",
        );
    }

    ui.send(Command::Stop);
    ui.next("Downloading(false)", |u| {
        matches!(u, Update::Downloading(false))
    });
    assert!(all_items_are(&ui.latest_queue(), ItemStatus::Queued));
    ui.wait_log(LogLevel::Info, "0 downloaded, 1 stopped");
    let dest = env
        .out()
        .join(format!("testuser/posts/2024-05-01_{code}_01.jpg"));
    let part = igdl::download::part_path(&dest);
    let kept = std::fs::metadata(&part).unwrap().len();
    assert!(kept > 0 && kept <= 64 * 1024, "{kept}");
    assert!(!dest.exists());

    ui.send(Command::Remove(vec![package_id]));
    ui.wait_queue("an empty list", |q| q.packages.is_empty());
    ui.wait_log(LogLevel::Info, "1 item(s) removed");
    ui.finish();
}

#[tokio::test]
async fn failed_items_can_be_retried() {
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
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);
    ui.grab_one(env.request(&post_url(code)));

    ui.send(Command::Accept {
        staged: vec![],
        start: true,
    });
    ui.wait_log(LogLevel::Error, "HTTP 404");
    ui.next("Downloading(false)", |u| {
        matches!(u, Update::Downloading(false))
    });
    assert!(all_items_are(&ui.latest_queue(), ItemStatus::Failed));
    ui.wait_log(LogLevel::Info, "0 downloaded, 1 failed");

    env.server.reset().await;
    env.cdn_file(&format!("{code}.jpg"), b"NOW-OK").await;
    ui.send(Command::Retry(vec![]));
    ui.wait_queue("the item re-queued", |q| {
        all_items_are(q, ItemStatus::Queued)
    });
    ui.wait_log(LogLevel::Info, "1 failed item(s) re-queued");

    ui.send(Command::Start);
    ui.next("Downloading(false)", |u| {
        matches!(u, Update::Downloading(false))
    });
    assert!(all_items_are(&ui.latest_queue(), ItemStatus::Done));
    let file = env
        .out()
        .join(format!("testuser/posts/2024-05-01_{code}_01.jpg"));
    assert_eq!(std::fs::read(file).unwrap(), b"NOW-OK");
    ui.finish();
}

// ---------------------------------------------------------------------------
// Account and settings

#[tokio::test]
async fn check_account_reports_the_logged_in_user() {
    let env = Env::new().await;
    Mock::given(method("GET"))
        .and(path("/api/v1/users/999/info/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "user": {"pk": "999", "username": "me_myself", "is_private": true},
            "status": "ok"
        })))
        .mount(&env.server)
        .await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    ui.send(Command::CheckAccount);
    ui.wait_log(LogLevel::Info, "logged in as @me_myself");
    ui.next("Checking", |u| {
        matches!(u, Update::Account(AccountStatus::Checking))
    });
    ui.next("LoggedIn", |u| {
        matches!(u, Update::Account(AccountStatus::LoggedIn { username }) if username == "me_myself")
    });
    ui.finish();
}

#[tokio::test]
async fn apply_settings_saves_valid_settings_and_rejects_invalid_ones() {
    let env = Env::new().await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);

    let settings = Settings {
        jobs: Some(3),
        retries: Some(2),
        api_delay: Some(0.5),
        limit_rate: Some("2M".to_string()),
        template: Some("{owner}/{shortcode}_{index}.{ext}".to_string()),
        include: Some(vec!["posts".to_string(), "stories".to_string()]),
        out_dir: Some(env.out()),
        ..Settings::default()
    };
    ui.send(Command::ApplySettings(Box::new(settings.clone())));
    let update = ui.wait_for("new settings", |u| matches!(u, Update::Settings { .. }));
    let Update::Settings {
        settings: applied,
        default_out_dir,
        ..
    } = update
    else {
        unreachable!()
    };
    assert_eq!(*applied, settings);
    assert_eq!(default_out_dir, env.out());
    assert_eq!(config::load(Some(&env.config_path())).unwrap(), settings);
    ui.wait_log(LogLevel::Info, "settings saved to");
    ui.wait_for("account status", |u| {
        matches!(u, Update::Account(AccountStatus::Loaded))
    });

    let invalid = Settings {
        jobs: Some(0),
        ..settings.clone()
    };
    ui.send(Command::ApplySettings(Box::new(invalid)));
    ui.wait_log(LogLevel::Error, "jobs must be between 1 and 32");
    let update = ui.wait_for("the old settings again", |u| {
        matches!(u, Update::Settings { .. })
    });
    let Update::Settings { settings: kept, .. } = update else {
        unreachable!()
    };
    assert_eq!(*kept, settings);
    assert_eq!(config::load(Some(&env.config_path())).unwrap(), settings);
    ui.finish();
}

#[tokio::test]
async fn settings_without_a_config_path_apply_for_the_session() {
    let env = Env::new().await;
    let mut config = env.config(Cookies::None);
    config.config_path = None;
    let mut ui = Ui::start(config);
    startup(&mut ui);
    let settings = Settings {
        jobs: Some(2),
        ..Settings::default()
    };
    ui.send(Command::ApplySettings(Box::new(settings.clone())));
    ui.wait_log(LogLevel::Warn, "settings are used for this session only");
    ui.wait_for(
        "new settings",
        |u| matches!(u, Update::Settings { settings: s, config_path: None, .. } if **s == settings),
    );
    assert!(!env.config_path().exists());
    ui.finish();
}

// ---------------------------------------------------------------------------
// Shutdown

#[tokio::test]
async fn shutdown_stops_downloads_saves_and_releases_the_lock() {
    let env = Env::new().await;
    let code = "Eeeeeeeeee1";
    mount_stalling_post(&env, code).await;
    mount_carousel(&env).await;
    let mut ui = env.start(Cookies::Valid);
    startup(&mut ui);
    assert!(QueueStore::open(&env.data()).is_err(), "the lock is held");

    ui.grab_one(env.request(&post_url(code)));
    ui.send(Command::Accept {
        staged: vec![],
        start: true,
    });
    ui.wait_for(
        "bytes on disk",
        |u| matches!(u, Update::Progress { done, .. } if *done > 0),
    );
    // A staged package that is never accepted is not saved.
    ui.grab_one(env.request(&post_url(SHORTCODE)));

    let started = Instant::now();
    ui.shutdown();
    assert!(started.elapsed() < Duration::from_secs(10));
    let store = QueueStore::open(&env.data()).expect("the lock is released");
    let queue = store.load().unwrap();
    assert_eq!(queue.packages.len(), 1);
    assert!(all_items_are(&queue, ItemStatus::Queued), "{queue:?}");
    ui.finish();
}
