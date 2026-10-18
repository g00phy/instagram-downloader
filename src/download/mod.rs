//! The download engine: bounded parallelism, `.part` files resumed with HTTP
//! `Range`, retries with jittered exponential backoff, a global bandwidth cap,
//! and refreshing of expired signed CDN URLs.

mod files;
mod limiter;
mod transfer;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, ErrorKind, Result};
use crate::model::{ItemError, MediaFile};

pub use files::part_path;
use limiter::RateLimiter;

/// Engine settings.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// Maximum concurrent transfers (≥ 1).
    pub jobs: usize,
    /// Retries per file for retryable failures.
    pub retries: u32,
    /// First retry delay; doubles each attempt, with random jitter.
    pub retry_base: Duration,
    /// Connect / read-idle timeout.
    pub timeout: Duration,
    pub user_agent: String,
    /// Global cap in bytes per second across all transfers.
    pub limit_rate: Option<u64>,
    /// Set the finished file's mtime to `media.taken_at`.
    pub set_mtime: bool,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        DownloadConfig {
            jobs: 4,
            retries: 5,
            retry_base: Duration::from_secs(2),
            timeout: Duration::from_secs(30),
            user_agent: crate::instagram::DEFAULT_USER_AGENT.to_string(),
            limit_rate: None,
            set_mtime: true,
        }
    }
}

/// One file to fetch.
#[derive(Debug, Clone)]
pub struct Job {
    /// Queue item id, echoed in every [`Event`].
    pub item_id: u64,
    pub media: MediaFile,
    /// Absolute final path. The partial file is `<dest>.part`.
    pub dest: PathBuf,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Downloaded to `path` (`bytes` total size).
    Done { path: PathBuf, bytes: u64 },
    /// Nothing downloaded, e.g. the destination already exists.
    Skipped { path: PathBuf, reason: String },
    /// Gave up; `error.kind == Cancelled` when interrupted (the `.part` file is kept).
    Failed { error: ItemError },
}

/// Progress reported by [`Downloader::run`].
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A transfer (re)started; `resumed_from` > 0 when continuing a `.part` file.
    Started {
        item_id: u64,
        name: String,
        total: Option<u64>,
        resumed_from: u64,
    },
    /// Bytes on disk so far (throttled to a few updates per second per file).
    Progress {
        item_id: u64,
        done: u64,
        total: Option<u64>,
    },
    /// A retryable failure; the next attempt starts after `delay`.
    Retrying {
        item_id: u64,
        attempt: u32,
        delay: Duration,
        reason: String,
    },
    /// The signed URL expired and was replaced (new URL, for persisting).
    Refreshed { item_id: u64, url: String },
    /// Exactly one per job that was started.
    Finished { item_id: u64, outcome: Outcome },
}

/// Supplies a fresh URL when a CDN URL expired (403/410). Implemented by the app
/// on top of [`crate::instagram::refresh_url`].
pub trait UrlRefresher: Send + Sync {
    fn refresh<'a>(&'a self, media: &'a MediaFile) -> BoxFuture<'a, Result<String>>;
}

/// The engine. Cookies are never sent to the CDN.
pub struct Downloader {
    inner: Arc<Inner>,
}

/// State shared by every transfer task of a [`Downloader`].
struct Inner {
    /// Plain client: no cookie store, no compression (byte offsets must match the
    /// file on the server for `Range` resume).
    client: reqwest::Client,
    config: DownloadConfig,
    /// One bucket for all transfers, so `limit_rate` caps the aggregate.
    limiter: Option<RateLimiter>,
}

impl std::fmt::Debug for Downloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Downloader")
            .field("config", &self.inner.config)
            .finish_non_exhaustive()
    }
}

impl Downloader {
    /// Builds the HTTP client. Fails with [`Error::Config`] if the user agent is
    /// not a valid header value or the TLS backend cannot be initialised.
    pub fn new(config: DownloadConfig) -> Result<Self> {
        let mut builder = reqwest::Client::builder()
            .user_agent(config.user_agent.as_str())
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd();
        if !config.timeout.is_zero() {
            builder = builder
                .connect_timeout(config.timeout)
                .read_timeout(config.timeout);
        }
        let client = builder.build().map_err(|err| {
            Error::Config(format!(
                "cannot set up the download client (check the user agent): {}",
                err.without_url()
            ))
        })?;
        let limiter = config
            .limit_rate
            .filter(|rate| *rate > 0)
            .map(RateLimiter::new);
        Ok(Downloader {
            inner: Arc::new(Inner {
                client,
                config,
                limiter,
            }),
        })
    }

    /// The settings this engine was built with.
    pub fn config(&self) -> &DownloadConfig {
        &self.inner.config
    }

    /// Runs all `jobs` with at most `config.jobs` in flight and returns when every
    /// started job has finished. After `cancel` fires no new job starts and running
    /// transfers stop promptly (finishing with a `Cancelled` failure).
    pub async fn run(
        &self,
        jobs: Vec<Job>,
        refresher: Option<Arc<dyn UrlRefresher>>,
        events: UnboundedSender<Event>,
        cancel: CancellationToken,
    ) {
        let slots = Arc::new(Semaphore::new(self.inner.config.jobs.max(1)));
        let mut tasks = JoinSet::new();
        let mut running: HashMap<tokio::task::Id, u64> = HashMap::new();

        for job in jobs {
            let permit = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                permit = Arc::clone(&slots).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    // The semaphore is never closed; stop scheduling if it ever is.
                    Err(_) => break,
                },
            };
            let item_id = job.item_id;
            let inner = Arc::clone(&self.inner);
            let refresher = refresher.clone();
            let events = events.clone();
            let cancel = cancel.clone();
            let handle = tasks.spawn(async move {
                let _permit = permit;
                transfer::run_job(&inner, job, refresher.as_deref(), &events, &cancel).await;
            });
            running.insert(handle.id(), item_id);
        }

        while let Some(joined) = tasks.join_next_with_id().await {
            let Err(err) = joined else { continue };
            // A task only ends without its `Finished` event if it panicked.
            tracing::error!(error = %err, "a download task stopped unexpectedly");
            if let Some(item_id) = running.get(&err.id()).copied() {
                let _ = events.send(Event::Finished {
                    item_id,
                    outcome: Outcome::Failed {
                        error: ItemError {
                            kind: ErrorKind::Io,
                            message: "the download task stopped unexpectedly".to_string(),
                        },
                    },
                });
            }
        }
    }
}
