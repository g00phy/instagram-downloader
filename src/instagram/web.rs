//! What `igdl` reads from Instagram's HTML pages (fetched like a browser
//! navigation): the profile id, the tokens GraphQL requests need, and the JS
//! bundles doc id discovery scans.
//!
//! Patterns follow gallery-dl 1.32.17 (`_user_by_web_impl`,
//! `_extract_fb_tokens`, `_extract_docid_map`) plus a few fallbacks. Every
//! value is optional: Instagram changes its pages often.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

/// Values read from one page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageInfo {
    /// Numeric id of the profile the page shows.
    pub profile_id: Option<String>,
    /// Username of the profile (`"params":{"username":…}`).
    pub username: Option<String>,
    /// `is_private` when every occurrence on the page agrees.
    pub is_private: Option<bool>,
    /// `followed_by_viewer` when every occurrence on the page agrees.
    pub followed_by_viewer: Option<bool>,
    /// GraphQL tokens and build identifiers.
    pub tokens: Tokens,
    /// Absolute URLs of the page's `rsrc.php` JS bundles, in page order, unique.
    pub bundles: Vec<String>,
    /// The page looks like Instagram's login page.
    pub login_form: bool,
}

/// Request tokens of the web app. `fb_dtsg` is session-bound: never log it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Tokens {
    /// `lsd` (`X-FB-LSD`).
    pub lsd: Option<String>,
    /// `fb_dtsg`.
    pub fb_dtsg: Option<String>,
    /// `__hs` (haste session) of the page's build.
    pub haste_session: Option<String>,
    /// `__rev` (server revision) of the page's build.
    pub revision: Option<String>,
    /// `__hsi` of the page load.
    pub hsi: Option<String>,
}

impl Tokens {
    /// Whether the page had an `lsd` or `fb_dtsg` token.
    pub fn is_empty(&self) -> bool {
        self.lsd.is_none() && self.fb_dtsg.is_none()
    }
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shown = |value: &Option<String>| if value.is_some() { "set" } else { "unset" };
        f.debug_struct("Tokens")
            .field("lsd", &shown(&self.lsd))
            .field("fb_dtsg", &shown(&self.fb_dtsg))
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

fn regex(pattern: &str) -> Option<Regex> {
    // The patterns are constants covered by the unit tests; `None` (never
    // expected) only disables that pattern.
    Regex::new(pattern).ok()
}

static PROFILE_ID_RES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#""profile_id":"(\d+)""#,
        r#""page_id":"profilePage_(\d+)""#,
        r#""xig_user_by_username":\{[^{}]{0,4000}?"pk":"?(\d+)"#,
    ]
    .into_iter()
    .filter_map(regex)
    .collect()
});

static USERNAME_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r#""params":\{"username":"([A-Za-z0-9._]+)""#));

static IS_PRIVATE_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r#""is_private":(true|false)\b"#));

static FOLLOWED_RE: LazyLock<Option<Regex>> =
    LazyLock::new(|| regex(r#""followed_by_viewer":(true|false)\b"#));

/// Parses a page. `static_base` is the scheme + host bundles are served from
/// (`https://static.cdninstagram.com`).
pub fn parse_page(html: &str, static_base: &str) -> PageInfo {
    PageInfo {
        profile_id: PROFILE_ID_RES
            .iter()
            .find_map(|re| re.captures(html)?.get(1).map(|m| m.as_str().to_string())),
        username: USERNAME_RE
            .as_ref()
            .and_then(|re| re.captures(html)?.get(1))
            .map(|m| m.as_str().to_string()),
        is_private: agreed_flag(IS_PRIVATE_RE.as_ref(), html),
        followed_by_viewer: agreed_flag(FOLLOWED_RE.as_ref(), html),
        tokens: tokens(html),
        bundles: bundles(html, static_base),
        login_form: looks_like_login_page(html),
    }
}

/// `true`/`false` when every match of `re` says the same, else `None`.
fn agreed_flag(re: Option<&Regex>, html: &str) -> Option<bool> {
    let mut agreed = None;
    for caps in re?.captures_iter(html) {
        let value = caps.get(1)?.as_str() == "true";
        match agreed {
            None => agreed = Some(value),
            Some(previous) if previous != value => return None,
            Some(_) => {}
        }
    }
    agreed
}

/// The text between the first `start` and the next `end` (gallery-dl's `text.extr`).
fn extract<'a>(html: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let from = html.find(start)? + start.len();
    let len = html[from..].find(end)?;
    Some(&html[from..from + len]).filter(|value| !value.is_empty())
}

/// The JSON object of `<script … id="__eqmc">…</script>`.
fn eqmc(html: &str) -> Option<serde_json::Map<String, Value>> {
    let pos = html.find(" id=\"__eqmc\"")?;
    let start = pos + html[pos..].find('>')? + 1;
    let end = start + html[start..].find("</script>")?;
    match serde_json::from_str(html[start..end].trim()) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

/// A non-empty string value of `map[key]`.
fn string_value(map: Option<&serde_json::Map<String, Value>>, key: &str) -> Option<String> {
    map?.get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn tokens(html: &str) -> Tokens {
    let eqmc = eqmc(html);
    let eqmc = eqmc.as_ref();
    // `u` is the URL of the page's own early request; its query carries the
    // build identifiers the web app sends with every request.
    let query: Vec<(String, String)> = string_value(eqmc, "u")
        .and_then(|u| u.split_once('?').map(|(_, q)| q.to_string()))
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect())
        .unwrap_or_default();
    let param = |name: &str| {
        query
            .iter()
            .find(|(key, value)| key == name && !value.is_empty())
            .map(|(_, value)| value.clone())
    };
    let plain = |value: Option<&str>| value.map(str::to_string);
    Tokens {
        lsd: string_value(eqmc, "l")
            .or_else(|| plain(extract(html, "\"lsd\":\"", "\"")))
            .or_else(|| plain(extract(html, "\"LSD\",[],{\"token\":\"", "\""))),
        fb_dtsg: string_value(eqmc, "f")
            .or_else(|| plain(extract(html, "\"dtsg\":{\"token\":\"", "\""))),
        haste_session: param("__hs").or_else(|| plain(extract(html, "\"haste_session\":\"", "\""))),
        revision: param("__rev")
            .or_else(|| plain(extract(html, "\"server_revision\":", ",")))
            .filter(|rev| rev.bytes().all(|b| b.is_ascii_digit())),
        hsi: param("__hsi")
            .or_else(|| plain(extract(html, "\"hsi\":\"", "\"")))
            .filter(|hsi| hsi.bytes().all(|b| b.is_ascii_digit())),
    }
}

/// `href="…/rsrc.php/….js"` and `src="…"` attributes under `static_base`.
fn bundles(html: &str, static_base: &str) -> Vec<String> {
    let prefix = format!("{}/rsrc.php/", static_base.trim_end_matches('/'));
    let Some(re) = regex(&format!(
        r#"(?:href|src)="({}[^"]+)""#,
        regex::escape(&prefix)
    )) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for caps in re.captures_iter(html) {
        let Some(url) = caps.get(1) else { continue };
        let url = url.as_str().replace("&amp;", "&");
        let path = url.split(['?', '#']).next().unwrap_or_default();
        if path.ends_with(".js") && !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// Whether an HTML body is Instagram's login page.
pub fn looks_like_login_page(body: &str) -> bool {
    body.contains("/accounts/login") || body.contains("loginForm")
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATIC: &str = "https://static.cdninstagram.com";

    fn page() -> String {
        format!(
            r#"<!DOCTYPE html><html><head>
<link rel="preload" href="{STATIC}/rsrc.php/v4/yA/r/One.js" as="script" crossorigin="anonymous" />
<link rel="stylesheet" href="{STATIC}/rsrc.php/v4/yB/l/Two.css" />
<script src="{STATIC}/rsrc.php/v4i/yC/r/Three.js?_nc_x=1&amp;b=2" async></script>
<link rel="preload" href="{STATIC}/rsrc.php/v4/yA/r/One.js" as="script" />
<link rel="preload" href="https://evil.example/rsrc.php/v4/x/Four.js" as="script" />
<script type="application/json" id="__eqmc">{{"u":"\/ajax\/qm\/?__a=1&__hs=20731.HYP%3Ainstagram_web_pkg.2.1...0&__rev=1049285836&__hsi=7693193318680449885&__comet_req=7","f":"DTSG-TOKEN","l":"LSD-TOKEN"}}</script>
</head><body><script type="application/json">{{"require":[["PolarisProfilePageContentQuery",{{"params":{{"username":"alice"}}}}],{{"profile_id":"1001","is_private":false}}]}}</script></body></html>"#
        )
    }

    #[test]
    fn parses_profile_tokens_and_bundles() {
        let info = parse_page(&page(), STATIC);
        assert_eq!(info.profile_id.as_deref(), Some("1001"));
        assert_eq!(info.username.as_deref(), Some("alice"));
        assert_eq!(info.is_private, Some(false));
        assert_eq!(info.followed_by_viewer, None);
        assert_eq!(info.tokens.lsd.as_deref(), Some("LSD-TOKEN"));
        assert_eq!(info.tokens.fb_dtsg.as_deref(), Some("DTSG-TOKEN"));
        assert_eq!(
            info.tokens.haste_session.as_deref(),
            Some("20731.HYP:instagram_web_pkg.2.1...0")
        );
        assert_eq!(info.tokens.revision.as_deref(), Some("1049285836"));
        assert_eq!(info.tokens.hsi.as_deref(), Some("7693193318680449885"));
        assert_eq!(
            info.bundles,
            vec![
                format!("{STATIC}/rsrc.php/v4/yA/r/One.js"),
                format!("{STATIC}/rsrc.php/v4i/yC/r/Three.js?_nc_x=1&b=2"),
            ]
        );
        let debug = format!("{:?}", info.tokens);
        assert!(!debug.contains("DTSG-TOKEN") && !debug.contains("LSD-TOKEN"), "{debug}");
    }

    #[test]
    fn fallback_patterns() {
        let html = r#"{"page_id":"profilePage_77"} ["LSD",[],{"token":"lsd2"}] {"dtsg":{"token":"dt2"}} "is_private":true "is_private":true "followed_by_viewer":true "followed_by_viewer":false"#;
        let info = parse_page(html, STATIC);
        assert_eq!(info.profile_id.as_deref(), Some("77"));
        assert_eq!(info.tokens.lsd.as_deref(), Some("lsd2"));
        assert_eq!(info.tokens.fb_dtsg.as_deref(), Some("dt2"));
        assert_eq!(info.is_private, Some(true));
        assert_eq!(info.followed_by_viewer, None, "disagreeing values are unknown");

        let xig = r#"{"xig_user_by_username":{"username":"bob","pk":"4242","is_private":false}}"#;
        assert_eq!(parse_page(xig, STATIC).profile_id.as_deref(), Some("4242"));

        let lsd = r#"{"lsd":"plain-lsd"}"#;
        assert_eq!(
            parse_page(lsd, STATIC).tokens.lsd.as_deref(),
            Some("plain-lsd")
        );
        let empty = parse_page("<html></html>", STATIC);
        assert_eq!(empty, PageInfo::default());
        assert!(empty.tokens.is_empty());
    }

    #[test]
    fn bundles_follow_the_static_base() {
        let html = r#"<script src="http://127.0.0.1:9/rsrc.php/v4/a/B.js"></script>"#;
        assert_eq!(
            parse_page(html, "http://127.0.0.1:9/").bundles,
            vec!["http://127.0.0.1:9/rsrc.php/v4/a/B.js".to_string()]
        );
        assert!(parse_page(html, STATIC).bundles.is_empty());
    }

    #[test]
    fn login_page_detection() {
        assert!(looks_like_login_page("<form id=\"loginForm\">"));
        assert!(!looks_like_login_page("<html>oops</html>"));
    }
}
