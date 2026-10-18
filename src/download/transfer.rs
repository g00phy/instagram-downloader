//! One job: skip check, `.part` resume, retries, URL refresh and the final rename.
//!
//! Error messages never contain the media URL (signed CDN URLs are sensitive);
//! failures are described by status code and cause only.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{CONTENT_RANGE, CONTENT_TYPE, HeaderMap, RANGE};
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc::UnboundedSender;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::files::{existing_len, free_path, part_path, remove_if_exists};
use super::{Event, Inner, Job, Outcome, UrlRefresher};
use crate::error::Error;
use crate::model::ItemError;

/// Minimum time between two `Progress` events of one file.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// Longest backoff between two attempts, whatever `retry_base` and the attempt are.
const MAX_BACKOFF: Duration = Duration::from_secs(600);

/// Jitter added on top of the exponential backoff (fraction of the delay).
const MAX_JITTER: f64 = 0.25;

/// Message for a CDN 403/410 that could not be fixed by refreshing the URL.
const EXPIRED_MESSAGE: &str = "the media URL expired or access was denied";

/// Why one attempt ended without a complete file.
#[derive(Debug)]
enum Failure {
    /// `cancel` fired; the `.part` file was flushed and is kept.
    Cancelled,
    /// 403/410: the signed URL expired (or access was denied).
    Expired(u16),
    /// Worth another attempt after a backoff.
    Retryable(Error),
    /// Give up on this job.
    Fatal(Error),
}

/// What to do with the response body of a successful request.
#[derive(Debug, Clone, Copy)]
enum Plan {
    /// Truncate the `.part` and write from offset 0.
    Fresh { total: Option<u64> },
    /// Append to the existing `.part` of `offset` bytes.
    Append { offset: u64, total: Option<u64> },
}

/// Runs one job to completion and emits its `Finished` event.
pub(super) async fn run_job(
    inner: &Inner,
    job: Job,
    refresher: Option<&dyn UrlRefresher>,
    events: &UnboundedSender<Event>,
    cancel: &CancellationToken,
) {
    let item_id = job.item_id;
    let outcome = download(inner, &job, refresher, events, cancel).await;
    let _ = events.send(Event::Finished { item_id, outcome });
}

async fn download(
    inner: &Inner,
    job: &Job,
    refresher: Option<&dyn UrlRefresher>,
    events: &UnboundedSender<Event>,
    cancel: &CancellationToken,
) -> Outcome {
    let dest = &job.dest;
    match tokio::fs::try_exists(dest).await {
        Ok(true) => {
            return Outcome::Skipped {
                path: dest.clone(),
                reason: "file already exists".to_string(),
            };
        }
        Ok(false) => {}
        Err(err) => return failed(Error::io(format!("cannot check {}", dest.display()), err)),
    }
    let Some(name) = dest.file_name() else {
        return failed(Error::Config(format!(
            "the destination {} has no file name",
            dest.display()
        )));
    };
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty())
        && let Err(err) = tokio::fs::create_dir_all(parent).await
    {
        return failed(Error::io(
            format!("cannot create {}", parent.display()),
            err,
        ));
    }

    let transfer = Transfer {
        inner,
        item_id: job.item_id,
        name: name.to_string_lossy().into_owned(),
        part: part_path(dest),
        events,
        cancel,
    };
    let mut url = job.media.url.clone();
    let mut refreshed = false;
    let mut retries = 0u32;
    let bytes = loop {
        if cancel.is_cancelled() {
            return cancelled();
        }
        match transfer.fetch(&url).await {
            Ok(bytes) => break bytes,
            Err(Failure::Cancelled) => return cancelled(),
            Err(Failure::Fatal(err)) => return failed(err),
            Err(Failure::Expired(status)) => match refresher {
                Some(refresher) if !refreshed => {
                    refreshed = true;
                    let fresh = tokio::select! {
                        biased;
                        () = cancel.cancelled() => return cancelled(),
                        fresh = refresher.refresh(&job.media) => fresh,
                    };
                    match fresh {
                        Ok(fresh) => {
                            url = fresh;
                            let _ = events.send(Event::Refreshed {
                                item_id: job.item_id,
                                url: url.clone(),
                            });
                        }
                        Err(err) => return failed(err),
                    }
                }
                _ => {
                    return failed(Error::Http {
                        status,
                        message: EXPIRED_MESSAGE.to_string(),
                    });
                }
            },
            Err(Failure::Retryable(err)) => {
                if retries >= inner.config.retries {
                    return failed(err);
                }
                retries += 1;
                let delay = backoff(inner.config.retry_base, retries);
                let _ = events.send(Event::Retrying {
                    item_id: job.item_id,
                    attempt: retries,
                    delay,
                    reason: err.to_string(),
                });
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return cancelled(),
                    () = tokio::time::sleep(delay) => {}
                }
            }
        }
    };

    match finish(&transfer.part, dest, bytes).await {
        Ok(path) => {
            if inner.config.set_mtime && job.media.taken_at > 0 {
                set_mtime(&path, job.media.taken_at);
            }
            Outcome::Done { path, bytes }
        }
        Err(err) => failed(err),
    }
}

/// One file's transfer context.
struct Transfer<'a> {
    inner: &'a Inner,
    item_id: u64,
    /// Destination file name, for `Started`.
    name: String,
    part: PathBuf,
    events: &'a UnboundedSender<Event>,
    cancel: &'a CancellationToken,
}

impl Transfer<'_> {
    /// One attempt: request (resuming the `.part` if any) and stream the body into
    /// it. Returns the complete size on success.
    async fn fetch(&self, url: &str) -> Result<u64, Failure> {
        // Set once the `.part` was discarded, so a confused server cannot loop us.
        let mut restarted = false;
        loop {
            let offset = existing_len(&self.part)
                .await
                .map_err(|err| self.part_error("cannot read", err))?;
            let mut request = self.inner.client.get(url);
            if offset > 0 {
                request = request.header(RANGE, format!("bytes={offset}-"));
            }
            let mut response = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return Err(Failure::Cancelled),
                response = request.send() => response.map_err(request_failure)?,
            };

            let status = response.status();
            let plan = match status {
                StatusCode::OK => Plan::Fresh {
                    total: response.content_length(),
                },
                StatusCode::PARTIAL_CONTENT => {
                    match parse_content_range(response.headers()) {
                        Some((start, total)) if start == offset => Plan::Append {
                            offset,
                            total: total
                                .or_else(|| response.content_length().map(|len| offset + len)),
                        },
                        Some((0, total)) => Plan::Fresh {
                            total: total.or(response.content_length()),
                        },
                        _ => {
                            // A range we did not ask for: start over without Range.
                            self.restart(&mut restarted, 206, "an unexpected byte range")
                                .await?;
                            continue;
                        }
                    }
                }
                StatusCode::RANGE_NOT_SATISFIABLE if offset > 0 => {
                    if unsatisfied_total(response.headers()) == Some(offset) {
                        // The `.part` already holds the whole file.
                        self.started(Some(offset), offset);
                        self.progress(offset, Some(offset));
                        return Ok(offset);
                    }
                    self.restart(
                        &mut restarted,
                        416,
                        "a size that does not match the partial file",
                    )
                    .await?;
                    continue;
                }
                _ => return Err(classify_status(status)),
            };

            if is_html(response.headers()) {
                return Err(Failure::Fatal(Error::Parse(
                    "server returned an HTML page instead of media".to_string(),
                )));
            }
            return self.write_body(&mut response, plan).await;
        }
    }

    /// Deletes the `.part` so the next request starts from byte 0.
    /// Fails if that already happened once in this attempt.
    async fn restart(&self, restarted: &mut bool, status: u16, cause: &str) -> Result<(), Failure> {
        if *restarted {
            return Err(Failure::Fatal(Error::Http {
                status,
                message: format!("the server keeps answering with {cause}"),
            }));
        }
        *restarted = true;
        tracing::debug!(item_id = self.item_id, cause, "discarding the partial file");
        remove_if_exists(&self.part)
            .await
            .map_err(|err| self.part_error("cannot delete", err))
    }

    /// Streams the body into the `.part` file.
    async fn write_body(
        &self,
        response: &mut reqwest::Response,
        plan: Plan,
    ) -> Result<u64, Failure> {
        let (opened, mut done, total) = match plan {
            Plan::Fresh { total } => {
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&self.part)
                    .await;
                (file, 0, total)
            }
            Plan::Append { offset, total } => {
                let file = OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&self.part)
                    .await;
                (file, offset, total)
            }
        };
        let mut file = opened.map_err(|err| self.part_error("cannot open", err))?;
        let file = &mut file;
        self.started(total, done);

        // When the last `Progress` went out and what it said.
        let mut last_progress: Option<(Instant, u64)> = None;
        loop {
            let chunk = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    close(file).await;
                    return Err(Failure::Cancelled);
                }
                chunk = response.chunk() => chunk,
            };
            let chunk = match chunk {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(err) => {
                    close(file).await;
                    return Err(Failure::Retryable(network_error(&err)));
                }
            };
            if let Err(err) = file.write_all(&chunk).await {
                close(file).await;
                return Err(self.part_error("cannot write", err));
            }
            done += chunk.len() as u64;
            if last_progress.is_none_or(|(at, _)| at.elapsed() >= PROGRESS_INTERVAL) {
                last_progress = Some((Instant::now(), done));
                self.progress(done, total);
            }
            if let Some(limiter) = &self.inner.limiter {
                tokio::select! {
                    biased;
                    () = self.cancel.cancelled() => {
                        close(file).await;
                        return Err(Failure::Cancelled);
                    }
                    () = limiter.acquire(chunk.len()) => {}
                }
            }
        }

        let synced = match file.flush().await {
            Ok(()) => file.sync_all().await,
            Err(err) => Err(err),
        };
        if let Err(err) = synced {
            return Err(self.part_error("cannot write", err));
        }
        if last_progress.is_none_or(|(_, reported)| reported != done) {
            self.progress(done, total);
        }
        match total {
            Some(total) if done < total => Err(Failure::Retryable(Error::Network(format!(
                "the transfer was cut off after {done} of {total} bytes"
            )))),
            _ => Ok(done),
        }
    }

    fn started(&self, total: Option<u64>, resumed_from: u64) {
        let _ = self.events.send(Event::Started {
            item_id: self.item_id,
            name: self.name.clone(),
            total,
            resumed_from,
        });
    }

    fn progress(&self, done: u64, total: Option<u64>) {
        let _ = self.events.send(Event::Progress {
            item_id: self.item_id,
            done,
            total,
        });
    }

    fn part_error(&self, what: &str, err: std::io::Error) -> Failure {
        Failure::Fatal(Error::io(format!("{what} {}", self.part.display()), err))
    }
}

/// Flushes what was received so far so the `.part` is resumable; errors are
/// irrelevant here because the next run measures the file anyway.
async fn close(file: &mut File) {
    let _ = file.flush().await;
}

/// Renames the finished `.part` to `dest` (or the first free `dest (N).ext` if a
/// file appeared there meanwhile) and returns the final path.
async fn finish(part: &Path, dest: &Path, bytes: u64) -> Result<PathBuf, Error> {
    let target = match tokio::fs::try_exists(dest).await {
        Ok(false) => dest.to_path_buf(),
        Ok(true) => match free_path(dest).await {
            Ok(Some(free)) => {
                tracing::warn!(
                    path = %dest.display(),
                    "a file appeared at the destination during the download; saving under a new name"
                );
                free
            }
            Ok(None) => {
                return Err(Error::Config(format!(
                    "no free file name next to {}",
                    dest.display()
                )));
            }
            Err(err) => return Err(Error::io(format!("cannot check {}", dest.display()), err)),
        },
        Err(err) => return Err(Error::io(format!("cannot check {}", dest.display()), err)),
    };
    tokio::fs::rename(part, &target).await.map_err(|err| {
        Error::io(
            format!("cannot move {} to {}", part.display(), target.display()),
            err,
        )
    })?;
    tracing::debug!(path = %target.display(), bytes, "download finished");
    Ok(target)
}

/// Sets the modification time to the post date; failure is only worth a warning.
fn set_mtime(path: &Path, taken_at: i64) {
    let time = filetime::FileTime::from_unix_time(taken_at, 0);
    if let Err(err) = filetime::set_file_mtime(path, time) {
        tracing::warn!(path = %path.display(), error = %err, "cannot set the file modification time");
    }
}

/// Maps a non-success status (other than 416 handled by the caller) to a failure.
fn classify_status(status: StatusCode) -> Failure {
    let code = status.as_u16();
    match code {
        403 | 410 => Failure::Expired(code),
        404 => Failure::Fatal(Error::NotFound(
            "the media file is no longer available (HTTP 404)".to_string(),
        )),
        429 => Failure::Retryable(Error::Http {
            status: code,
            message: "too many requests to the media server".to_string(),
        }),
        500..=599 => Failure::Retryable(Error::Http {
            status: code,
            message: status
                .canonical_reason()
                .unwrap_or("server error")
                .to_lowercase(),
        }),
        _ => Failure::Fatal(Error::Http {
            status: code,
            message: format!(
                "unexpected response from the media server ({})",
                status.canonical_reason().unwrap_or("unknown status")
            ),
        }),
    }
}

/// A request that produced no response: a bad URL is fatal, anything else
/// (connect error, timeout, reset) is worth retrying.
fn request_failure(err: reqwest::Error) -> Failure {
    if err.is_builder() {
        return Failure::Fatal(Error::Unsupported(
            "the media URL is not a valid http(s) URL".to_string(),
        ));
    }
    Failure::Retryable(network_error(&err))
}

/// Describes a transport error by its cause, never by its URL.
fn network_error(err: &reqwest::Error) -> Error {
    let mut message = if err.is_timeout() {
        "timed out".to_string()
    } else if err.is_connect() {
        "cannot connect to the media server".to_string()
    } else if err.is_body() || err.is_decode() {
        "the transfer was interrupted".to_string()
    } else {
        "the request failed".to_string()
    };
    // The innermost cause (an io::Error or hyper error) carries no URL.
    let mut root: Option<&dyn std::error::Error> = None;
    let mut source = std::error::Error::source(err);
    while let Some(cause) = source {
        root = Some(cause);
        source = cause.source();
    }
    if let Some(root) = root {
        message.push_str(": ");
        message.push_str(&root.to_string());
    }
    Error::Network(message)
}

/// `Content-Range: bytes START-END/TOTAL` → `(START, TOTAL)` (`TOTAL` may be `*`).
fn parse_content_range(headers: &HeaderMap) -> Option<(u64, Option<u64>)> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let spec = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = spec.split_once('/')?;
    let (start, _end) = range.trim().split_once('-')?;
    let start = start.trim().parse().ok()?;
    let total = total.trim().parse().ok();
    Some((start, total))
}

/// `Content-Range: bytes */TOTAL` of a 416 response → `TOTAL`.
fn unsatisfied_total(headers: &HeaderMap) -> Option<u64> {
    let value = headers.get(CONTENT_RANGE)?.to_str().ok()?;
    let spec = value.trim().strip_prefix("bytes")?.trim_start();
    spec.strip_prefix("*/")?.trim().parse().ok()
}

fn is_html(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("text/html"))
}

/// `base * 2^(attempt-1)` plus 0–25 % random jitter, capped at [`MAX_BACKOFF`].
fn backoff(base: Duration, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(30);
    let jitter = 1.0 + rand::random::<f64>() * MAX_JITTER;
    let secs = base.as_secs_f64() * 2f64.powi(exponent as i32) * jitter;
    Duration::try_from_secs_f64(secs)
        .unwrap_or(MAX_BACKOFF)
        .min(MAX_BACKOFF)
}

fn failed(error: Error) -> Outcome {
    Outcome::Failed {
        error: ItemError {
            kind: error.kind(),
            message: error.to_string(),
        },
    }
}

fn cancelled() -> Outcome {
    failed(Error::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::HeaderValue;

    fn headers(range: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_RANGE, HeaderValue::from_str(range).unwrap());
        headers
    }

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range(&headers("bytes 100-199/200")),
            Some((100, Some(200)))
        );
        assert_eq!(
            parse_content_range(&headers("bytes 0-9/*")),
            Some((0, None))
        );
        assert_eq!(parse_content_range(&headers("bytes */200")), None);
        assert_eq!(unsatisfied_total(&headers("bytes */200")), Some(200));
        assert_eq!(unsatisfied_total(&headers("bytes 0-1/2")), None);
    }

    #[test]
    fn backoff_doubles_with_bounded_jitter() {
        let base = Duration::from_millis(100);
        for attempt in 1..=4u32 {
            let delay = backoff(base, attempt);
            let nominal = base * 2u32.pow(attempt - 1);
            assert!(delay >= nominal, "{delay:?} < {nominal:?}");
            assert!(delay <= nominal.mul_f64(1.0 + MAX_JITTER), "{delay:?}");
        }
        assert_eq!(backoff(Duration::from_secs(3600), 40), MAX_BACKOFF);
        assert_eq!(backoff(Duration::ZERO, 3), Duration::ZERO);
    }

    #[test]
    fn statuses_are_classified() {
        assert!(matches!(
            classify_status(StatusCode::FORBIDDEN),
            Failure::Expired(403)
        ));
        assert!(matches!(
            classify_status(StatusCode::GONE),
            Failure::Expired(410)
        ));
        assert!(matches!(
            classify_status(StatusCode::NOT_FOUND),
            Failure::Fatal(Error::NotFound(_))
        ));
        assert!(matches!(
            classify_status(StatusCode::SERVICE_UNAVAILABLE),
            Failure::Retryable(Error::Http { status: 503, .. })
        ));
        assert!(matches!(
            classify_status(StatusCode::TOO_MANY_REQUESTS),
            Failure::Retryable(Error::Http { status: 429, .. })
        ));
        assert!(matches!(
            classify_status(StatusCode::UNAUTHORIZED),
            Failure::Fatal(Error::Http { status: 401, .. })
        ));
    }

    #[test]
    fn html_detection() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("Text/HTML; charset=utf-8"),
        );
        assert!(is_html(&headers));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
        assert!(!is_html(&headers));
        assert!(!is_html(&HeaderMap::new()));
    }
}
