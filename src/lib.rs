//! `igdl`: a JDownloader-style Instagram download manager.
//!
//! See `AGENTS.md` for the architecture; the binary entry point is `src/main.rs`.

pub mod app;
pub mod archive;
pub mod cli;
pub mod config;
pub mod cookies;
pub mod download;
pub mod error;
pub mod fsutil;
#[cfg(feature = "gui")]
pub mod gui;
pub mod instagram;
pub mod links;
pub mod model;
pub mod naming;
pub mod progress;
pub mod queue;
pub mod redact;
pub mod watch;

pub use error::{Error, ErrorKind, Result};
