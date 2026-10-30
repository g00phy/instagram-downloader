//! GraphQL operations of the Instagram web app and their `doc_id`s.
//!
//! The web app runs persisted GraphQL queries: the request names a `doc_id`
//! and Instagram replaces those ids whenever it ships a new build. A doc id is
//! chosen in this order:
//!
//! 1. the `[doc_ids]` table of `config.toml` ([`crate::instagram::ClientConfig::doc_ids`]);
//! 2. a fresh (< 24 h) entry of `<data dir>/doc_ids.json`, written by discovery;
//! 3. discovery: the profile page's `static.cdninstagram.com/rsrc.php/*.js`
//!    bundles are scanned for `__d("<Name>_instagramRelayOperation",…exports="<id>"`
//!    (like gallery-dl; the bundles are fetched without cookies);
//! 4. the built-in defaults below (gallery-dl 1.32.17's `QUERIES` table).
//!
//! Discovery failures are logged and fall back to the next source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// Name of the cache file in the data directory.
pub const CACHE_FILE: &str = "doc_ids.json";

/// How long a discovered doc id is trusted.
pub const CACHE_TTL_SECS: i64 = 24 * 60 * 60;

/// Most bundles fetched by one discovery run.
pub(crate) const MAX_BUNDLES: usize = 200;

/// Where a GraphQL operation is posted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// `POST /graphql/query` (gallery-dl's `_pagination_graphql`).
    GraphqlQuery,
    /// `POST /api/graphql` (gallery-dl's `_call_graphql`).
    ApiGraphql,
}

impl Endpoint {
    /// Path relative to the API base.
    pub fn path(self) -> &'static str {
        match self {
            Endpoint::GraphqlQuery => "/graphql/query",
            Endpoint::ApiGraphql => "/api/graphql",
        }
    }
}

/// One GraphQL operation `igdl` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operation {
    /// Relay operation name (`fb_api_req_friendly_name`, `X-FB-Friendly-Name`).
    pub name: &'static str,
    /// Built-in doc id (gallery-dl 1.32.17).
    pub default_doc_id: &'static str,
    /// Profile sub-page the web app runs it on (`""`, `"/reels"`, `"/tagged"`):
    /// the `Referer`, and the page whose bundles discovery scans.
    pub page_suffix: &'static str,
    /// `X-Root-Field-Name` and the field of `data` holding the result; `None`
    /// for operations whose whole `data` object is the result.
    pub root_field: Option<&'static str>,
    pub endpoint: Endpoint,
}

impl Operation {
    /// The page (path with trailing slash) the operation belongs to: the
    /// profile's (sub-)page, or the home page without a username.
    pub fn page_path(&self, username: Option<&str>) -> String {
        match username {
            Some(user) if !user.is_empty() => format!("/{user}{}/", self.page_suffix),
            _ => "/".to_string(),
        }
    }
}

/// Posts grid: `data.xdt_api__v1__feed__user_timeline_graphql_connection`.
pub const POSTS: Operation = Operation {
    name: "PolarisProfilePostsTabContentQuery_connection",
    default_doc_id: "39229011733413759",
    page_suffix: "",
    root_field: Some("xdt_api__v1__feed__user_timeline_graphql_connection"),
    endpoint: Endpoint::GraphqlQuery,
};

/// Reels tab: `data.fetch__XDTUserDict.<connection>.edges[].node.media.code`.
pub const REELS: Operation = Operation {
    name: "PolarisProfileReelsTabContentQuery_connection",
    default_doc_id: "28989741540630190",
    page_suffix: "/reels",
    root_field: Some("fetch__XDTUserDict"),
    endpoint: Endpoint::GraphqlQuery,
};

/// Tagged tab: `data.xdt_api__v1__usertags__user_id__feed_connection`.
pub const TAGGED: Operation = Operation {
    name: "PolarisProfileTaggedTabContentQuery_connection",
    default_doc_id: "28794602806842110",
    page_suffix: "/tagged",
    root_field: Some("xdt_api__v1__usertags__user_id__feed_connection"),
    endpoint: Endpoint::GraphqlQuery,
};

/// Highlights tray: `data.highlights.edges[].node{id, title}`.
pub const HIGHLIGHTS_TRAY: Operation = Operation {
    name: "PolarisProfileStoryHighlightsTrayContentQuery",
    default_doc_id: "26970053832668570",
    page_suffix: "",
    root_field: None,
    endpoint: Endpoint::ApiGraphql,
};

/// Story / highlight reels: `data.xdt_api__v1__feed__reels_media__connection`.
pub const STORIES: Operation = Operation {
    name: "PolarisStoriesV3ReelPageGalleryQuery",
    default_doc_id: "28262315486766731",
    page_suffix: "",
    root_field: Some("xdt_api__v1__feed__reels_media__connection"),
    endpoint: Endpoint::GraphqlQuery,
};

/// Every operation `igdl` uses (the names accepted in `[doc_ids]`).
pub const OPERATIONS: [Operation; 5] = [POSTS, REELS, TAGGED, HIGHLIGHTS_TRAY, STORIES];

/// The operation called `name`.
pub fn operation(name: &str) -> Option<&'static Operation> {
    OPERATIONS.iter().find(|op| op.name == name)
}

/// The names accepted in `[doc_ids]`, comma-separated (for messages).
pub fn operation_names() -> String {
    OPERATIONS
        .iter()
        .map(|op| op.name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `id` looks like a doc id (decimal digits only).
pub fn is_doc_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_digit())
}

/// `__d("<Name>_instagramRelayOperation",[],(function(…)…exports="<id>"` —
/// gallery-dl's pattern (`.` does not cross line breaks, as in Python).
static RELAY_OPERATION_RE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"__d\("(\w+)_instagramRelayOperation",\[\],\(function\([^)]+\).+?\bexports="([^"]+)""#,
    )
    .ok()
});

/// The `(operation name, doc id)` pairs of the operations `igdl` uses found in
/// a JS bundle.
pub fn extract_doc_ids(script: &str) -> Vec<(&'static str, String)> {
    let Some(re) = RELAY_OPERATION_RE.as_ref() else {
        return Vec::new();
    };
    re.captures_iter(script)
        .filter_map(|caps| {
            let op = operation(caps.get(1)?.as_str())?;
            let id = caps.get(2)?.as_str();
            is_doc_id(id).then(|| (op.name, id.to_string()))
        })
        .collect()
}

/// One discovered doc id in the cache file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedDocId {
    pub doc_id: String,
    /// Unix time of discovery.
    pub found_at: i64,
}

impl CachedDocId {
    /// Discovered less than [`CACHE_TTL_SECS`] before `now` (and not in the future).
    pub fn is_fresh(&self, now: i64) -> bool {
        self.found_at <= now + 300 && now - self.found_at < CACHE_TTL_SECS
    }
}

/// Contents of `doc_ids.json`: operation name → discovered doc id.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocIdCache {
    #[serde(default)]
    pub queries: BTreeMap<String, CachedDocId>,
}

impl DocIdCache {
    /// Reads the cache file; a missing, unreadable or malformed file is empty
    /// (logged at debug level), and unknown or malformed entries are dropped.
    pub fn load(path: &Path) -> DocIdCache {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    tracing::debug!(path = %path.display(), error = %err, "cannot read the doc id cache");
                }
                return DocIdCache::default();
            }
        };
        let mut cache: DocIdCache = match serde_json::from_str(&text) {
            Ok(cache) => cache,
            Err(err) => {
                tracing::debug!(path = %path.display(), error = %err, "ignoring a malformed doc id cache");
                return DocIdCache::default();
            }
        };
        cache
            .queries
            .retain(|name, entry| operation(name).is_some() && is_doc_id(&entry.doc_id));
        cache
    }

    /// The fresh doc id of `name`.
    pub fn fresh(&self, name: &str, now: i64) -> Option<&str> {
        self.queries
            .get(name)
            .filter(|entry| entry.is_fresh(now))
            .map(|entry| entry.doc_id.as_str())
    }

    /// Records discovered ids.
    pub fn record(&mut self, found: &BTreeMap<&'static str, String>, now: i64) {
        for (name, id) in found {
            self.queries.insert(
                (*name).to_string(),
                CachedDocId {
                    doc_id: id.clone(),
                    found_at: now,
                },
            );
        }
    }

    /// Writes the cache to `path` atomically (temp file + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
        tmp_name.push(".tmp");
        let tmp = path.with_file_name(tmp_name);
        std::fs::write(&tmp, text)?;
        crate::fsutil::rename_with_retry(&tmp, path).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
    }
}

/// Doc id bookkeeping of one client: overrides, the cache file and what
/// discovery already did in this session.
#[derive(Debug, Default)]
pub(crate) struct DocIdState {
    /// `[doc_ids]` from the config (validated names and ids).
    pub overrides: BTreeMap<String, String>,
    /// `<data dir>/doc_ids.json`, or `None` to keep discoveries in memory.
    pub cache_path: Option<PathBuf>,
    pub cache: DocIdCache,
    pub cache_loaded: bool,
    /// Pages whose bundles were scanned in this session.
    pub scanned_pages: std::collections::HashSet<String>,
    /// Operations whose doc id was rediscovered after an "outdated" error.
    pub rediscovered: std::collections::HashSet<&'static str>,
}

impl DocIdState {
    /// State for the configured overrides and cache directory. Overrides with
    /// unknown names or non-numeric ids are dropped with a warning (the config
    /// loader already refuses them).
    pub fn new(overrides: &BTreeMap<String, String>, cache_dir: Option<&Path>) -> Self {
        let mut valid = BTreeMap::new();
        for (name, id) in overrides {
            let id = id.trim();
            if operation(name).is_some() && is_doc_id(id) {
                valid.insert(name.clone(), id.to_string());
            } else {
                tracing::warn!(query = %name, "ignoring an invalid [doc_ids] entry");
            }
        }
        DocIdState {
            overrides: valid,
            cache_path: cache_dir.map(|dir| dir.join(CACHE_FILE)),
            ..DocIdState::default()
        }
    }

    /// Loads the cache file once.
    pub fn ensure_loaded(&mut self) {
        if self.cache_loaded {
            return;
        }
        self.cache_loaded = true;
        if let Some(path) = &self.cache_path {
            self.cache = DocIdCache::load(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_paths() {
        assert_eq!(POSTS.page_path(Some("alice")), "/alice/");
        assert_eq!(REELS.page_path(Some("alice")), "/alice/reels/");
        assert_eq!(TAGGED.page_path(Some("alice")), "/alice/tagged/");
        assert_eq!(STORIES.page_path(None), "/");
    }

    #[test]
    fn extracts_relay_operations() {
        let script = concat!(
            r#"__d("PolarisProfilePostsTabContentQuery_connection_instagramRelayOperation",[],(function(a,b,c,d,e,f){e.exports="1234567890"}),null);"#,
            "\n",
            r#"__d("SomethingElse_instagramRelayOperation",[],(function(a,b,c,d,e,f){e.exports="42"}),null);"#,
            "\n",
            r#"__d("PolarisStoriesV3ReelPageGalleryQuery_instagramRelayOperation",[],(function(a,b,c,d,e,f){e.exports="not-a-number"}),null);"#,
            "\n",
            r#"__d("PolarisProfileReelsTabContentQuery_connection_instagramRelayOperation",[],(function(a,b,c,d,e,f){"use strict";e.exports="987"}),null);"#,
        );
        assert_eq!(
            extract_doc_ids(script),
            vec![
                (POSTS.name, "1234567890".to_string()),
                (REELS.name, "987".to_string())
            ]
        );
    }

    #[test]
    fn cache_round_trip_and_freshness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join(CACHE_FILE);
        assert_eq!(DocIdCache::load(&path), DocIdCache::default());
        let mut cache = DocIdCache::default();
        let found = BTreeMap::from([(POSTS.name, "111".to_string())]);
        cache.record(&found, 1_000_000);
        cache.save(&path).unwrap();
        let loaded = DocIdCache::load(&path);
        assert_eq!(loaded, cache);
        assert_eq!(loaded.fresh(POSTS.name, 1_000_000 + 60), Some("111"));
        assert_eq!(loaded.fresh(POSTS.name, 1_000_000 + CACHE_TTL_SECS), None);
        assert_eq!(loaded.fresh(REELS.name, 1_000_000), None);

        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(DocIdCache::load(&path), DocIdCache::default());
        std::fs::write(
            &path,
            r#"{"queries":{"Bogus":{"doc_id":"1","found_at":1},"PolarisProfilePostsTabContentQuery_connection":{"doc_id":"x","found_at":1}}}"#,
        )
        .unwrap();
        assert!(DocIdCache::load(&path).queries.is_empty());
    }

    #[test]
    fn overrides_are_validated() {
        let overrides = BTreeMap::from([
            (POSTS.name.to_string(), " 555 ".to_string()),
            ("Unknown".to_string(), "1".to_string()),
            (REELS.name.to_string(), "abc".to_string()),
        ]);
        let state = DocIdState::new(&overrides, None);
        assert_eq!(
            state.overrides,
            BTreeMap::from([(POSTS.name.to_string(), "555".to_string())])
        );
    }
}
