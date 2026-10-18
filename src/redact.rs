//! Scrubbing secrets from strings before they are displayed, logged or stored.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use url::Url;

use crate::cookies::CookieJar;

/// What every secret is replaced with.
const MASK: &str = "***";

/// Jar values shorter than this are not replaced by [`redact_with`] (too likely
/// to match unrelated text).
const MIN_SECRET_LEN: usize = 4;

/// Compiles one of the built-in patterns below.
fn compile(pattern: &str) -> Regex {
    // Infallible in practice: the patterns are constants exercised by the unit
    // tests, so a bad one fails `cargo test`, never a user's run.
    Regex::new(pattern).expect("built-in redaction regex must compile")
}

/// `http(s)://…` up to whitespace or a closing delimiter, also in the
/// JSON-escaped form `https:\/\/…`.
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"(?i)\bhttps?:(?://|\\/\\/)[^\s"'<>)\]}]+"#));

/// `Cookie:` / `Set-Cookie:` / `Authorization:` header and its value up to the
/// end of the line.
static HEADER_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r"(?i)\b(set-cookie|cookie|proxy-authorization|authorization)([ \t]*:[ \t]*)(\S[^\r\n]*)",
    )
});

/// `Bearer <token>`.
static BEARER_RE: LazyLock<Regex> =
    LazyLock::new(|| compile(r"(?i)\b(bearer)([ \t]+)[A-Za-z0-9\-._~+/]+=*"));

/// `key=value`, `key: value`, `"key":"value"`, `"key": value` (also with
/// JSON-escaped quotes) for the sensitive keys.
static PAIR_RE: LazyLock<Regex> = LazyLock::new(|| {
    compile(concat!(
        r"(?i)\b(sessionid|csrftoken|csrf_token|ds_user_id|rur|mid|ig_did|shbid|shbts|datr|",
        r"ig_nrcb|password|passwd|access_token|token|authorization|auth|cookie)\b",
        r#"(\\?["']?)(\s*[:=]\s*)"#,
        r#"(\\"(?:[^"\\]|\\[^"])*\\"|"(?:[^"\\]|\\.)*"|'[^']*'|[^\s;&,"'<>(){}\[\]]+)"#,
    ))
});

/// Shortens a URL for display: keeps scheme, host and path, replaces the query
/// (signed CDN parameters such as `oh`, `oe`, `_nc_sid`) with `?…`.
/// Strings that are not URLs are returned unchanged.
pub fn redact_url(url: &str) -> String {
    let Ok(parsed) = Url::parse(url) else {
        return url.to_string();
    };
    let Some(host) = parsed.host_str() else {
        return url.to_string();
    };
    if !matches!(parsed.scheme(), "http" | "https") {
        return url.to_string();
    }
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    if !has_userinfo && parsed.query().is_none() && parsed.fragment().is_none() {
        return url.to_string();
    }
    let mut out = format!("{}://{host}", parsed.scheme());
    if let Some(port) = parsed.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    out.push_str(parsed.path());
    if parsed.query().is_some_and(|query| !query.is_empty()) {
        out.push_str("?…");
    }
    out
}

/// Scrubs free text (error bodies, log lines): `Cookie:`/`Authorization:` header
/// values, `sessionid=…`/`csrftoken=…`-style pairs, bearer tokens, and the query
/// part of any URL in the text.
pub fn redact(text: &str) -> String {
    let text = URL_RE.replace_all(text, |caps: &Captures| redact_url_match(&caps[0]));
    let text = HEADER_RE.replace_all(&text, redact_header);
    let text = BEARER_RE.replace_all(&text, "${1}${2}***");
    let text = PAIR_RE.replace_all(&text, redact_pair);
    text.into_owned()
}

/// [`redact`] plus replacing every value present in `jar` with `***`
/// (defense in depth for server responses that echo cookies).
pub fn redact_with(text: &str, jar: &CookieJar) -> String {
    let mut secrets: Vec<String> = Vec::new();
    for cookie in jar.cookies() {
        let value = cookie.value.as_str();
        let unquoted = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        secrets.push(value.to_string());
        secrets.push(unquoted.to_string());
        secrets.extend(percent_decode(unquoted));
    }
    secrets.retain(|secret| secret.chars().count() >= MIN_SECRET_LEN);
    // Longest first, so a value that contains another one is replaced whole.
    secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    secrets.dedup();

    let mut out = redact(text);
    for secret in &secrets {
        if out.contains(secret.as_str()) {
            out = out.replace(secret.as_str(), MASK);
        }
    }
    out
}

/// One URL found by [`URL_RE`]: trailing sentence punctuation is kept outside,
/// JSON-escaped slashes are handled.
fn redact_url_match(found: &str) -> String {
    let url = found.trim_end_matches(['.', ',', ';', ':', '!', '?', '\\']);
    let tail = &found[url.len()..];
    let redacted = if url.contains("\\/") {
        let plain = url.replace("\\/", "/");
        let redacted = redact_url(&plain);
        if redacted == plain {
            url.to_string()
        } else {
            redacted.replace('/', "\\/")
        }
    } else {
        redact_url(url)
    };
    format!("{redacted}{tail}")
}

fn redact_header(caps: &Captures) -> String {
    let (name, separator, value) = (&caps[1], &caps[2], &caps[3]);
    // A cookie header always carries `name=value`; without one this is prose
    // such as "no sessionid cookie: export the cookies …".
    if name.to_ascii_lowercase().ends_with("cookie") && !value.contains('=') {
        return caps[0].to_string();
    }
    format!("{name}{separator}{MASK}")
}

fn redact_pair(caps: &Captures) -> String {
    let (key, quote, separator, value) = (&caps[1], &caps[2], &caps[3], &caps[4]);
    // Unquoted `Cookie: …` / `Authorization: …` is the header form, which
    // `redact_header` already handled (or deliberately left alone as prose).
    let header_form = quote.is_empty()
        && separator.contains(':')
        && (key.eq_ignore_ascii_case("cookie") || key.eq_ignore_ascii_case("authorization"));
    if header_form {
        return caps[0].to_string();
    }
    let masked = if value.starts_with("\\\"") {
        "\\\"***\\\""
    } else if value.starts_with('"') {
        "\"***\""
    } else if value.starts_with('\'') {
        "'***'"
    } else {
        MASK
    };
    format!("{key}{quote}{separator}{masked}")
}

/// Decodes `%XX` escapes; `None` when there are none or the result is not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    if !text.contains('%') {
        return None;
    }
    let mut out = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let Some((&byte, tail)) = rest.split_first() {
        let escaped = if byte == b'%' {
            tail.get(..2)
                .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        } else {
            None
        };
        match escaped {
            Some(decoded) => {
                out.push(decoded);
                rest = tail.get(2..).unwrap_or_default();
            }
            None => {
                out.push(byte);
                rest = tail;
            }
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cookies::Cookie;

    #[test]
    fn redact_url_strips_query_fragment_and_userinfo() {
        assert_eq!(
            redact_url(
                "https://scontent.cdninstagram.com/v/t51/123_n.jpg?stp=dst&_nc_sid=abc&oh=00_FAKE&oe=6700AAAA"
            ),
            "https://scontent.cdninstagram.com/v/t51/123_n.jpg?…"
        );
        assert_eq!(
            redact_url("http://user:pass@example.com:8080/a/b?x=1#frag"),
            "http://example.com:8080/a/b?…"
        );
        assert_eq!(
            redact_url("https://www.instagram.com/p/ABC/#comments"),
            "https://www.instagram.com/p/ABC/"
        );
        assert_eq!(
            redact_url("https://www.instagram.com/p/ABC/?"),
            "https://www.instagram.com/p/ABC/"
        );
        assert_eq!(
            redact_url("http://127.0.0.1:4567/api/v1/x?y=2"),
            "http://127.0.0.1:4567/api/v1/x?…"
        );
    }

    #[test]
    fn redact_url_leaves_other_strings_alone() {
        for text in [
            "https://www.instagram.com/natgeo/",
            "https://example.com",
            "not a url",
            "/relative/path?x=1",
            "ftp://example.com/file?x=1",
            "mailto:someone@example.com",
            "",
        ] {
            assert_eq!(redact_url(text), text);
        }
    }

    #[test]
    fn redacts_urls_inside_text() {
        let text = "GET https://cdn.example.com/a.mp4?oh=SIG&oe=1 failed (see https://x.test/b?c=d). \
                    Also \"https://x.test/q?z=1\" and <https://x.test/w?v=2>, https://x.test/plain.";
        assert_eq!(
            redact(text),
            "GET https://cdn.example.com/a.mp4?… failed (see https://x.test/b?…). \
             Also \"https://x.test/q?…\" and <https://x.test/w?…>, https://x.test/plain."
        );
    }

    #[test]
    fn redacts_json_escaped_urls() {
        let text = r#"{"url":"https:\/\/cdn.example.com\/v\/a.jpg?oh=SIG&oe=1","next":"https:\/\/x.test\/p\/"}"#;
        assert_eq!(
            redact(text),
            r#"{"url":"https:\/\/cdn.example.com\/v\/a.jpg?…","next":"https:\/\/x.test\/p\/"}"#
        );
    }

    #[test]
    fn redacts_sensitive_json_fields() {
        let text = r#"{"sessionid":"FAKE%3Asess","csrftoken": "tok-123", "ds_user_id": 12345, "status":"ok"}"#;
        assert_eq!(
            redact(text),
            r#"{"sessionid":"***","csrftoken": "***", "ds_user_id": ***, "status":"ok"}"#
        );
        let pretty = "{\n  \"password\":\n    \"hunter2\",\n  \"user\": \"me\"\n}";
        assert_eq!(
            redact(pretty),
            "{\n  \"password\":\n    \"***\",\n  \"user\": \"me\"\n}"
        );
        let escaped = r#"{"body":"{\"sessionid\":\"FAKESESS\",\"ok\":true}"}"#;
        assert_eq!(
            redact(escaped),
            r#"{"body":"{\"sessionid\":\"***\",\"ok\":true}"}"#
        );
        assert_eq!(redact("{'access_token': 'abc'}"), "{'access_token': '***'}");
    }

    #[test]
    fn redacts_header_strings_and_pairs() {
        assert_eq!(
            redact("sessionid=FAKE%3Aabc; csrftoken=tok; ds_user_id=42; theme=dark"),
            "sessionid=***; csrftoken=***; ds_user_id=***; theme=dark"
        );
        assert_eq!(
            redact("rur=\"CLN\\0541\\0541790000000:01ab\"; mid=Zabc; ig_did=X-Y"),
            "rur=\"***\"; mid=***; ig_did=***"
        );
        assert_eq!(redact("TOKEN=abc&x=1"), "TOKEN=***&x=1");
        assert_eq!(redact("password: hunter2 next"), "password: *** next");
        assert_eq!(redact("SessionId = abc,rest"), "SessionId = ***,rest");
    }

    #[test]
    fn redacts_header_lines() {
        let text = "GET /api/v1/x HTTP/1.1\r\nCookie: sessionid=FAKE; csrftoken=tok\r\n\
                    authorization: Basic dXNlcjpwYXNz\r\nset-cookie: rur=abc; Path=/\r\nAccept: */*\r\n";
        assert_eq!(
            redact(text),
            "GET /api/v1/x HTTP/1.1\r\nCookie: ***\r\nauthorization: ***\r\n\
             set-cookie: ***\r\nAccept: */*\r\n"
        );
        assert_eq!(redact("> Cookie: a=b; sessionid=c"), "> Cookie: ***");
    }

    #[test]
    fn redacts_bearer_tokens() {
        assert_eq!(
            redact("sent bearer abc.DEF-123_x/y+z== to the API"),
            "sent bearer *** to the API"
        );
        assert_eq!(
            redact(r#"{"Authorization": "Bearer abc.def"}"#),
            r#"{"Authorization": "***"}"#
        );
    }

    #[test]
    fn leaves_text_without_secrets_unchanged() {
        for text in [
            "downloaded 3 files to D:\\insta\\natgeo (2 skipped)",
            "authentication failed: no sessionid cookie: export the cookies while logged in to instagram.com",
            "cookies: line 3: expected 7 tab-separated fields",
            "csrftoken is missing: Instagram may reject requests; re-export all instagram.com cookies",
            "midnight: tokens=5, authentication: ok, cookies: fine",
            "https://www.instagram.com/p/C0ffeeXYZ12/ is a post",
            "",
        ] {
            assert_eq!(redact(text), text);
        }
    }

    #[test]
    fn redact_is_idempotent() {
        let text =
            "Cookie: sessionid=a\nhttps://x.test/?a=1 sessionid=abc \"csrftoken\":\"x\" Bearer tok";
        let once = redact(text);
        assert_eq!(redact(&once), once);
        assert!(!once.contains("abc") && !once.contains("Bearer tok"));
    }

    #[test]
    fn redact_with_replaces_jar_values() {
        let mut rur = Cookie::new("rur", "\"CLN\\0541\\054ABCD\"");
        rur.domain = ".instagram.com".to_string();
        let jar = CookieJar::new(vec![
            Cookie::new("sessionid", "1234567890%3AFAKESESSION%3A12"),
            Cookie::new("csrftoken", "FAKECSRFTOKEN"),
            Cookie::new("ig_nrcb", "1"),
            rur,
        ]);
        let text = "echo: 1234567890:FAKESESSION:12 then FAKECSRFTOKEN, \
                    1234567890%3AFAKESESSION%3A12, CLN\\0541\\054ABCD and 1 item";
        assert_eq!(
            redact_with(text, &jar),
            "echo: *** then ***, ***, *** and 1 item"
        );
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("a%3Ab%2fc").as_deref(), Some("a:b/c"));
        assert_eq!(percent_decode("100%").as_deref(), Some("100%"));
        assert_eq!(percent_decode("%zz%4").as_deref(), Some("%zz%4"));
        assert_eq!(percent_decode("plain"), None);
        assert_eq!(percent_decode("%FF"), None);
    }
}
