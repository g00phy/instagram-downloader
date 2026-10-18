//! Instagram web API: authenticated client, response models, media extraction
//! and the crawler that turns a [`crate::links::Target`] into files.

pub mod client;
pub mod crawler;
pub mod extract;
pub mod models;

pub use client::{Client, ClientConfig, DEFAULT_API_BASE, DEFAULT_USER_AGENT, UserSummary};
pub use crawler::{CrawlOptions, Resolved, refresh_url, resolve};
