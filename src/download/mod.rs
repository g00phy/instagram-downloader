//! The download engine: bounded parallelism, `.part` files resumed with HTTP
//! `Range`, retries with jittered exponential backoff, a global bandwidth cap,
//! and refreshing of expired signed CDN URLs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::model::{ItemError, MediaFile};

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
    _private: (),
}

impl Downloader {
    pub fn new(config: DownloadConfig) -> Result<Self> {
        let _ = config;
        todo!("download agent")
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
        let _ = (jobs, refresher, events, cancel);
        todo!("download agent")
    }
}
