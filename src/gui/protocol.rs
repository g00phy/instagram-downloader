//! Messages between the window ([`super::view`]) and the background
//! [`super::controller`]. The window never touches the network, the queue file or
//! the archive; it sends [`Command`]s and renders the [`Update`]s it receives.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Settings;
use crate::links::ProfileSection;
use crate::model::{MediaFile, MediaKind};
use crate::queue::Queue;

/// One LinkGrabber run: resolve the links in `text` with these options.
#[derive(Debug, Clone, PartialEq)]
pub struct GrabRequest {
    /// Pasted text; links are extracted with [`crate::links::read_links`].
    pub text: String,
    /// Sections crawled for plain profile links.
    pub sections: Vec<ProfileSection>,
    /// Max posts per profile section (`None` = no limit).
    pub limit: Option<usize>,
    pub only: Option<MediaKind>,
    /// `YYYY-MM-DD` or empty.
    pub since: String,
    /// `YYYY-MM-DD` or empty.
    pub until: String,
    /// Stop profile crawls at the first already-downloaded post.
    pub update: bool,
    /// Download root for the resulting packages.
    pub out_dir: PathBuf,
    /// Output path template.
    pub template: String,
}

/// What the window asks the controller to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Resolve links into staged packages (shown in the LinkGrabber).
    Grab(GrabRequest),
    /// Move staged packages into the download list; `start` downloads them right away.
    Accept { staged: Vec<u64>, start: bool },
    /// Drop staged packages (empty = all).
    Discard { staged: Vec<u64> },
    /// Download every queued item.
    Start,
    /// Stop running downloads; their items go back to queued (partial files are kept).
    Stop,
    /// Re-queue failed items among these package/item ids (empty = all failed).
    Retry(Vec<u64>),
    /// Remove packages/items from the download list (files on disk are kept).
    Remove(Vec<u64>),
    /// Remove packages whose items are all done or skipped.
    ClearFinished,
    /// Ask Instagram which account the cookies belong to.
    CheckAccount,
    /// Validate, save to `config.toml` and apply new settings.
    ApplySettings(Box<Settings>),
    /// Stop everything, save, and end the controller.
    Shutdown,
}

/// A resolved link waiting in the LinkGrabber.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedPackage {
    /// Staging id (separate from queue ids).
    pub id: u64,
    pub name: String,
    /// The link it came from.
    pub source: String,
    pub out_dir: PathBuf,
    pub files: Vec<StagedFile>,
    /// Files left out because they were downloaded before.
    pub archived: usize,
    /// Files already in the download list (they will not be added twice).
    pub already_queued: usize,
    /// Crawler notes (filters, unavailable sections, …).
    pub notes: Vec<String>,
}

/// One file of a staged package.
#[derive(Debug, Clone, PartialEq)]
pub struct StagedFile {
    pub media: MediaFile,
    /// Output path relative to `out_dir`.
    pub rel_path: PathBuf,
}

/// Severity of a log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// One line for the Log tab (already redacted).
#[derive(Debug, Clone, PartialEq)]
pub struct LogLine {
    /// Unix seconds.
    pub at: i64,
    pub level: LogLevel,
    pub text: String,
}

/// What is known about the cookies' account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountStatus {
    /// No cookie file configured.
    NoCookies,
    /// Cookies loaded and valid offline, not yet checked with Instagram.
    Loaded,
    /// A [`Command::CheckAccount`] is running.
    Checking,
    /// Instagram confirmed the session.
    LoggedIn { username: String },
    /// The cookies look valid offline, but Instagram could not confirm them right
    /// now (rate limit, network): not proof they are bad.
    Unverified(String),
    /// The cookie file is missing/invalid/expired, or Instagram rejected it.
    Invalid(String),
}

/// What the controller tells the window.
#[derive(Debug, Clone)]
pub enum Update {
    /// The effective settings (sent at start and after `ApplySettings`), and
    /// where they are saved.
    Settings {
        settings: Box<Settings>,
        config_path: Option<PathBuf>,
        /// The download root: the settings' folder, else the window's default
        /// (`Downloads/Instagram`, see the controller).
        default_out_dir: PathBuf,
        /// The cookie file named by `--cookies` / `IGDL_COOKIES`, which is used
        /// instead of `settings.cookies` while it is set.
        cookies_override: Option<PathBuf>,
    },
    /// The whole download list, after any change (throttled to a few per second).
    Queue(Arc<Queue>),
    /// Live byte counts of a running item (between queue snapshots).
    Progress {
        item_id: u64,
        done: u64,
        total: Option<u64>,
    },
    /// The complete LinkGrabber contents.
    Staged(Vec<StagedPackage>),
    /// LinkGrabber activity: `pending_links` still to resolve (0 = idle).
    Grabbing {
        pending_links: usize,
    },
    /// Whether downloads are running.
    Downloading(bool),
    Account(AccountStatus),
    Log(LogLine),
    /// The controller cannot work (e.g. another igdl holds the queue lock).
    Fatal(String),
}
