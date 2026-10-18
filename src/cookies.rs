//! Cookie files → [`CookieJar`].
//!
//! Accepts Netscape `cookies.txt`, JSON exports (Cookie-Editor / EditThisCookie
//! arrays, Playwright `storageState`) and plain `name=value; …` header strings.
//! Only `instagram.com` cookies are kept. Cookie values never appear in `Debug`
//! output, logs or error messages.

use std::fmt;
use std::path::Path;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// Domain assumed for cookies that come without one (header-string format).
pub const DEFAULT_DOMAIN: &str = ".instagram.com";

/// `validate` warns when `sessionid` expires within this many seconds (7 days).
const EXPIRY_WARNING_SECS: i64 = 7 * 24 * 60 * 60;

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

    /// Number of cookies in the jar.
    pub fn len(&self) -> usize {
        self.cookies.len()
    }

    /// Whether the jar holds no cookies.
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
        self.apply_set_cookie_at(header, Utc::now().timestamp());
    }

    /// [`CookieJar::apply_set_cookie`] with an explicit current time (unix
    /// seconds), which `Max-Age` is added to and `Expires` is compared with.
    /// Headers for non-Instagram domains and malformed headers are ignored.
    pub fn apply_set_cookie_at(&mut self, header: &str, now: i64) {
        match parse_set_cookie(header, now) {
            Some(SetCookie::Store(cookie)) => self.insert(cookie),
            Some(SetCookie::Remove(name)) => self.remove(&name),
            None => {}
        }
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
    let text = strip_bom(text).trim_start();
    if text.starts_with('[') || text.starts_with('{') {
        return CookieFormat::Json;
    }
    for line in text.lines() {
        let trimmed = line.trim();
        if strip_prefix_ci(trimmed, "#HttpOnly_").is_some() {
            return CookieFormat::Netscape;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            let comment = comment.trim_start_matches('#').trim_start();
            if strip_prefix_ci(comment, "Netscape HTTP Cookie File").is_some()
                || strip_prefix_ci(comment, "HTTP Cookie File").is_some()
            {
                return CookieFormat::Netscape;
            }
            continue;
        }
        if line.split('\t').count() >= 6 {
            return CookieFormat::Netscape;
        }
    }
    CookieFormat::Header
}

/// Parses cookie file contents (any [`CookieFormat`]), keeping only
/// `instagram.com` cookies. Errors with [`crate::error::Error::Cookies`] when the
/// text is malformed or contains no Instagram cookies.
pub fn parse(text: &str) -> Result<CookieJar> {
    let text = strip_bom(text);
    if text.trim().is_empty() {
        return Err(Error::Cookies("the cookie file is empty".to_string()));
    }
    let all = match detect_format(text) {
        CookieFormat::Netscape => parse_netscape(text)?,
        CookieFormat::Json => parse_json(text)?,
        CookieFormat::Header => parse_header(text)?,
    };
    let total = all.len();
    let kept: Vec<Cookie> = all
        .into_iter()
        .filter(|cookie| is_instagram_domain(&cookie.domain))
        .collect();
    if kept.is_empty() {
        return Err(Error::Cookies(format!(
            "no instagram.com cookies found in the file ({total} cookies for other sites were ignored)"
        )));
    }
    Ok(CookieJar::new(kept))
}

/// Reads and parses a cookie file.
pub fn load(path: &Path) -> Result<CookieJar> {
    let bytes = std::fs::read(path)
        .map_err(|err| Error::Cookies(format!("cannot read {}: {err}", path.display())))?;
    let text = decode_text(&bytes).ok_or_else(|| {
        Error::Cookies(format!(
            "cannot read {}: the file is not UTF-8 or UTF-16 text",
            path.display()
        ))
    })?;
    parse(&text).map_err(|err| match err {
        Error::Cookies(message) => Error::Cookies(format!("{}: {message}", path.display())),
        other => other,
    })
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
    let session = jar
        .cookies()
        .iter()
        .find(|cookie| cookie.name == "sessionid" && !cookie.value.is_empty())
        .ok_or_else(|| {
            Error::Auth(
                "no sessionid cookie: export the cookies while logged in to instagram.com"
                    .to_string(),
            )
        })?;
    if let Some(expires) = session.expires
        && expires < now
    {
        return Err(Error::Auth(format!(
            "the sessionid cookie expired on {} (UTC): log in again and re-export the cookies",
            format_date(expires)
        )));
    }

    let mut warnings = Vec::new();
    if jar.get("csrftoken").is_none_or(str::is_empty) {
        warnings.push(
            "csrftoken is missing: Instagram may reject requests; re-export all instagram.com cookies"
                .to_string(),
        );
    }
    let user_id = jar
        .get("ds_user_id")
        .filter(|id| !id.is_empty())
        .map(str::to_string);
    if user_id.is_none() {
        warnings.push(
            "ds_user_id is missing: the logged-in account cannot be identified offline".to_string(),
        );
    }
    if let Some(expires) = session.expires
        && expires - now < EXPIRY_WARNING_SECS
    {
        let days = (expires - now) / (24 * 60 * 60);
        let when = match days {
            0 => "within a day".to_string(),
            1 => "in 1 day".to_string(),
            n => format!("in {n} days"),
        };
        warnings.push(format!(
            "sessionid expires on {} (UTC, {when}): log in again and re-export the cookies soon",
            format_date(expires)
        ));
    }

    Ok(CookieReport {
        names: jar.names().into_iter().map(str::to_string).collect(),
        session_expires: session.expires,
        user_id,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Helpers shared by the parsers.

/// Drops a leading UTF-8 byte order mark.
fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

/// `text` without `prefix`, compared ASCII case-insensitively.
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

/// `instagram.com` or any subdomain of it (leading dots ignored).
fn is_instagram_domain(domain: &str) -> bool {
    let domain = domain.trim().to_ascii_lowercase();
    let domain = domain.trim_start_matches('.');
    domain == "instagram.com" || domain.ends_with(".instagram.com")
}

/// `YYYY-MM-DD` (UTC) for a unix time, or the raw number when out of range.
fn format_date(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|at| at.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| unix.to_string())
}

/// Decodes a file's bytes: UTF-16 LE/BE when a UTF-16 BOM is present (Windows
/// PowerShell `>` writes UTF-16LE), otherwise UTF-8 with an optional BOM.
fn decode_text(bytes: &[u8]) -> Option<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return decode_utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return decode_utf16(rest, u16::from_be_bytes);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    String::from_utf8(rest.to_vec()).ok()
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let units = bytes.chunks_exact(2).map(|pair| unit([pair[0], pair[1]]));
    char::decode_utf16(units)
        .collect::<std::result::Result<String, _>>()
        .ok()
}

/// Parses a cookie date: RFC 2822 (`Wed, 21 Oct 2015 07:28:00 GMT`) or the
/// Netscape variant `Wed, 21-Oct-2015 07:28:00 GMT` (two-digit years too).
/// The weekday is not checked.
fn parse_http_date(text: &str) -> Option<i64> {
    let text = text.trim();
    if let Ok(at) = DateTime::parse_from_rfc2822(text) {
        return Some(at.timestamp());
    }
    let rest = text.split_once(',').map_or(text, |(_, rest)| rest).trim();
    let rest = rest
        .strip_suffix("GMT")
        .or_else(|| rest.strip_suffix("UTC"))
        .unwrap_or(rest)
        .trim();
    // Two-digit years first: `%Y` would also accept `24` as the year 24.
    [
        "%d-%b-%y %H:%M:%S",
        "%d %b %y %H:%M:%S",
        "%d %b %Y %H:%M:%S",
        "%d-%b-%Y %H:%M:%S",
    ]
    .iter()
    .find_map(|format| NaiveDateTime::parse_from_str(rest, format).ok())
    .map(|at| at.and_utc().timestamp())
}

// ---------------------------------------------------------------------------
// Netscape cookies.txt

fn parse_netscape(text: &str) -> Result<Vec<Cookie>> {
    let mut cookies = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim_end_matches('\r');
        let (line, http_only) = match strip_prefix_ci(line.trim_start(), "#HttpOnly_") {
            Some(rest) => (rest, true),
            None => (line, false),
        };
        let trimmed = line.trim();
        if trimmed.is_empty() || (!http_only && trimmed.starts_with('#')) {
            continue;
        }
        // Only the line number goes into the message: the line holds secrets.
        let cookie = parse_netscape_line(line, http_only)
            .map_err(|problem| Error::Cookies(format!("line {}: {problem}", index + 1)))?;
        cookies.push(cookie);
    }
    Ok(cookies)
}

fn parse_netscape_line(line: &str, http_only: bool) -> std::result::Result<Cookie, String> {
    let mut fields: Vec<&str> = line.splitn(7, '\t').map(str::trim).collect();
    if fields.len() < 6 && !line.contains('\t') {
        // Tabs turned into spaces by copy & paste; values never contain spaces.
        fields = line.split_whitespace().collect();
    }
    if !(6..=7).contains(&fields.len()) {
        return Err(format!(
            "expected 7 tab-separated fields (domain, include subdomains, path, secure, \
             expiry, name, value), found {}",
            fields.len()
        ));
    }
    let domain = fields[0];
    if domain.is_empty() {
        return Err("empty domain field".to_string());
    }
    parse_flag(fields[1]).ok_or("the include-subdomains field must be TRUE or FALSE")?;
    let secure = parse_flag(fields[3]).ok_or("the secure field must be TRUE or FALSE")?;
    let expires = parse_expiry_field(fields[4]).ok_or("invalid expiry time")?;
    let name = fields[5];
    if name.is_empty() {
        return Err("empty cookie name".to_string());
    }
    let path = if fields[2].is_empty() { "/" } else { fields[2] };
    Ok(Cookie {
        name: name.to_string(),
        value: fields.get(6).copied().unwrap_or_default().to_string(),
        domain: domain.to_string(),
        path: path.to_string(),
        expires,
        secure,
        http_only,
    })
}

/// `TRUE`/`FALSE` (any case, or `1`/`0`; empty counts as false).
fn parse_flag(field: &str) -> Option<bool> {
    if field.eq_ignore_ascii_case("true") || field == "1" {
        Some(true)
    } else if field.eq_ignore_ascii_case("false") || field == "0" || field.is_empty() {
        Some(false)
    } else {
        None
    }
}

/// Integer or float unix seconds; empty, `0` or negative is a session cookie.
/// `None` when the field is not a number.
fn parse_expiry_field(field: &str) -> Option<Option<i64>> {
    if field.is_empty() {
        return Some(None);
    }
    let secs = match field.parse::<i64>() {
        Ok(secs) => secs,
        Err(_) => {
            let secs = field.parse::<f64>().ok().filter(|secs| secs.is_finite())?;
            secs as i64
        }
    };
    Some((secs > 0).then_some(secs))
}

// ---------------------------------------------------------------------------
// JSON exports

fn parse_json(text: &str) -> Result<Vec<Cookie>> {
    // serde_json's messages can quote input; report the position only.
    let value: Value = serde_json::from_str(text).map_err(|err| {
        Error::Cookies(format!(
            "invalid JSON at line {} column {}",
            err.line(),
            err.column()
        ))
    })?;
    let items = match &value {
        Value::Array(items) => items,
        Value::Object(map) => match map.get("cookies") {
            Some(Value::Array(items)) => items,
            _ => return Err(json_shape_error()),
        },
        _ => return Err(json_shape_error()),
    };
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            json_cookie(item)
                .map_err(|problem| Error::Cookies(format!("JSON cookie #{}: {problem}", index + 1)))
        })
        .collect()
}

fn json_shape_error() -> Error {
    Error::Cookies(
        "JSON cookie files must be an array of cookie objects or an object with a \"cookies\" array"
            .to_string(),
    )
}

fn json_cookie(item: &Value) -> std::result::Result<Cookie, String> {
    let object = item.as_object().ok_or("not an object")?;
    let name = json_text(object.get("name")).ok_or("missing \"name\"")?;
    if name.is_empty() {
        return Err("empty \"name\"".to_string());
    }
    let value =
        json_text(object.get("value")).ok_or_else(|| format!("{name}: missing \"value\""))?;
    let domain = json_text(object.get("domain"))
        .filter(|domain| !domain.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DOMAIN.to_string());
    let path = json_text(object.get("path"))
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| "/".to_string());
    let http_only = ["httpOnly", "httponly", "http_only"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_bool))
        .unwrap_or(false);
    Ok(Cookie {
        expires: json_expiry(object),
        secure: object
            .get("secure")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        http_only,
        name,
        value,
        domain,
        path,
    })
}

/// A string (or number) field as text.
fn json_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// `expirationDate` | `expires` | `expiry` in (float) seconds or as a date
/// string; `-1`, `0` and `"session": true` mean a session cookie.
fn json_expiry(object: &Map<String, Value>) -> Option<i64> {
    if object.get("session").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let raw = ["expirationDate", "expires", "expiry"]
        .iter()
        .find_map(|key| object.get(*key).filter(|value| !value.is_null()))?;
    let secs = match raw {
        Value::Number(number) => number.as_f64()?,
        Value::String(text) => {
            let text = text.trim();
            match text.parse::<f64>() {
                Ok(secs) => secs,
                Err(_) => DateTime::parse_from_rfc3339(text)
                    .ok()
                    .map(|at| at.timestamp())
                    .or_else(|| parse_http_date(text))? as f64,
            }
        }
        _ => return None,
    };
    (secs.is_finite() && secs > 0.0).then_some(secs as i64)
}

// ---------------------------------------------------------------------------
// Header strings

fn parse_header(text: &str) -> Result<Vec<Cookie>> {
    let mut cookies = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = strip_prefix_ci(line, "cookie:").unwrap_or(line);
        for piece in line.split(';') {
            let piece = piece.trim();
            if piece.is_empty() {
                continue;
            }
            let (name, value) = piece.split_once('=').ok_or_else(unrecognized_format)?;
            let name = name.trim();
            if name.is_empty() {
                return Err(unrecognized_format());
            }
            cookies.push(Cookie::new(name, value.trim()));
        }
    }
    Ok(cookies)
}

fn unrecognized_format() -> Error {
    Error::Cookies(
        "unrecognized cookie file: expected a Netscape cookies.txt, a JSON export, \
         or a 'name=value; name2=value2' header string"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Set-Cookie

/// What one `Set-Cookie` header does to the jar.
enum SetCookie {
    Store(Cookie),
    Remove(String),
}

fn parse_set_cookie(header: &str, now: i64) -> Option<SetCookie> {
    let header = header.trim();
    let header = strip_prefix_ci(header, "set-cookie:").unwrap_or(header);
    let mut parts = header.split(';');
    let (name, value) = parts.next()?.split_once('=')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() {
        return None;
    }
    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: DEFAULT_DOMAIN.to_string(),
        path: "/".to_string(),
        expires: None,
        secure: false,
        http_only: false,
    };
    let mut max_age = None;
    let mut expires = None;
    for attribute in parts {
        let (key, val) = match attribute.split_once('=') {
            Some((key, val)) => (key.trim(), val.trim()),
            None => (attribute.trim(), ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "domain" if !val.is_empty() => {
                if !is_instagram_domain(val) {
                    return None;
                }
                cookie.domain = val.to_string();
            }
            "path" if !val.is_empty() => cookie.path = val.to_string(),
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "max-age" => max_age = val.parse::<i64>().ok().or(max_age),
            "expires" => expires = parse_http_date(val).or(expires),
            _ => {}
        }
    }
    if value.is_empty() || value == "\"\"" {
        return Some(SetCookie::Remove(cookie.name));
    }
    // Max-Age wins over Expires (RFC 6265 §5.3).
    cookie.expires = match (max_age, expires) {
        (Some(secs), _) if secs <= 0 => return Some(SetCookie::Remove(cookie.name)),
        (Some(secs), _) => Some(now.saturating_add(secs)),
        (None, Some(at)) if at <= now => return Some(SetCookie::Remove(cookie.name)),
        (None, at) => at,
    };
    Some(SetCookie::Store(cookie))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2025-10-09T08:53:20Z.
    const NOW: i64 = 1_760_000_000;
    const DAY: i64 = 24 * 60 * 60;
    const SECRET: &str = "FAKE-SECRET-1234";

    fn expect_cookies_error(result: Result<CookieJar>) -> String {
        match result {
            Err(Error::Cookies(message)) => message,
            other => panic!("expected a cookies error, got {other:?}"),
        }
    }

    fn cookie<'a>(jar: &'a CookieJar, name: &str) -> &'a Cookie {
        jar.cookies()
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("cookie {name} missing"))
    }

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

    // --- detect_format -----------------------------------------------------

    #[test]
    fn detects_json() {
        assert_eq!(detect_format("[{\"name\":\"a\"}]"), CookieFormat::Json);
        assert_eq!(detect_format("{\"cookies\":[]}"), CookieFormat::Json);
        assert_eq!(detect_format("\u{feff}\r\n  \t[ ]"), CookieFormat::Json);
    }

    #[test]
    fn detects_netscape() {
        assert_eq!(
            detect_format("# Netscape HTTP Cookie File\n"),
            CookieFormat::Netscape
        );
        assert_eq!(
            detect_format("\u{feff}# HTTP Cookie File\r\n"),
            CookieFormat::Netscape
        );
        assert_eq!(
            detect_format("# exported\n.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\n"),
            CookieFormat::Netscape
        );
        assert_eq!(
            detect_format("#HttpOnly_.instagram.com TRUE / TRUE 0 sessionid x\n"),
            CookieFormat::Netscape
        );
    }

    #[test]
    fn detects_header() {
        assert_eq!(
            detect_format("sessionid=a; csrftoken=b"),
            CookieFormat::Header
        );
        assert_eq!(detect_format("Cookie: sessionid=a"), CookieFormat::Header);
        assert_eq!(
            detect_format("# comment\nsessionid=a"),
            CookieFormat::Header
        );
    }

    // --- Netscape -----------------------------------------------------------

    #[test]
    fn parses_netscape_file() {
        let text = format!(
            "\u{feff}# Netscape HTTP Cookie File\r\n\
             # https://curl.se/docs/http-cookies.html\r\n\
             \r\n\
             .instagram.com\tTRUE\t/\tTRUE\t1790000000\tcsrftoken\tFAKECSRF\r\n\
             #HttpOnly_.instagram.com\tTRUE\t/\tTRUE\t1790000000.5\tsessionid\t{SECRET}\r\n\
             .instagram.com\tTRUE\t/\tTRUE\t0\tds_user_id\t1234567890\r\n\
             www.instagram.com\tFALSE\t/\tFALSE\t\tig_nrcb\r\n\
             .example.com\tTRUE\t/\tFALSE\t1790000000\tother\tvalue\r\n"
        );
        let jar = parse(&text).unwrap();
        assert_eq!(
            jar.names(),
            ["csrftoken", "sessionid", "ds_user_id", "ig_nrcb"]
        );

        let session = cookie(&jar, "sessionid");
        assert_eq!(session.value, SECRET);
        assert_eq!(session.domain, ".instagram.com");
        assert_eq!(session.expires, Some(1_790_000_000));
        assert!(session.http_only);
        assert!(session.secure);

        let csrf = cookie(&jar, "csrftoken");
        assert_eq!(csrf.value, "FAKECSRF");
        assert!(!csrf.http_only);
        assert_eq!(cookie(&jar, "ds_user_id").expires, None);

        let short = cookie(&jar, "ig_nrcb");
        assert_eq!(short.value, "");
        assert_eq!(short.domain, "www.instagram.com");
        assert_eq!(short.expires, None);
        assert!(!short.secure);
    }

    #[test]
    fn netscape_tolerates_spaces_when_tabs_were_lost() {
        let text =
            "# Netscape HTTP Cookie File\n.instagram.com TRUE / TRUE 1790000000 sessionid abc\n";
        let jar = parse(text).unwrap();
        assert_eq!(jar.get("sessionid"), Some("abc"));
        assert_eq!(cookie(&jar, "sessionid").expires, Some(1_790_000_000));
    }

    #[test]
    fn netscape_malformed_line_names_line_number_only() {
        let text = format!(
            "# Netscape HTTP Cookie File\n\
             .instagram.com\tTRUE\t/\tTRUE\t0\tcsrftoken\tok\n\
             .instagram.com\tTRUE\t{SECRET}\n"
        );
        let message = expect_cookies_error(parse(&text));
        assert!(message.contains("line 3"), "{message}");
        assert!(!message.contains(SECRET), "{message}");
    }

    #[test]
    fn netscape_bad_expiry_or_flag_is_an_error_without_value() {
        let text = format!(".instagram.com\tTRUE\t/\tTRUE\tsoon\tsessionid\t{SECRET}\n");
        let message = expect_cookies_error(parse(&text));
        assert!(
            message.contains("line 1") && message.contains("expiry"),
            "{message}"
        );
        assert!(!message.contains(SECRET));

        let text = format!("\n.instagram.com\tTRUE\t/\tMAYBE\t0\tsessionid\t{SECRET}\n");
        let message = expect_cookies_error(parse(&text));
        assert!(message.contains("line 2"), "{message}");
        assert!(!message.contains(SECRET));
    }

    // --- JSON -----------------------------------------------------------------

    #[test]
    fn parses_cookie_editor_array() {
        let text = format!(
            r#"[
              {{"domain": ".instagram.com", "expirationDate": 1790000000.123, "hostOnly": false,
                "httpOnly": true, "name": "sessionid", "path": "/", "secure": true,
                "session": false, "value": "{SECRET}"}},
              {{"domain": ".instagram.com", "name": "csrftoken", "value": "FAKECSRF",
                "expirationDate": 1790000000, "session": true}},
              {{"name": "ds_user_id", "value": 1234567890}},
              {{"domain": ".facebook.com", "name": "c_user", "value": "x"}}
            ]"#
        );
        let jar = parse(&text).unwrap();
        assert_eq!(jar.names(), ["sessionid", "csrftoken", "ds_user_id"]);
        let session = cookie(&jar, "sessionid");
        assert_eq!(session.value, SECRET);
        assert_eq!(session.expires, Some(1_790_000_000));
        assert!(session.http_only && session.secure);
        assert_eq!(cookie(&jar, "csrftoken").expires, None, "session: true");
        let user = cookie(&jar, "ds_user_id");
        assert_eq!(user.value, "1234567890");
        assert_eq!(user.domain, DEFAULT_DOMAIN);
        assert_eq!(user.path, "/");
    }

    #[test]
    fn parses_playwright_storage_state() {
        let text = r#"{
          "cookies": [
            {"name": "sessionid", "value": "abc", "domain": ".instagram.com", "path": "/",
             "expires": -1, "httpOnly": true, "secure": true, "sameSite": "Lax"},
            {"name": "mid", "value": "def", "domain": "www.instagram.com", "path": "/",
             "expires": 1790000000.75, "httpOnly": false, "secure": true, "sameSite": "None"},
            {"name": "rur", "value": "ghi", "domain": "instagram.com", "expiry": 0}
          ],
          "origins": [{"origin": "https://www.instagram.com", "localStorage": []}]
        }"#;
        let jar = parse(text).unwrap();
        assert_eq!(jar.names(), ["sessionid", "mid", "rur"]);
        assert_eq!(cookie(&jar, "sessionid").expires, None);
        assert_eq!(cookie(&jar, "mid").expires, Some(1_790_000_000));
        assert_eq!(cookie(&jar, "rur").expires, None);
    }

    #[test]
    fn json_errors_do_not_echo_values() {
        let text = format!(r#"[{{"name": "sessionid", "value": "{SECRET}" "#);
        let message = expect_cookies_error(parse(&text));
        assert!(
            message.starts_with("invalid JSON at line 1 column"),
            "{message}"
        );
        assert!(!message.contains(SECRET));

        let message = expect_cookies_error(parse(r#"[{"name": "sessionid"}]"#));
        assert!(
            message.contains("#1") && message.contains("missing \"value\""),
            "{message}"
        );

        let message = expect_cookies_error(parse(r#"{"other": []}"#));
        assert!(message.contains("\"cookies\" array"), "{message}");
        let message = expect_cookies_error(parse(r#"[{"value": "x"}]"#));
        assert!(message.contains("missing \"name\""), "{message}");
    }

    // --- Header strings --------------------------------------------------------

    #[test]
    fn parses_header_string() {
        let jar = parse("Cookie: sessionid=abc=def; csrftoken = tok ;; ds_user_id=42 ;\n").unwrap();
        assert_eq!(jar.names(), ["sessionid", "csrftoken", "ds_user_id"]);
        assert_eq!(jar.get("sessionid"), Some("abc=def"));
        assert_eq!(jar.get("csrftoken"), Some("tok"));
        assert_eq!(cookie(&jar, "ds_user_id").domain, DEFAULT_DOMAIN);
    }

    #[test]
    fn rejects_unrecognized_text() {
        let message = expect_cookies_error(parse("hello world, these are not cookies"));
        assert!(message.contains("unrecognized cookie file"), "{message}");
    }

    // --- Filtering ------------------------------------------------------------

    #[test]
    fn no_instagram_cookies_is_an_error() {
        let text = ".example.com\tTRUE\t/\tFALSE\t0\ta\t1\nfoo.org\tFALSE\t/\tFALSE\t0\tb\t2\n";
        let message = expect_cookies_error(parse(text));
        assert_eq!(
            message,
            "no instagram.com cookies found in the file (2 cookies for other sites were ignored)"
        );
        let message = expect_cookies_error(parse("[]"));
        assert!(message.contains("(0 cookies"), "{message}");
        let message = expect_cookies_error(parse(" \r\n"));
        assert!(message.contains("empty"), "{message}");
    }

    #[test]
    fn instagram_domain_matching() {
        for domain in [
            "instagram.com",
            ".instagram.com",
            "www.instagram.com",
            "I.Instagram.COM",
        ] {
            assert!(is_instagram_domain(domain), "{domain}");
        }
        for domain in [
            "notinstagram.com",
            "instagram.com.evil.net",
            "example.com",
            "",
        ] {
            assert!(!is_instagram_domain(domain), "{domain}");
        }
    }

    // --- load -------------------------------------------------------------------

    #[test]
    fn load_decodes_utf8_bom_and_utf16() {
        let dir = tempfile::tempdir().unwrap();
        let text =
            "# Netscape HTTP Cookie File\r\n.instagram.com\tTRUE\t/\tTRUE\t0\tsessionid\tabc\r\n";

        let utf8 = dir.path().join("utf8.txt");
        std::fs::write(&utf8, [&[0xEF, 0xBB, 0xBF][..], text.as_bytes()].concat()).unwrap();
        assert_eq!(load(&utf8).unwrap().get("sessionid"), Some("abc"));

        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let utf16le = dir.path().join("utf16le.txt");
        std::fs::write(&utf16le, le).unwrap();
        assert_eq!(load(&utf16le).unwrap().get("sessionid"), Some("abc"));

        let mut be = vec![0xFE, 0xFF];
        be.extend("sessionid=xyz".encode_utf16().flat_map(u16::to_be_bytes));
        let utf16be = dir.path().join("utf16be.txt");
        std::fs::write(&utf16be, be).unwrap();
        assert_eq!(load(&utf16be).unwrap().get("sessionid"), Some("xyz"));
    }

    #[test]
    fn load_errors_name_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let message = expect_cookies_error(load(&missing));
        assert!(message.starts_with("cannot read "), "{message}");
        assert!(message.contains("missing.txt"), "{message}");

        let binary = dir.path().join("binary.bin");
        std::fs::write(&binary, [0x80, 0x81, 0xFF]).unwrap();
        let message = expect_cookies_error(load(&binary));
        assert!(message.contains("not UTF-8 or UTF-16"), "{message}");

        let odd = dir.path().join("odd.txt");
        std::fs::write(&odd, [0xFF, 0xFE, 0x41]).unwrap();
        assert!(load(&odd).is_err());

        let other = dir.path().join("other.txt");
        std::fs::write(
            &other,
            r#"[{"name": "a", "value": "1", "domain": "example.com"}]"#,
        )
        .unwrap();
        let message = expect_cookies_error(load(&other));
        assert!(
            message.contains("other.txt: no instagram.com cookies"),
            "{message}"
        );
    }

    // --- validate ---------------------------------------------------------------

    fn jar_with_session(expires: Option<i64>) -> CookieJar {
        let mut session = Cookie::new("sessionid", SECRET);
        session.expires = expires;
        CookieJar::new(vec![
            session,
            Cookie::new("csrftoken", "FAKECSRF"),
            Cookie::new("ds_user_id", "1234567890"),
        ])
    }

    #[test]
    fn validate_healthy_jar() {
        let expires = NOW + 300 * DAY;
        let report = validate(&jar_with_session(Some(expires)), NOW).unwrap();
        assert_eq!(report.names, ["sessionid", "csrftoken", "ds_user_id"]);
        assert_eq!(report.session_expires, Some(expires));
        assert_eq!(report.user_id.as_deref(), Some("1234567890"));
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let report = validate(&jar_with_session(None), NOW).unwrap();
        assert_eq!(report.session_expires, None);
        assert!(report.warnings.is_empty());
    }

    #[test]
    fn validate_missing_sessionid() {
        let jar = CookieJar::from_pairs([("csrftoken", "x")]);
        match validate(&jar, NOW) {
            Err(Error::Auth(message)) => assert!(message.contains("no sessionid cookie")),
            other => panic!("unexpected {other:?}"),
        }
        let jar = CookieJar::from_pairs([("sessionid", "")]);
        assert!(matches!(validate(&jar, NOW), Err(Error::Auth(_))));
    }

    #[test]
    fn validate_expired_sessionid() {
        // 2024-05-01T00:00:00Z
        let jar = jar_with_session(Some(1_714_521_600));
        match validate(&jar, NOW) {
            Err(Error::Auth(message)) => {
                assert!(message.contains("2024-05-01"), "{message}");
                assert!(message.contains("log in again and re-export"), "{message}");
                assert!(!message.contains(SECRET));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn validate_warnings() {
        let mut session = Cookie::new("sessionid", SECRET);
        session.expires = Some(NOW + 3 * DAY + 60);
        let report = validate(&CookieJar::new(vec![session]), NOW).unwrap();
        assert_eq!(report.user_id, None);
        assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
        assert!(report.warnings[0].contains("csrftoken"));
        assert!(report.warnings[1].contains("ds_user_id"));
        assert!(
            report.warnings[2].contains("in 3 days"),
            "{}",
            report.warnings[2]
        );
        assert!(
            report.warnings[2].contains("2025-10-12"),
            "{}",
            report.warnings[2]
        );

        let report = validate(&jar_with_session(Some(NOW + 3600)), NOW).unwrap();
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("within a day"));

        let report = validate(&jar_with_session(Some(NOW + 8 * DAY)), NOW).unwrap();
        assert!(report.warnings.is_empty());
    }

    // --- Set-Cookie -------------------------------------------------------------

    #[test]
    fn set_cookie_updates_and_inserts() {
        let mut jar = jar_with_session(None);
        jar.apply_set_cookie_at(
            "csrftoken=NEWTOKEN; expires=Sat, 18-Oct-2036 00:00:00 GMT; Max-Age=31449600; \
             Path=/; Secure",
            NOW,
        );
        let csrf = cookie(&jar, "csrftoken");
        assert_eq!(csrf.value, "NEWTOKEN");
        assert_eq!(csrf.expires, Some(NOW + 31_449_600), "Max-Age wins");
        assert!(csrf.secure);
        assert_eq!(csrf.domain, DEFAULT_DOMAIN);

        jar.apply_set_cookie_at(
            "Set-Cookie: rur=\"CLN\\0541\\0541790000000:01ab\"; DOMAIN=.instagram.com; \
             httponly; PATH=/api; EXPIRES=Sat, 18 Oct 2036 00:00:00 GMT",
            NOW,
        );
        let rur = cookie(&jar, "rur");
        assert_eq!(rur.value, "\"CLN\\0541\\0541790000000:01ab\"");
        assert!(rur.http_only);
        assert!(!rur.secure);
        assert_eq!(rur.path, "/api");
        assert_eq!(rur.expires, Some(2_107_900_800));
        assert_eq!(jar.len(), 4);
    }

    #[test]
    fn set_cookie_deletes() {
        let mut jar = jar_with_session(None);
        jar.apply_set_cookie_at("csrftoken=gone; Max-Age=0; Path=/", NOW);
        assert_eq!(jar.get("csrftoken"), None);

        let mut jar = jar_with_session(None);
        jar.apply_set_cookie_at(
            "ds_user_id=x; expires=Thu, 01-Jan-1970 00:00:01 GMT; Path=/",
            NOW,
        );
        assert_eq!(jar.get("ds_user_id"), None);

        let mut jar = jar_with_session(None);
        jar.apply_set_cookie_at("sessionid=x; Expires=Wed, 01 May 2024 00:00:00 GMT", NOW);
        assert_eq!(jar.get("sessionid"), None);

        let mut jar = jar_with_session(None);
        jar.apply_set_cookie_at("csrftoken=\"\"; Path=/", NOW);
        jar.apply_set_cookie_at("ds_user_id=; Path=/", NOW);
        jar.apply_set_cookie_at("sessionid=x; Max-Age=-5", NOW);
        assert!(jar.is_empty(), "{jar:?}");
    }

    #[test]
    fn set_cookie_ignores_foreign_and_malformed_headers() {
        let original = jar_with_session(None);
        let mut jar = original.clone();
        jar.apply_set_cookie_at("csrftoken=EVIL; Domain=.facebook.com; Path=/", NOW);
        jar.apply_set_cookie_at("sessionid=x; Domain=instagram.com.evil.net; Max-Age=0", NOW);
        jar.apply_set_cookie_at("not a cookie", NOW);
        jar.apply_set_cookie_at("=value; Path=/", NOW);
        jar.apply_set_cookie_at("", NOW);
        assert_eq!(jar, original);

        jar.apply_set_cookie_at("mid=abc; Domain=www.instagram.com; Max-Age=nonsense", NOW);
        let mid = cookie(&jar, "mid");
        assert_eq!(mid.domain, "www.instagram.com");
        assert_eq!(mid.expires, None);
    }

    #[test]
    fn set_cookie_with_wall_clock() {
        let mut jar = jar_with_session(None);
        jar.apply_set_cookie("csrftoken=x; Max-Age=0");
        jar.apply_set_cookie("mid=abc; Max-Age=60");
        assert_eq!(jar.get("csrftoken"), None);
        assert!(cookie(&jar, "mid").expires.is_some_and(|at| at > NOW));
    }

    #[test]
    fn parses_cookie_dates() {
        assert_eq!(
            parse_http_date("Wed, 01 May 2024 00:00:00 GMT"),
            Some(1_714_521_600)
        );
        assert_eq!(
            parse_http_date("Wed, 01-May-2024 00:00:00 GMT"),
            Some(1_714_521_600)
        );
        assert_eq!(
            parse_http_date("Mon, 01-May-24 00:00:00 GMT"),
            Some(1_714_521_600),
            "two-digit year, wrong weekday"
        );
        assert_eq!(parse_http_date("tomorrow"), None);
    }
}
