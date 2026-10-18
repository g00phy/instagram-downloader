//! Cookie files → [`CookieJar`].
//!
//! Accepts Netscape `cookies.txt`, JSON exports (Cookie-Editor / EditThisCookie
//! arrays, Playwright `storageState`) and plain `name=value; …` header strings.
//! Only `instagram.com` cookies are kept. Cookie values never appear in `Debug`
//! output, logs or error messages.

use std::fmt;
use std::path::Path;

use crate::error::Result;

/// Domain assumed for cookies that come without one (header-string format).
pub const DEFAULT_DOMAIN: &str = ".instagram.com";

/// One cookie. `Debug` hides the value.
#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// As written in the source, e.g. `.instagram.com`.
    pub domain: String,
    pub path: String,
    /// Unix expiry time; `None` for session cookies.
    pub expires: Option<i64>,
    pub secure: bool,
    pub http_only: bool,
}

impl Cookie {
    /// A cookie for [`DEFAULT_DOMAIN`] with path `/` and no expiry.
    pub fn new(name: impl Into<String>, value: impl Into<String>) -> Self {
        Cookie {
            name: name.into(),
            value: value.into(),
            domain: DEFAULT_DOMAIN.to_string(),
            path: "/".to_string(),
            expires: None,
            secure: true,
            http_only: false,
        }
    }
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("expires", &self.expires)
            .finish()
    }
}

/// The Instagram cookies `igdl` authenticates with. `Debug` lists names only.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CookieJar {
    cookies: Vec<Cookie>,
}

impl CookieJar {
    /// Builds a jar; later cookies with the same name replace earlier ones.
    pub fn new(cookies: Vec<Cookie>) -> Self {
        let mut jar = CookieJar::default();
        for cookie in cookies {
            jar.insert(cookie);
        }
        jar
    }

    /// Convenience constructor (mostly for tests): `[("sessionid", "…")]`.
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        CookieJar::new(
            pairs
                .into_iter()
                .map(|(name, value)| Cookie::new(name, value))
                .collect(),
        )
    }

    /// Inserts or replaces the cookie with the same name.
    pub fn insert(&mut self, cookie: Cookie) {
        match self.cookies.iter_mut().find(|c| c.name == cookie.name) {
            Some(existing) => *existing = cookie,
            None => self.cookies.push(cookie),
        }
    }

    /// Removes a cookie by name.
    pub fn remove(&mut self, name: &str) {
        self.cookies.retain(|c| c.name != name);
    }

    /// The value of the named cookie.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.value.as_str())
    }

    /// All cookies, in insertion order.
    pub fn cookies(&self) -> &[Cookie] {
        &self.cookies
    }

    /// Cookie names, in insertion order (safe to display).
    pub fn names(&self) -> Vec<&str> {
        self.cookies.iter().map(|c| c.name.as_str()).collect()
    }

    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty()
    }

    /// `Cookie` request header value (`a=1; b=2`), or `None` when empty.
    pub fn header_value(&self) -> Option<String> {
        if self.cookies.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        Some(parts.join("; "))
    }

    /// Applies one `Set-Cookie` response header (Instagram rotates `csrftoken`,
    /// `rur`, …). Deletions (`Max-Age=0`, empty value, past `Expires`) remove the
    /// cookie. Changes stay in memory; the user's cookie file is never written.
    pub fn apply_set_cookie(&mut self, header: &str) {
        let _ = header;
        todo!("cookies agent")
    }
}

impl fmt::Debug for CookieJar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CookieJar")
            .field("names", &self.names())
            .finish()
    }
}

/// Cookie file formats `igdl` understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CookieFormat {
    /// Netscape / Mozilla `cookies.txt`.
    Netscape,
    /// JSON array of cookie objects, or `{ "cookies": [...] }`.
    Json,
    /// `name=value; name2=value2`.
    Header,
}

/// Guesses the format of a cookie file's contents.
pub fn detect_format(text: &str) -> CookieFormat {
    let _ = text;
    todo!("cookies agent")
}

/// Parses cookie file contents (any [`CookieFormat`]), keeping only
/// `instagram.com` cookies. Errors with [`crate::error::Error::Cookies`] when the
/// text is malformed or contains no Instagram cookies.
pub fn parse(text: &str) -> Result<CookieJar> {
    let _ = text;
    todo!("cookies agent")
}

/// Reads and parses a cookie file.
pub fn load(path: &Path) -> Result<CookieJar> {
    let _ = path;
    todo!("cookies agent")
}

/// What `igdl cookies check` reports (names and metadata only, never values).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CookieReport {
    /// Names of the Instagram cookies found.
    pub names: Vec<String>,
    /// Expiry of `sessionid`, if it has one.
    pub session_expires: Option<i64>,
    /// Logged-in user id from `ds_user_id`.
    pub user_id: Option<String>,
    /// Non-fatal problems, e.g. "csrftoken missing".
    pub warnings: Vec<String>,
}

/// Checks that the jar can authenticate: `sessionid` present and not expired at
/// `now` (unix seconds). Missing/expired `sessionid` → [`crate::error::Error::Auth`].
pub fn validate(jar: &CookieJar, now: i64) -> Result<CookieReport> {
    let _ = (jar, now);
    todo!("cookies agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_shows_values() {
        let jar = CookieJar::from_pairs([("sessionid", "SECRET-VALUE"), ("csrftoken", "tok")]);
        let shown = format!("{jar:?} {:?}", jar.cookies());
        assert!(!shown.contains("SECRET-VALUE"));
        assert!(!shown.contains("tok\""));
        assert!(shown.contains("sessionid"));
    }

    #[test]
    fn header_value_joins_pairs() {
        let jar = CookieJar::from_pairs([("a", "1"), ("b", "2"), ("a", "3")]);
        assert_eq!(jar.header_value().as_deref(), Some("a=3; b=2"));
        assert_eq!(CookieJar::default().header_value(), None);
    }
}
