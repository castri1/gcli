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

/// `text` made safe to show and keep: terminal sequences removed, anything shaped like an
/// OAuth token (`ya29.…` access tokens, `1//…` refresh tokens, `eyJ…` JWTs) replaced, and cut
/// to a line or two. Error texts end up in toasts, logs pasted into chats and reports for an AI.
pub fn redact(text: &str) -> String {
    let clean = crate::util::sanitize(text);
    let words: Vec<String> = clean
        .split(' ')
        .map(|w| {
            let bare =
                w.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '/' && c != '-' && c != '_');
            let jwt = bare.starts_with("eyJ") && bare.len() > 40;
            if bare.starts_with("ya29.") || bare.starts_with("1//") || jwt {
                "[redacted]".to_string()
            } else {
                w.to_string()
            }
        })
        .collect();
    crate::util::ellipsize(words.join(" ").trim(), 300)
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
