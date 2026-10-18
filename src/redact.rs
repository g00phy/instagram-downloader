//! Scrubbing secrets from strings before they are displayed, logged or stored.

use crate::cookies::CookieJar;

/// Shortens a URL for display: keeps scheme, host and path, replaces the query
/// (signed CDN parameters such as `oh`, `oe`, `_nc_sid`) with `?…`.
/// Strings that are not URLs are returned unchanged.
pub fn redact_url(url: &str) -> String {
    let _ = url;
    todo!("cookies agent")
}

/// Scrubs free text (error bodies, log lines): `Cookie:`/`Authorization:` header
/// values, `sessionid=…`/`csrftoken=…`-style pairs, bearer tokens, and the query
/// part of any URL in the text.
pub fn redact(text: &str) -> String {
    let _ = text;
    todo!("cookies agent")
}

/// [`redact`] plus replacing every value present in `jar` with `***`
/// (defense in depth for server responses that echo cookies).
pub fn redact_with(text: &str, jar: &CookieJar) -> String {
    let _ = (text, jar);
    todo!("cookies agent")
}
