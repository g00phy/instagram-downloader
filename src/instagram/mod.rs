//! Instagram web app: authenticated client, GraphQL operations and their doc
//! ids, page parsing, response models, media extraction and the crawler that
//! turns a [`crate::links::Target`] into files.

pub mod client;
pub mod crawler;
pub mod doc_ids;
pub mod extract;
mod graphql;
pub mod models;
pub mod web;

pub use client::{
    Client, ClientConfig, DEFAULT_API_BASE, DEFAULT_API_DELAY, DEFAULT_STATIC_BASE,
    DEFAULT_USER_AGENT, UserSummary,
};
pub use crawler::{CrawlOptions, Resolved, refresh_url, resolve};
