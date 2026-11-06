use std::fmt;

/// Every failure the Google Cloud layer can report. `LoginRequired` is separated out so the
/// UI can offer a sign-in (`L`) and the CLI a hint, instead of a raw error.
///
/// No variant ever carries a token: texts come from Google's error bodies, the token
/// endpoint's `error` fields or gcloud's stderr, and pass through [`redact`] first.
#[derive(Debug, Clone)]
pub enum GcpError {
    /// No usable credentials: ADC missing or refused, and gcloud could not give a token.
    LoginRequired {
        detail: String,
    },
    /// Google answered with an error. `code` is its `error.status` (`PERMISSION_DENIED`,
    /// `NOT_FOUND`…), or `HTTP<n>` when the body has none.
    Api {
        status: u16,
        code: String,
        message: String,
    },
    Network(String),
}

pub type Result<T> = std::result::Result<T, GcpError>;

impl GcpError {
    pub fn is_login_required(&self) -> bool {
        matches!(self, GcpError::LoginRequired { .. })
    }

    /// Worth sending again when the request may run twice: server errors, throttling and
    /// lost connections.
    pub fn is_retryable(&self) -> bool {
        match self {
            GcpError::Api { status, .. } => *status >= 500 || self.is_throttling(),
            GcpError::Network(_) => true,
            _ => false,
        }
    }

    /// Google turned the request away before doing anything (quota, rate limit): sending it
    /// again is safe even when it has effects.
    pub fn is_throttling(&self) -> bool {
        matches!(self, GcpError::Api { status: 429, .. })
            || matches!(self, GcpError::Api { code, .. } if code == "RESOURCE_EXHAUSTED")
    }

    /// The access token was refused (expired or revoked early).
    pub fn is_unauthenticated(&self) -> bool {
        matches!(self, GcpError::Api { status: 401, .. })
            || matches!(self, GcpError::Api { code, .. } if code == "UNAUTHENTICATED")
    }

    /// The API is not enabled in the project: it holds nothing, as opposed to a read refused.
    pub fn is_api_off(&self) -> bool {
        matches!(self, GcpError::Api { status: 403, message, .. }
            if message.contains("has not been used in project") || message.contains("SERVICE_DISABLED"))
    }

    /// One-line text for status bars and toasts.
    pub fn short(&self) -> String {
        match self {
            GcpError::LoginRequired { .. } => "Google Cloud credentials expired — sign in again (L)".into(),
            GcpError::Api { code, message, .. } if message.is_empty() => code.clone(),
            GcpError::Api { code, message, .. } => format!("{code}: {message}"),
            GcpError::Network(m) => m.clone(),
        }
    }
}

impl fmt::Display for GcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GcpError::LoginRequired { detail } => {
                write!(f, "Google Cloud credentials are missing or expired ({detail})")
            }
            GcpError::Api { status, code, message } => write!(f, "{code} ({status}): {message}"),
            GcpError::Network(m) => write!(f, "network error: {m}"),
        }
    }
}

impl std::error::Error for GcpError {}

/// `text` made safe to show and keep: terminal sequences removed, credentials replaced
/// ([`redact_tokens`]), and cut to a line or two. Error texts end up in toasts, logs pasted into
/// chats and reports for an AI.
pub fn redact(text: &str) -> String {
    crate::util::ellipsize(redact_tokens(&crate::util::sanitize(text)).trim(), 300)
}

const REDACTED: &str = "[redacted]";

/// Where a credential's shape starts, and how many characters must follow to count. `true`: it
/// may start inside a word; the others are short enough to be part of an ordinary one.
const TOKEN_SHAPES: &[(&str, usize, bool)] = &[
    ("ya29.", 8, true),
    ("1//", 4, false),
    ("eyJ", 37, false),
    ("ghp_", 20, false),
    ("gho_", 20, false),
    ("ghu_", 20, false),
    ("ghs_", 20, false),
    ("ghr_", 20, false),
    ("github_pat_", 20, false),
    ("glpat-", 16, false),
    ("npm_", 20, false),
    ("AIza", 30, false),
    ("AKIA", 16, false),
    ("ASIA", 16, false),
    ("xoxb-", 10, false),
    ("xoxp-", 10, false),
    ("sk-", 20, false),
];

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '+' | '=' | '~')
}

/// A name that says its value is a credential: `password`, `MCP_API_TOKEN`, `client_secret`.
/// Ends with, so `tokens=5` and `token_count=5` stay.
fn names_a_secret(name: &str) -> bool {
    [
        "password",
        "passwd",
        "pwd",
        "secret",
        "token",
        "apikey",
        "api_key",
        "api-key",
        "credential",
        "credentials",
        "private_key",
    ]
    .iter()
    .any(|n| name.ends_with(n))
}

/// A name whose whole value, to the end of the string or the line, is a credential.
fn names_a_header(name: &str) -> bool {
    matches!(name, "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
}

/// How long the credential at the start of `rest` is, when one is.
fn token_at(rest: &str, before: Option<char>) -> Option<usize> {
    TOKEN_SHAPES.iter().find_map(|&(prefix, min, anywhere)| {
        let tail = rest.strip_prefix(prefix)?;
        if !anywhere && before.is_some_and(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        let len = tail.find(|c| !is_token_char(c)).unwrap_or(tail.len());
        (len >= min).then_some(prefix.len() + len)
    })
}

/// The name that ends at the end of `before`, lower-cased, past a closing quote.
fn name_before(before: &str) -> String {
    let before = before.strip_suffix(['"', '\'']).unwrap_or(before);
    let rest = before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    before[rest.len()..].to_ascii_lowercase()
}

/// `text` with anything that looks like a credential replaced, wherever it sits (`token=ya29.…`,
/// `"password":"…"`, `postgres://user:pass@host`): tokens by their shape, values by the name
/// before them, and the user and password of a URL. A best effort over text that may hold any
/// format, so reports and agents get log lines without what is plainly a secret; it is not a
/// guarantee that none is left.
pub fn redact_tokens(text: &str) -> String {
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY") {
        return REDACTED.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while let Some(c) = text[i..].chars().next() {
        let rest = &text[i..];
        let before = text[..i].chars().next_back();
        if let Some(len) = token_at(rest, before) {
            out.push_str(REDACTED);
            i += len;
            continue;
        }
        // `Bearer abc…`, as a header or inside any sentence.
        if c.eq_ignore_ascii_case(&'b')
            && !before.is_some_and(|b| b.is_ascii_alphanumeric())
            && rest.get(..7).is_some_and(|w| w.eq_ignore_ascii_case("bearer "))
        {
            let value = &rest[7..];
            let len = value.find(|c| !is_token_char(c)).unwrap_or(value.len());
            if len >= 8 {
                out.push_str(&rest[..7]);
                out.push_str(REDACTED);
                i += 7 + len;
                continue;
            }
        }
        // The user and password of a URL.
        if let Some(authority) = rest.strip_prefix("://") {
            let end = authority
                .find(|c: char| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '"' | '\''))
                .unwrap_or(authority.len());
            if let Some(at) = authority[..end].rfind('@') {
                out.push_str("://");
                out.push_str(REDACTED);
                i += 3 + at;
                continue;
            }
        }
        // `name=value`, `name: value`, `"name":"value"`.
        if c == '=' || c == ':' {
            let name = name_before(&text[..i]);
            let header = names_a_header(&name);
            if header || names_a_secret(&name) {
                let after = &rest[1..];
                let spaces = after.len() - after.trim_start_matches(' ').len();
                let value = &after[spaces..];
                let quote = value.chars().next().filter(|q| matches!(q, '"' | '\''));
                let value = value.get(quote.map_or(0, char::len_utf8)..).unwrap_or("");
                let ends = |v: char| match quote {
                    Some(q) => v == q || v == '\n',
                    None if header => v == '\n',
                    None => v.is_whitespace() || matches!(v, '&' | ',' | ';' | ')' | '}' | ']' | '"' | '\''),
                };
                let len = value.find(ends).unwrap_or(value.len());
                // After a colon, a short bare word is prose (`token: expired`), not a value.
                let enough = if c == ':' && quote.is_none() && !header { len >= 8 } else { len >= 1 };
                if enough {
                    let head = 1 + spaces + quote.map_or(0, char::len_utf8);
                    out.push_str(&rest[..head]);
                    out.push_str(REDACTED);
                    i += head + len;
                    continue;
                }
            }
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Turns an error response into a `GcpError::Api`. Google APIs answer
/// `{"error": {"code": 403, "status": "PERMISSION_DENIED", "message": "…"}}`; the OAuth token
/// endpoint answers `{"error": "invalid_grant", "error_description": "…"}`.
pub fn api_error(status: u16, body: &[u8]) -> GcpError {
    let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
    let error = &json["error"];
    let (code, message) = match error {
        serde_json::Value::Object(e) => (
            e.get("status").and_then(|v| v.as_str()).map(str::to_string),
            e.get("message").and_then(|v| v.as_str()).map(str::to_string),
        ),
        serde_json::Value::String(code) => (Some(code.clone()), json["error_description"].as_str().map(str::to_string)),
        _ => (None, None),
    };
    let code = code.filter(|c| !c.is_empty()).unwrap_or_else(|| format!("HTTP{status}"));
    let message = message.unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    GcpError::Api { status, code: redact(&code), message: redact(&message) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_go_wherever_they_sit() {
        for (text, want) in [
            ("token=ya29.a0AfH6SMBxyz", "token=[redacted]"),
            ("got ya29.a0AfH6SMBxyz back", "got [redacted] back"),
            ("x=ya29.a0AfH6SMBxyz&y=1", "x=[redacted]&y=1"),
            (r#"{"access_token":"ya29.a0AfH6SMBxyz","n":1}"#, r#"{"access_token":"[redacted]","n":1}"#),
            ("Authorization: Bearer abc.def.ghi", "Authorization: [redacted]"),
            ("sent bearer abcdefghijkl now", "sent bearer [redacted] now"),
            ("postgres://app:s3cret@10.0.0.1:5432/db?x=1", "postgres://[redacted]@10.0.0.1:5432/db?x=1"),
            ("MCP_API_TOKEN=abc123 next", "MCP_API_TOKEN=[redacted] next"),
            ("password: hunter22hunter", "password: [redacted]"),
            (r#"{"password": "a b c"}"#, r#"{"password": "[redacted]"}"#),
            ("Cookie: a=1; b=2", "Cookie: [redacted]"),
            ("key ghp_abcdefghijklmnopqrst1234", "key [redacted]"),
            ("-----BEGIN PRIVATE KEY----- abc", "[redacted]"),
        ] {
            assert_eq!(redact_tokens(text), want, "{text}");
        }
    }

    #[test]
    fn ordinary_text_stays() {
        for text in [
            "token: expired",
            "max_tokens=100",
            "token_count=5",
            "tokens=5",
            "https://example.com/a?b=c",
            "the bearer of bad news",
            "ñandú: ok = sí",
            "ssh git@example.com",
            "no-sk-here risk-assessment-for-the-whole-year",
        ] {
            assert_eq!(redact_tokens(text), text);
        }
    }

    #[test]
    fn google_error_bodies_are_mapped() {
        let body =
            br#"{"error":{"code":403,"message":"Permission 'run.services.list' denied","status":"PERMISSION_DENIED"}}"#;
        let e = api_error(403, body);
        assert_eq!(e.short(), "PERMISSION_DENIED: Permission 'run.services.list' denied");
        assert!(!e.is_retryable() && !e.is_login_required());

        let e =
            api_error(400, br#"{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)"}"#);
        assert!(matches!(&e, GcpError::Api { code, .. } if code == "invalid_grant"), "{e:?}");
        assert_eq!(e.short(), "invalid_grant: reauth related error (invalid_rapt)");

        let e = api_error(503, b"<html>Service Unavailable</html>");
        assert_eq!(e.short(), "HTTP503: <html>Service Unavailable</html>");
        assert!(e.is_retryable());
        assert!(api_error(429, b"{}").is_throttling());
        let e = api_error(429, br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota exceeded"}}"#);
        assert!(e.is_throttling() && e.is_retryable());
        assert!(
            api_error(401, br#"{"error":{"code":401,"status":"UNAUTHENTICATED","message":"x"}}"#).is_unauthenticated()
        );
    }

    #[test]
    fn login_required_says_how_to_sign_in() {
        let e = GcpError::LoginRequired { detail: "Reauthentication failed".into() };
        assert!(e.is_login_required() && !e.is_retryable());
        assert!(e.short().contains("sign in again (L)"));
        assert!(e.to_string().contains("Reauthentication failed"));
    }

    #[test]
    fn tokens_and_escapes_never_reach_an_error() {
        let e = api_error(
            400,
            "{\"error\":{\"status\":\"BAD\\u001b[2J\",\"message\":\"token ya29.a0AfB_secret-value, refresh 1//0gSecret\"}}".as_bytes(),
        );
        let text = e.to_string();
        assert!(!text.contains("ya29.") && !text.contains("1//0g") && !text.contains('\u{1b}'), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
    }
}
