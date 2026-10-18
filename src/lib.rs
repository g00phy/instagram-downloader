//! `igdl`: a JDownloader-style Instagram download manager.
//!
//! See `AGENTS.md` for the architecture; the binary entry point is `src/main.rs`.

pub mod archive;
pub mod config;
pub mod cookies;
pub mod download;
pub mod error;
pub mod instagram;
pub mod links;
pub mod model;
pub mod naming;
pub mod queue;
pub mod redact;

pub use error::{Error, ErrorKind, Result};
