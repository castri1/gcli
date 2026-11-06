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

/// A name that says its value is a credential, in any spelling (`password`, `MCP_API_TOKEN`,
/// `client-secret`, `secretKey`). By how it ends, so `tokens=5` and `token_count=5` stay.
fn names_a_secret(name: &str) -> bool {
    const ENDS: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "pwd",
        "secret",
        "secretkey",
        "accesskey",
        "token",
        "apikey",
        "credential",
        "credentials",
        "privatekey",
    ];
    // `pass` alone only as its own word: `bypass` and `compass` are not secrets.
    let pass = name.eq_ignore_ascii_case("pass")
        || ["_pass", "-pass", "_PASS", "-PASS", "Pass"].iter().any(|e| name.ends_with(e));
    let flat: String = name.chars().filter(|c| !matches!(c, '_' | '-')).collect::<String>().to_ascii_lowercase();
    pass || ENDS.iter().any(|e| flat.ends_with(e))
}

/// A name whose whole value, to the end of the string or the line, is a credential.
fn names_a_header(name: &str) -> bool {
    matches!(name.to_ascii_lowercase().as_str(), "authorization" | "proxy-authorization" | "cookie" | "set-cookie")
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

fn is_base64_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')
}

/// Short segments with slashes close together, and no `+` or `=`. Random base64 holds a `/` only
/// now and then, so few runs of it pass; those that do are a best effort's known limit.
fn looks_like_path(run: &str) -> bool {
    let slashes = run.matches('/').count();
    slashes >= 3
        && run.len() / slashes <= 16
        && !run.contains(['+', '='])
        && run.split('/').all(|segment| segment.len() <= 32)
}

/// Where the last `word` stands alone in `line`: `END`, not the end of `APPEND`.
fn last_word(line: &str, word: &str) -> Option<usize> {
    line.rmatch_indices(word).map(|(i, _)| i).find(|&i| !line[..i].ends_with(|c: char| c.is_ascii_alphabetic()))
}

/// How long the run of base64 at the start of `rest` is, when it reads like a key's body or a
/// random secret: long, of all three kinds of character (so a commit id or a digest, all hex in
/// one case, stays), and not a path or the first part of a name (`src/SomeLongName2.ts`).
fn key_run_at(rest: &str) -> Option<usize> {
    let len = rest.find(|c| !is_base64_char(c)).unwrap_or(rest.len());
    let run = &rest[..len];
    let goes_on = rest[len..].starts_with(['.', '-', '_']);
    (len >= 40
        && !goes_on
        && !looks_like_path(run)
        && run.contains(|c: char| c.is_ascii_uppercase())
        && run.contains(|c: char| c.is_ascii_lowercase())
        && run.contains(|c: char| c.is_ascii_digit()))
    .then_some(len)
}

fn is_key_marker(line: &str) -> bool {
    line.contains("PRIVATE KEY") && line.contains("-----")
}

/// For each line of a log read line by line, whether it sits in a private key's block: from its
/// BEGIN line to its END line, or to the end of the log. A key's last lines can be short enough
/// to look like nothing. Each line comes with its source (a build's step): sources running at the
/// same time mix their lines, and one must not close another's block.
pub fn in_key_block<'a, S: Eq + std::hash::Hash>(lines: impl Iterator<Item = (S, &'a str)>) -> Vec<bool> {
    let mut inside = std::collections::HashSet::new();
    lines
        .map(|(source, line)| {
            let marker = is_key_marker(line);
            let begin = last_word(line, "BEGIN").filter(|_| marker);
            let end = last_word(line, "END").filter(|_| marker);
            match (begin, end) {
                (Some(b), Some(e)) if e > b => {
                    inside.remove(&source);
                    true
                }
                (Some(_), _) => {
                    inside.insert(source);
                    true
                }
                (None, Some(_)) => {
                    inside.remove(&source);
                    true
                }
                (None, None) => inside.contains(&source),
            }
        })
        .collect()
}

/// Where the quoted text starting at `body` ends: at the first `quote` no backslash escapes.
fn closing_quote(body: &str, quote: char) -> usize {
    let mut escaped = false;
    for (i, c) in body.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '\n' => return i,
            c if c == quote => return i,
            _ => {}
        }
    }
    body.len()
}

/// The name that ends at the end of `before`, lower-cased, past spaces and quotes, escaped ones
/// included (`\"password\" : `, as JSON inside a log's string has it).
fn name_before(before: &str) -> String {
    let before = before.trim_end_matches([' ', '"', '\'', '\\']);
    let rest = before.trim_end_matches(|c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    before[rest.len()..].to_string()
}

/// `text` with anything that looks like a credential replaced, wherever it sits (`token=ya29.…`,
/// `"password":"…"`, `postgres://user:pass@host`): tokens by their shape, values by the name
/// before them, and the user and password of a URL. A best effort over text that may hold any
/// format, so reports and agents get log lines without what is plainly a secret; it is not a
/// guarantee that none is left.
pub fn redact_tokens(text: &str) -> String {
    if is_key_marker(text) {
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
        if !before.is_some_and(is_base64_char)
            && let Some(len) = key_run_at(rest)
        {
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
        // The user and password of a URL: up to the last `@` when what comes before it reads
        // `user:pass` (a password can hold a `/` or an `@`), or else an `@` in the host part. A
        // quote ends the URL only when the URL itself began inside one.
        if let Some(url) = rest.strip_prefix("://") {
            let scheme =
                text[..i].trim_end_matches(|c: char| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
            let quoted = scheme.ends_with(['"', '\'']);
            let word =
                url.find(|c: char| c.is_whitespace() || (quoted && matches!(c, '"' | '\''))).unwrap_or(url.len());
            let host = url[..word].find(['/', '?', '#']).unwrap_or(word);
            let at = url[..word].rfind('@').filter(|&at| url[..at].contains(':')).or_else(|| url[..host].rfind('@'));
            if let Some(at) = at {
                out.push_str("://");
                out.push_str(REDACTED);
                i += 3 + at;
                continue;
            }
        }
        // `name=value`, `name: value`, `"name":"value"`, `\"name\":\"value\"`.
        if c == '=' || c == ':' {
            let name = name_before(&text[..i]);
            let header = names_a_header(&name);
            if header || names_a_secret(&name) {
                let after = &rest[1..];
                let spaces = after.len() - after.trim_start_matches(' ').len();
                let value = &after[spaces..];
                let opening = ["\\\"", "\\'", "\"", "'"].into_iter().find(|o| value.starts_with(o));
                let body = &value[opening.map_or(0, str::len)..];
                let len = match opening {
                    // Inside a JSON string: it closes with the same escaped quote.
                    Some(o) if o.len() > 1 => body.find(o).or_else(|| body.find('\n')).unwrap_or(body.len()),
                    Some(o) => closing_quote(body, if o == "\"" { '"' } else { '\'' }),
                    None if header => body.find('\n').unwrap_or(body.len()),
                    None => body.find(char::is_whitespace).unwrap_or(body.len()),
                };
                // After a colon, a short bare word next to "token" is prose (`token: expired`).
                let prose =
                    c == ':' && opening.is_none() && !header && name.to_ascii_lowercase().ends_with("token") && len < 8;
                if len >= 1 && !prose {
                    let head = 1 + spaces + opening.map_or(0, str::len);
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
            (r#"{\"password\":\"s3 cret\",\"n\":1}"#, r#"{\"password\":\"[redacted]\",\"n\":1}"#),
            (r#"{"token":"a\"b c","n":1}"#, r#"{"token":"[redacted]","n":1}"#),
            ("password = abc", "password = [redacted]"),
            ("password: short", "password: [redacted]"),
            ("token=a,b;c&d next", "token=[redacted] next"),
            ("secret_key=x", "secret_key=[redacted]"),
            ("passphrase=x", "passphrase=[redacted]"),
            ("db_pass=x", "db_pass=[redacted]"),
            ("dbPass=x", "dbPass=[redacted]"),
            ("secretKey=x", "secretKey=[redacted]"),
            (r#"{"accessKey":"x","privateKey":"y"}"#, r#"{"accessKey":"[redacted]","privateKey":"[redacted]"}"#),
            ("postgres://u:pa@ss/word@10.0.0.1/db", "postgres://[redacted]@10.0.0.1/db"),
            (r#"postgres://u:pa"ss@10.0.0.1/db"#, "postgres://[redacted]@10.0.0.1/db"),
            (r#"{"url":"postgres://h/db","n":"a:b@c"}"#, r#"{"url":"postgres://h/db","n":"a:b@c"}"#),
            ("Step 2: MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj", "Step 2: [redacted]"),
            ("postgres://u:p/ss@10.0.0.1/db", "postgres://[redacted]@10.0.0.1/db"),
            ("-----END PRIVATE KEY-----", "[redacted]"),
            ("/IEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj", "[redacted]"),
            ("MIIEvQIB/DANBgkqhkiG9w0B/QEFAASCBKcw/gSjAgEAAoIBAQC7VJTUt9Us8cKj", "[redacted]"),
            ("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLE+KEY1", "[redacted]"),
            ("MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj", "[redacted]"),
            ("-----BEGIN PRIVATE KEY----- abc", "[redacted]"),
        ] {
            assert_eq!(redact_tokens(text), want, "{text}");
        }
    }

    #[test]
    fn a_key_block_is_followed_per_source() {
        let lines = [
            (1, "APPEND -----BEGIN PRIVATE KEY-----"),
            (2, "another step"),
            (1, "c2hvcnQ="),
            (2, "-----END PRIVATE KEY-----"),
            (1, "c2hvcnQ="),
            (1, "-----END PRIVATE KEY-----"),
            (1, "after"),
            (3, "-----BEGIN PRIVATE KEY----- abc -----END PRIVATE KEY-----"),
            (3, "after"),
            (4, "----- BEGIN RSA PRIVATE KEY -----"),
            (4, "c2hvcnQ="),
            (4, "-----END PRIVATE KEY----- -----BEGIN PRIVATE KEY-----"),
            (4, "c2hvcnQ="),
        ];
        let got = in_key_block(lines.iter().map(|(s, l)| (*s, *l)));
        assert_eq!(got, [true, false, true, true, true, true, false, true, false, true, true, true, true]);
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
            "bypass=1",
            "-----BEGIN CERTIFICATE-----",
            "src/components/SomeVeryLongComponentNameWithNumber2AndMore.tsx",
            "/workspace/packages/MyComponent/SomeOtherFolder2/AndAnotherOneHere",
            "https://example.com/react@18.3.1/index.js",
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b",
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
