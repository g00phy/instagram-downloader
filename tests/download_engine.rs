//! Download engine against a mocked CDN (wiremock, or a raw TCP server where the
//! test needs to cut a transfer short or stall it).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;
use igdl::download::{DownloadConfig, Downloader, Event, Job, Outcome, UrlRefresher, part_path};
use igdl::error::{Error, ErrorKind, Result};
use igdl::model::{Category, ItemError, MediaFile, MediaKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const TAKEN_AT: i64 = 1_600_000_000;
const TEST_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------- helpers

fn config() -> DownloadConfig {
    DownloadConfig {
        jobs: 2,
        retries: 3,
        retry_base: Duration::from_millis(10),
        timeout: Duration::from_secs(10),
        user_agent: "igdl-test/1.0".to_string(),
        limit_rate: None,
        set_mtime: true,
    }
}

fn media(key: &str, url: &str) -> MediaFile {
    MediaFile {
        key: key.to_string(),
        post_id: format!("post{key}"),
        shortcode: Some("C0ffeeXYZ12".to_string()),
        index: 1,
        count: 1,
        kind: MediaKind::Image,
        category: Category::Post,
        url: url.to_string(),
        width: None,
        height: None,
        owner: "tester".to_string(),
        owner_id: None,
        taken_at: TAKEN_AT,
        caption: None,
        highlight: None,
        ext: "jpg".to_string(),
    }
}

fn job(item_id: u64, url: &str, dest: &Path) -> Job {
    Job {
        item_id,
        media: media(&item_id.to_string(), url),
        dest: dest.to_path_buf(),
    }
}

/// Deterministic, non-repeating-looking test payload.
fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 % 251) as u8).collect()
}

async fn run_with(
    config: DownloadConfig,
    jobs: Vec<Job>,
    refresher: Option<Arc<dyn UrlRefresher>>,
) -> Vec<Event> {
    let downloader = Downloader::new(config).unwrap();
    let (tx, mut rx) = mpsc::unbounded_channel();
    tokio::time::timeout(
        TEST_TIMEOUT,
        downloader.run(jobs, refresher, tx, CancellationToken::new()),
    )
    .await
    .expect("run finished in time");
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

async fn run(jobs: Vec<Job>) -> Vec<Event> {
    run_with(config(), jobs, None).await
}

/// The single `Finished` outcome of `id`.
fn outcome(events: &[Event], id: u64) -> Outcome {
    let finished: Vec<&Outcome> = events
        .iter()
        .filter_map(|event| match event {
            Event::Finished { item_id, outcome } if *item_id == id => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished.len(),
        1,
        "exactly one Finished for {id}: {events:#?}"
    );
    finished[0].clone()
}

fn done(events: &[Event], id: u64) -> (PathBuf, u64) {
    match outcome(events, id) {
        Outcome::Done { path, bytes } => (path, bytes),
        other => panic!("item {id} did not finish: {other:?}\n{events:#?}"),
    }
}

fn failure(events: &[Event], id: u64) -> ItemError {
    match outcome(events, id) {
        Outcome::Failed { error } => error,
        other => panic!("item {id} did not fail: {other:?}"),
    }
}

/// `(total, resumed_from)` of every `Started` of `id`.
fn starts(events: &[Event], id: u64) -> Vec<(Option<u64>, u64)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Started {
                item_id,
                total,
                resumed_from,
                ..
            } if *item_id == id => Some((*total, *resumed_from)),
            _ => None,
        })
        .collect()
}

fn retry_attempts(events: &[Event], id: u64) -> Vec<u32> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Retrying {
                item_id, attempt, ..
            } if *item_id == id => Some(*attempt),
            _ => None,
        })
        .collect()
}

async fn requests(server: &MockServer) -> Vec<Request> {
    server.received_requests().await.unwrap_or_default()
}

fn range_header(request: &Request) -> Option<String> {
    request
        .headers
        .get("range")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// Serves `body` like a CDN that honours `Range: bytes=N-`.
struct Ranged(Vec<u8>);

impl Respond for Ranged {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let len = self.0.len();
        let start = range_header(request).and_then(|range| {
            range
                .strip_prefix("bytes=")?
                .strip_suffix('-')?
                .parse::<usize>()
                .ok()
        });
        match start {
            None => ResponseTemplate::new(200)
                .insert_header("content-type", "image/jpeg")
                .set_body_bytes(self.0.clone()),
            Some(start) if start >= len => {
                ResponseTemplate::new(416).insert_header("content-range", format!("bytes */{len}"))
            }
            Some(start) => ResponseTemplate::new(206)
                .insert_header("content-type", "image/jpeg")
                .insert_header("content-range", format!("bytes {start}-{}/{len}", len - 1))
                .set_body_bytes(self.0[start..].to_vec()),
        }
    }
}

async fn serve(server: &MockServer, route: &str, response: impl Respond + 'static) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(response)
        .mount(server)
        .await;
}

/// Hands out a fixed URL and counts calls.
struct FixedRefresher {
    result: std::result::Result<String, ErrorKind>,
    calls: AtomicUsize,
}

impl FixedRefresher {
    fn ok(url: String) -> Arc<Self> {
        Arc::new(FixedRefresher {
            result: Ok(url),
            calls: AtomicUsize::new(0),
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(FixedRefresher {
            result: Err(ErrorKind::Auth),
            calls: AtomicUsize::new(0),
        })
    }
}

impl UrlRefresher for FixedRefresher {
    fn refresh<'a>(&'a self, _media: &'a MediaFile) -> BoxFuture<'a, Result<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self
            .result
            .clone()
            .map_err(|_| Error::Auth("cookies expired".to_string()));
        Box::pin(async move { result })
    }
}

type Handler = Arc<dyn Fn(usize, String, TcpStream) -> BoxFuture<'static, ()> + Send + Sync>;

/// A bare HTTP/1.1 server: one request per connection, `handler(connection index,
/// request head, stream)` writes whatever it likes. Returns the URL and the heads.
async fn raw_server(handler: Handler) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let recorded = heads.clone();
    tokio::spawn(async move {
        let mut index = 0;
        while let Ok((mut stream, _)) = listener.accept().await {
            let handler = handler.clone();
            let recorded = recorded.clone();
            let n = index;
            index += 1;
            tokio::spawn(async move {
                let head = read_head(&mut stream).await;
                recorded.lock().unwrap().push(head.clone());
                handler(n, head, stream).await;
            });
        }
    });
    (format!("http://{addr}/media/file.jpg"), heads)
}

async fn read_head(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => head.extend_from_slice(&buf[..n]),
        }
    }
    String::from_utf8_lossy(&head).to_ascii_lowercase()
}

async fn write_response(stream: &mut TcpStream, status: &str, headers: &[String], body: &[u8]) {
    let mut head =
        format!("HTTP/1.1 {status}\r\nconnection: close\r\ncontent-type: image/jpeg\r\n");
    for header in headers {
        head.push_str(header);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

// ---------------------------------------------------------------- tests

#[tokio::test]
async fn simple_download_sets_bytes_and_mtime() {
    let server = MockServer::start().await;
    let body = payload(10_000);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("out").join("photo.jpg");

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    let (path, bytes) = done(&events, 1);
    assert_eq!(path, dest);
    assert_eq!(bytes, 10_000);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!part_path(&dest).exists());
    let mtime = std::fs::metadata(&dest).unwrap().modified().unwrap();
    assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(TAKEN_AT as u64));

    assert_eq!(starts(&events, 1), vec![(Some(10_000), 0)]);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Started { name, .. } if name == "photo.jpg"
    )));
    let last_progress = events.iter().rev().find_map(|event| match event {
        Event::Progress { done, total, .. } => Some((*done, *total)),
        _ => None,
    });
    assert_eq!(last_progress, Some((10_000, Some(10_000))));
    assert!(matches!(events.last(), Some(Event::Finished { .. })));

    let received = requests(&server).await;
    assert_eq!(received.len(), 1);
    assert_eq!(range_header(&received[0]), None);
    assert_eq!(
        received[0].headers.get("user-agent").unwrap(),
        "igdl-test/1.0"
    );
    assert!(received[0].headers.get("cookie").is_none());
}

#[tokio::test]
async fn resumes_existing_part_with_range() {
    let server = MockServer::start().await;
    let body = payload(5_000);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(part_path(&dest), &body[..1_000]).unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(done(&events, 1), (dest.clone(), 5_000));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(starts(&events, 1), vec![(Some(5_000), 1_000)]);
    let received = requests(&server).await;
    assert_eq!(received.len(), 1);
    assert_eq!(range_header(&received[0]).as_deref(), Some("bytes=1000-"));
    assert!(!part_path(&dest).exists());
}

#[tokio::test]
async fn server_ignoring_range_restarts_from_zero() {
    let server = MockServer::start().await;
    let body = payload(3_000);
    serve(
        &server,
        "/a.jpg",
        ResponseTemplate::new(200)
            .insert_header("content-type", "image/jpeg")
            .set_body_bytes(body.clone()),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(part_path(&dest), b"WRONG").unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(done(&events, 1), (dest.clone(), 3_000));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(starts(&events, 1), vec![(Some(3_000), 0)]);
    let received = requests(&server).await;
    assert_eq!(range_header(&received[0]).as_deref(), Some("bytes=5-"));
}

#[tokio::test]
async fn range_not_satisfiable_with_complete_part_finishes() {
    let server = MockServer::start().await;
    let body = payload(2_000);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(part_path(&dest), &body).unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(done(&events, 1), (dest.clone(), 2_000));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn range_not_satisfiable_with_oversized_part_restarts() {
    let server = MockServer::start().await;
    let body = payload(2_000);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(part_path(&dest), payload(2_500)).unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(done(&events, 1), (dest.clone(), 2_000));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let received = requests(&server).await;
    assert_eq!(received.len(), 2);
    assert_eq!(range_header(&received[0]).as_deref(), Some("bytes=2500-"));
    assert_eq!(range_header(&received[1]), None);
    assert!(retry_attempts(&events, 1).is_empty());
}

#[tokio::test]
async fn truncated_body_is_retried_and_resumed() {
    let body = Arc::new(payload(100));
    let served = body.clone();
    let handler: Handler = Arc::new(move |n, head, mut stream| {
        let body = served.clone();
        Box::pin(async move {
            if n == 0 {
                // Promise 100 bytes, send 40, hang up.
                write_response(
                    &mut stream,
                    "200 OK",
                    &["content-length: 100".to_string()],
                    &body[..40],
                )
                .await;
            } else if head.contains("range: bytes=40-") {
                write_response(
                    &mut stream,
                    "206 Partial Content",
                    &[
                        "content-length: 60".to_string(),
                        "content-range: bytes 40-99/100".to_string(),
                    ],
                    &body[40..],
                )
                .await;
            } else {
                write_response(&mut stream, "400 Bad Request", &[], b"").await;
            }
        })
    });
    let (url, heads) = raw_server(handler).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("clip.mp4");

    let events = run(vec![job(1, &url, &dest)]).await;

    assert_eq!(done(&events, 1), (dest.clone(), 100));
    assert_eq!(std::fs::read(&dest).unwrap(), *body);
    assert_eq!(retry_attempts(&events, 1), vec![1]);
    assert_eq!(starts(&events, 1), vec![(Some(100), 0), (Some(100), 40)]);
    let heads = heads.lock().unwrap().clone();
    assert_eq!(heads.len(), 2);
    assert!(!heads[0].contains("range:"));
    assert!(heads[1].contains("range: bytes=40-"));
}

#[tokio::test]
async fn server_errors_are_retried_with_backoff() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/a.jpg"))
        .respond_with(ResponseTemplate::new(500))
        .up_to_n_times(2)
        .mount(&server)
        .await;
    serve(&server, "/a.jpg", Ranged(payload(500))).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(done(&events, 1).1, 500);
    assert_eq!(retry_attempts(&events, 1), vec![1, 2]);
    for event in &events {
        if let Event::Retrying {
            attempt,
            delay,
            reason,
            ..
        } = event
        {
            let nominal = Duration::from_millis(10) * 2u32.pow(attempt - 1);
            assert!(
                *delay >= nominal && *delay <= nominal.mul_f64(1.25),
                "{delay:?}"
            );
            assert!(reason.contains("500"), "{reason}");
        }
    }
    assert_eq!(requests(&server).await.len(), 3);
}

#[tokio::test]
async fn retries_exhausted_fail_with_last_kind() {
    let server = MockServer::start().await;
    serve(&server, "/a.jpg", ResponseTemplate::new(503)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let config = DownloadConfig {
        retries: 2,
        ..config()
    };

    let events = run_with(
        config,
        vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)],
        None,
    )
    .await;

    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Http);
    assert!(error.message.contains("503"), "{}", error.message);
    assert_eq!(retry_attempts(&events, 1), vec![1, 2]);
    assert_eq!(requests(&server).await.len(), 3);
    assert!(!dest.exists());
}

#[tokio::test]
async fn connection_refused_is_a_network_failure() {
    // Bind and drop a listener to get a port nobody listens on.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let url = format!("http://127.0.0.1:{port}/a.jpg?oe=SECRET");
    let config = DownloadConfig {
        retries: 1,
        ..config()
    };

    let events = run_with(config, vec![job(1, &url, &dest)], None).await;

    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Network);
    assert!(!error.message.contains("SECRET"), "{}", error.message);
    assert!(!error.message.contains("127.0.0.1"), "{}", error.message);
    assert_eq!(retry_attempts(&events, 1), vec![1]);
}

#[tokio::test]
async fn not_found_is_not_retried() {
    let server = MockServer::start().await;
    serve(&server, "/a.jpg", ResponseTemplate::new(404)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(failure(&events, 1).kind, ErrorKind::NotFound);
    assert!(retry_attempts(&events, 1).is_empty());
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn expired_url_is_refreshed_once() {
    let server = MockServer::start().await;
    serve(&server, "/old.jpg", ResponseTemplate::new(403)).await;
    let body = payload(800);
    serve(&server, "/new.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let new_url = format!("{}/new.jpg", server.uri());
    let refresher = FixedRefresher::ok(new_url.clone());

    let events = run_with(
        config(),
        vec![job(1, &format!("{}/old.jpg", server.uri()), &dest)],
        Some(refresher.clone()),
    )
    .await;

    assert_eq!(done(&events, 1), (dest.clone(), 800));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
    assert!(events.contains(&Event::Refreshed {
        item_id: 1,
        url: new_url
    }));
    assert!(
        retry_attempts(&events, 1).is_empty(),
        "a refresh is not a retry"
    );
    let paths: Vec<String> = requests(&server)
        .await
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(paths, vec!["/old.jpg", "/new.jpg"]);
}

#[tokio::test]
async fn refreshed_url_that_still_fails_gives_up() {
    let server = MockServer::start().await;
    serve(&server, "/old.jpg", ResponseTemplate::new(410)).await;
    serve(&server, "/new.jpg", ResponseTemplate::new(403)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let refresher = FixedRefresher::ok(format!("{}/new.jpg", server.uri()));

    let events = run_with(
        config(),
        vec![job(1, &format!("{}/old.jpg", server.uri()), &dest)],
        Some(refresher.clone()),
    )
    .await;

    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Http);
    assert!(error.message.contains("403"), "{}", error.message);
    assert_eq!(refresher.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn expired_url_without_refresher_fails() {
    let server = MockServer::start().await;
    serve(&server, "/a.jpg", ResponseTemplate::new(403)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");

    let events = run(vec![job(
        1,
        &format!("{}/a.jpg?oe=SECRET", server.uri()),
        &dest,
    )])
    .await;

    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Http);
    assert!(error.message.contains("expired"), "{}", error.message);
    assert!(!error.message.contains("SECRET"), "{}", error.message);
    assert!(retry_attempts(&events, 1).is_empty());
}

#[tokio::test]
async fn refresher_error_fails_with_its_kind() {
    let server = MockServer::start().await;
    serve(&server, "/a.jpg", ResponseTemplate::new(403)).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");

    let events = run_with(
        config(),
        vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)],
        Some(FixedRefresher::failing()),
    )
    .await;

    assert_eq!(failure(&events, 1).kind, ErrorKind::Auth);
}

#[tokio::test]
async fn existing_destination_is_skipped_without_request() {
    let server = MockServer::start().await;
    serve(&server, "/a.jpg", Ranged(payload(10))).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(&dest, b"mine").unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    assert_eq!(
        outcome(&events, 1),
        Outcome::Skipped {
            path: dest.clone(),
            reason: "file already exists".to_string()
        }
    );
    assert_eq!(events.len(), 1, "{events:#?}");
    assert!(requests(&server).await.is_empty());
    assert_eq!(std::fs::read(&dest).unwrap(), b"mine");
}

#[tokio::test]
async fn html_response_is_a_parse_failure_and_keeps_part() {
    let server = MockServer::start().await;
    serve(
        &server,
        "/a.jpg",
        ResponseTemplate::new(200).set_body_raw("<html>login</html>", "text/html; charset=utf-8"),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    std::fs::write(part_path(&dest), b"12345").unwrap();

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;

    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Parse);
    assert!(error.message.contains("HTML"), "{}", error.message);
    assert_eq!(std::fs::read(part_path(&dest)).unwrap(), b"12345");
    assert!(!dest.exists());
}

#[tokio::test]
async fn destination_appearing_during_transfer_is_not_overwritten() {
    let server = MockServer::start().await;
    let body = payload(300);
    serve(
        &server,
        "/a.jpg",
        ResponseTemplate::new(200)
            .insert_header("content-type", "image/jpeg")
            .set_body_bytes(body.clone())
            .set_delay(Duration::from_millis(600)),
    )
    .await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let writer = {
        let dest = dest.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            std::fs::write(&dest, b"someone else").unwrap();
        })
    };

    let events = run(vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)]).await;
    writer.await.unwrap();

    let (path, _) = done(&events, 1);
    assert_eq!(path, dir.path().join("photo (1).jpg"));
    assert_eq!(std::fs::read(&path).unwrap(), body);
    assert_eq!(std::fs::read(&dest).unwrap(), b"someone else");
}

#[tokio::test]
async fn cancel_mid_transfer_keeps_part_and_skips_unstarted_jobs() {
    let body = Arc::new(payload(64 * 1024));
    let served = body.clone();
    let handler: Handler = Arc::new(move |_, _, mut stream| {
        let body = served.clone();
        Box::pin(async move {
            write_response(
                &mut stream,
                "200 OK",
                &["content-length: 1000000".to_string()],
                &body,
            )
            .await;
            // Stall with the connection open.
            tokio::time::sleep(Duration::from_secs(60)).await;
        })
    });
    let (url, _) = raw_server(handler).await;
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("first.mp4");
    let second = dir.path().join("second.mp4");
    let config = DownloadConfig {
        jobs: 1,
        ..config()
    };
    let downloader = Arc::new(Downloader::new(config).unwrap());
    let (tx, mut rx) = mpsc::unbounded_channel();
    let cancel = CancellationToken::new();
    let jobs = vec![job(1, &url, &first), job(2, &url, &second)];
    let task = tokio::spawn({
        let downloader = downloader.clone();
        let cancel = cancel.clone();
        async move { downloader.run(jobs, None, tx, cancel).await }
    });

    let started = Instant::now();
    let mut events = Vec::new();
    let collect = async {
        while let Some(event) = rx.recv().await {
            if matches!(event, Event::Progress { done, .. } if done > 0) {
                cancel.cancel();
            }
            events.push(event);
        }
    };
    tokio::time::timeout(TEST_TIMEOUT, collect)
        .await
        .expect("run ended");
    task.await.unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(30),
        "stopped promptly"
    );
    let error = failure(&events, 1);
    assert_eq!(error.kind, ErrorKind::Cancelled);
    let kept = std::fs::metadata(part_path(&first)).unwrap().len();
    assert!(kept > 0 && kept <= body.len() as u64, "{kept}");
    assert_eq!(
        std::fs::read(part_path(&first)).unwrap(),
        body[..kept as usize]
    );
    assert!(!first.exists());
    assert!(
        events.iter().all(|event| !matches!(
            event,
            Event::Started { item_id: 2, .. } | Event::Finished { item_id: 2, .. }
        )),
        "the second job never started: {events:#?}"
    );
    assert!(!part_path(&second).exists());
}

#[tokio::test]
async fn concurrency_limit_is_honoured() {
    let in_flight = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let handler: Handler = {
        let in_flight = in_flight.clone();
        let peak = peak.clone();
        Arc::new(move |_, _, mut stream| {
            let in_flight = in_flight.clone();
            let peak = peak.clone();
            Box::pin(async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(400)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                write_response(
                    &mut stream,
                    "200 OK",
                    &["content-length: 4".to_string()],
                    b"data",
                )
                .await;
            })
        })
    };
    let (url, heads) = raw_server(handler).await;
    let dir = tempfile::tempdir().unwrap();
    let jobs = (1..=4)
        .map(|id| job(id, &url, &dir.path().join(format!("{id}.jpg"))))
        .collect();

    let start = Instant::now();
    let events = run(jobs).await;
    let elapsed = start.elapsed();

    for id in 1..=4 {
        assert_eq!(done(&events, id).1, 4);
    }
    assert_eq!(heads.lock().unwrap().len(), 4);
    assert_eq!(
        peak.load(Ordering::SeqCst),
        2,
        "two transfers in parallel, never more"
    );
    assert!(elapsed >= Duration::from_millis(750), "{elapsed:?}");
}

#[tokio::test]
async fn limit_rate_slows_transfers() {
    let server = MockServer::start().await;
    let body = payload(150_000);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("photo.jpg");
    let config = DownloadConfig {
        limit_rate: Some(100_000),
        ..config()
    };

    let start = Instant::now();
    let events = run_with(
        config,
        vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)],
        None,
    )
    .await;
    let elapsed = start.elapsed();

    assert_eq!(done(&events, 1).1, 150_000);
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    // 150 kB at 100 kB/s takes about 1.5 s.
    assert!(elapsed >= Duration::from_millis(1_000), "{elapsed:?}");
}

#[tokio::test]
async fn paths_with_spaces_and_unicode() {
    let server = MockServer::start().await;
    let body = payload(1_234);
    serve(&server, "/a.jpg", Ranged(body.clone())).await;
    let dir = tempfile::tempdir().unwrap();
    let dest = dir
        .path()
        .join("my folder ü")
        .join("日本 写真")
        .join("2024-05-01 café #1.jpg");
    let config = DownloadConfig {
        set_mtime: false,
        ..config()
    };

    let events = run_with(
        config,
        vec![job(1, &format!("{}/a.jpg", server.uri()), &dest)],
        None,
    )
    .await;

    assert_eq!(done(&events, 1), (dest.clone(), 1_234));
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Started { name, .. } if name == "2024-05-01 café #1.jpg"
    )));
    // set_mtime = false leaves the current time.
    let mtime = std::fs::metadata(&dest).unwrap().modified().unwrap();
    let age = SystemTime::now()
        .duration_since(mtime)
        .unwrap_or(Duration::ZERO);
    assert!(age < Duration::from_secs(3600), "{age:?}");
}

#[tokio::test]
async fn invalid_user_agent_is_a_config_error() {
    let config = DownloadConfig {
        user_agent: "bad\nagent".to_string(),
        ..config()
    };
    let err = Downloader::new(config).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Config);
}
