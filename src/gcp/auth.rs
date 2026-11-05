//! Access tokens for Google APIs, without an SDK.
//!
//! 1. Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`, else
//!    `application_default_credentials.json` in gcloud's folder) of type `authorized_user`:
//!    its refresh token is exchanged at the OAuth token endpoint and the access token is kept
//!    until a minute before it expires.
//! 2. `gcloud auth print-access-token`, when ADC is missing, is another kind of credential
//!    (service account key, workload identity…), or Google refuses its refresh token
//!    (`invalid_grant`, `invalid_rapt`: the session needs a new sign-in). Its token is kept
//!    for 50 minutes (they last an hour).
//!
//! When neither gives a token the error is `LoginRequired`: the UI offers `L`, the CLI says
//! `gcli login`, both of which run `gcloud auth login --update-adc`. One `Auth` serves the
//! whole process (credentials belong to the person, not to an environment).
//!
//! Tokens never leave this module except in the `Authorization` header: they are not logged,
//! not shown and not put in errors.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use serde::Deserialize;

use super::error::{GcpError, Result, api_error};
use super::{api_url, gcloud, http};
use crate::util::{now_secs, uri_encode};

const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
/// An access token is renewed this long before it expires.
const EARLY: f64 = 60.0;
/// How long a token from gcloud is used (they last an hour; gcloud may hand out one it
/// already had, so a 401 also renews it).
const GCLOUD_TTL: f64 = 50.0 * 60.0;

struct Token {
    value: String,
    expires_at: f64,
}

/// Runs `gcloud auth print-access-token` (tests give their own).
type GcloudToken = Box<dyn Fn() -> std::result::Result<String, String> + Send + Sync>;

pub struct Auth {
    /// The ADC file to read; `None` without a home directory.
    adc_path: Option<PathBuf>,
    token_url: String,
    gcloud: GcloudToken,
    cached: Mutex<Option<Token>>,
    /// How the current token was obtained, and why gcloud was used when it was (for
    /// `GCLI_DEBUG` and `gcli login`).
    how: Mutex<Option<String>>,
}

/// What the ADC file holds.
#[derive(Debug)]
enum Adc {
    Missing,
    /// Not JSON, or a user credential without its refresh token.
    Unreadable(String),
    /// A kind gcli does not exchange itself (`service_account`, `external_account`…).
    Other(String),
    User {
        client_id: String,
        client_secret: String,
        refresh_token: String,
    },
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct AdcFile {
    #[serde(rename = "type")]
    kind: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenResponse {
    access_token: String,
    expires_in: f64,
}

/// What came of trying ADC, which decides whether gcloud is asked ([`decide`]).
#[derive(Debug)]
enum AdcOutcome {
    Token(String, f64),
    Missing,
    Unreadable(String),
    Other(String),
    /// The token endpoint answered with an error.
    Refused(GcpError),
    /// It could not be reached (after retries).
    Failed(GcpError),
}

/// The process-wide credentials: ADC from its usual place, Google's token endpoint (or
/// `GCLI_API_ROOT`), the gcloud on the PATH.
pub fn shared() -> Arc<Auth> {
    static SHARED: OnceLock<Arc<Auth>> = OnceLock::new();
    SHARED
        .get_or_init(|| {
            let root = super::api_root();
            Arc::new(Auth::new(
                default_adc_path(),
                api_url(root.as_deref(), TOKEN_URL),
                Box::new(gcloud::print_access_token),
            ))
        })
        .clone()
}

/// `GOOGLE_APPLICATION_CREDENTIALS`, else the file `gcloud auth application-default login`
/// (or `gcloud auth login --update-adc`) writes.
fn default_adc_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(gcloud::config_dir()?.join("application_default_credentials.json"))
}

fn read_adc(path: Option<&Path>) -> Adc {
    let Some(path) = path else { return Adc::Missing };
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Adc::Missing,
        Err(e) => return Adc::Unreadable(format!("cannot read {}: {}", crate::util::display_path(path), e.kind())),
    };
    let Ok(file) = serde_json::from_str::<AdcFile>(&text) else {
        return Adc::Unreadable(format!("{} is not JSON", crate::util::display_path(path)));
    };
    match file.kind.as_str() {
        "authorized_user" if file.refresh_token.is_empty() || file.client_id.is_empty() => {
            Adc::Unreadable("the ADC file has no refresh token".into())
        }
        "authorized_user" => Adc::User {
            client_id: file.client_id,
            client_secret: file.client_secret,
            refresh_token: file.refresh_token,
        },
        other => Adc::Other(crate::util::sanitize(other)),
    }
}

/// What to do once ADC was tried.
#[derive(Debug)]
enum Next {
    /// ADC gave this token, valid for so many seconds.
    Use(String, f64),
    /// Fail with this error.
    Fail(GcpError),
    /// Ask gcloud, for this reason.
    AskGcloud(String),
}

/// Whether to ask gcloud for a token after trying ADC, and why. A network failure is not a
/// reason: gcloud would meet the same network, and the error says what is wrong.
fn decide(outcome: AdcOutcome) -> Next {
    match outcome {
        AdcOutcome::Token(token, expires_in) => Next::Use(token, expires_in),
        AdcOutcome::Failed(e) => Next::Fail(e),
        AdcOutcome::Missing => Next::AskGcloud("no Application Default Credentials".into()),
        AdcOutcome::Unreadable(why) => Next::AskGcloud(why),
        AdcOutcome::Other(kind) => Next::AskGcloud(format!("ADC of type {kind}")),
        // invalid_grant (expired or revoked), invalid_rapt (reauthentication required),
        // invalid_client: this refresh token will not work again until a new sign-in.
        AdcOutcome::Refused(e) => Next::AskGcloud(format!("ADC refused: {}", e.short())),
    }
}

/// Google refused the refresh token itself, as opposed to failing to answer.
fn is_refusal(e: &GcpError) -> bool {
    match e {
        GcpError::Api { status, code, message } => {
            let text = format!("{code} {message}").to_ascii_lowercase();
            (400..500).contains(status)
                || text.contains("invalid_grant")
                || text.contains("invalid_rapt")
                || text.contains("reauth")
        }
        _ => false,
    }
}

impl Auth {
    fn new(adc_path: Option<PathBuf>, token_url: String, gcloud: GcloudToken) -> Auth {
        Auth { adc_path, token_url, gcloud, cached: Mutex::new(None), how: Mutex::new(None) }
    }

    /// A valid access token, from the cache or freshly obtained. Holding the lock while
    /// obtaining one means threads starting together wait for a single refresh.
    pub fn token(&self) -> Result<String> {
        let mut cached = self.cached.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = cached.as_ref().filter(|t| now_secs() < t.expires_at) {
            return Ok(t.value.clone());
        }
        let (token, how) = self.obtain()?;
        *self.how.lock().unwrap_or_else(|e| e.into_inner()) = Some(how);
        let value = token.value.clone();
        *cached = Some(token);
        Ok(value)
    }

    /// Forgets the cached token (after a sign-in, or when Google refused it).
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// How the last token was obtained, in words (never the token): `Application Default
    /// Credentials…` or `gcloud auth print-access-token (why)`.
    pub fn describe(&self) -> Option<String> {
        self.how.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn obtain(&self) -> Result<(Token, String)> {
        let reason = match decide(self.try_adc()) {
            Next::Use(value, expires_in) => {
                let token = Token { value, expires_at: now_secs() + expires_in - EARLY };
                return Ok((token, "Application Default Credentials (authorized_user)".into()));
            }
            Next::Fail(e) => return Err(e),
            Next::AskGcloud(reason) => reason,
        };
        match (self.gcloud)() {
            Ok(value) => Ok((
                Token { value, expires_at: now_secs() + GCLOUD_TTL },
                format!("gcloud auth print-access-token ({reason})"),
            )),
            Err(why) => {
                Err(GcpError::LoginRequired { detail: super::error::redact(&format!("{reason}; gcloud: {why}")) })
            }
        }
    }

    fn try_adc(&self) -> AdcOutcome {
        let (client_id, client_secret, refresh_token) = match read_adc(self.adc_path.as_deref()) {
            Adc::Missing => return AdcOutcome::Missing,
            Adc::Unreadable(why) => return AdcOutcome::Unreadable(why),
            Adc::Other(kind) => return AdcOutcome::Other(kind),
            Adc::User { client_id, client_secret, refresh_token } => (client_id, client_secret, refresh_token),
        };
        let form = format!(
            "grant_type=refresh_token&client_id={}&client_secret={}&refresh_token={}",
            uri_encode(&client_id),
            uri_encode(&client_secret),
            uri_encode(&refresh_token)
        );
        let headers = [("content-type".to_string(), "application/x-www-form-urlencoded".to_string())];
        // Exchanging the refresh token has no effect but a new access token: safe to repeat.
        let result = super::retrying(true, super::BACKOFF_MS, || {
            let resp = http::send("POST", &self.token_url, &headers, form.clone().into_bytes())?;
            if resp.ok() { Ok(resp.body) } else { Err(api_error(resp.status, &resp.body)) }
        });
        match result {
            Ok(body) => match serde_json::from_slice::<TokenResponse>(&body) {
                Ok(t) if !t.access_token.is_empty() => {
                    let expires_in = if t.expires_in > EARLY { t.expires_in } else { 3600.0 };
                    AdcOutcome::Token(t.access_token, expires_in)
                }
                _ => AdcOutcome::Failed(GcpError::Network("unexpected answer from the OAuth token endpoint".into())),
            },
            Err(e) if is_refusal(&e) => AdcOutcome::Refused(e),
            Err(e) => AdcOutcome::Failed(e),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An `Auth` that always hands out `token`, for tests of the API calls.
    pub(crate) fn fixed(token: &str) -> Arc<Auth> {
        let token = token.to_string();
        Arc::new(Auth::new(None, "http://127.0.0.1:9/token".into(), Box::new(move || Ok(token.clone()))))
    }

    fn adc_file(name: &str, content: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("gcli-adc-{name}-{}.json", std::process::id()));
        std::fs::write(&path, content).unwrap();
        path
    }

    const USER_ADC: &str = r#"{"type":"authorized_user","client_id":"cid.apps.googleusercontent.com","client_secret":"shh","refresh_token":"1//refresh","quota_project_id":"q"}"#;

    /// An `Auth` reading `adc` and talking to `server`, whose gcloud answers `gcloud` and
    /// counts its runs.
    fn auth(
        adc: Option<PathBuf>,
        server: &MockServer,
        gcloud: std::result::Result<&str, &str>,
    ) -> (Auth, Arc<AtomicUsize>) {
        let runs = Arc::new(AtomicUsize::new(0));
        let counter = runs.clone();
        let answer = gcloud.map(str::to_string).map_err(str::to_string);
        let gcloud: GcloudToken = Box::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
            answer.clone()
        });
        (Auth::new(adc, api_url(Some(&server.url()), TOKEN_URL), gcloud), runs)
    }

    #[test]
    fn adc_refresh_token_is_exchanged_and_cached() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"access_token":"ya29.fresh","expires_in":3599,"token_type":"Bearer"}"#,
        )]);
        let path = adc_file("exchange", USER_ADC);
        let (auth, gcloud_runs) = auth(Some(path.clone()), &server, Err("unused"));
        assert_eq!(auth.token().unwrap(), "ya29.fresh");
        assert_eq!(auth.token().unwrap(), "ya29.fresh", "the second call is served from the cache");
        assert!(auth.describe().unwrap().starts_with("Application Default Credentials"));
        assert_eq!(gcloud_runs.load(Ordering::SeqCst), 0);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let r = &requests[0];
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/oauth2/token"));
        assert!(r.header("content-type").unwrap().starts_with("application/x-www-form-urlencoded"));
        for part in [
            "grant_type=refresh_token",
            "client_id=cid.apps.googleusercontent.com",
            "client_secret=shh",
            "refresh_token=1%2F%2Frefresh",
        ] {
            assert!(r.body.contains(part), "{part} in {}", r.body);
        }
        // Invalidated: exchanged again.
        auth.invalidate();
        server.push(Reply::json(200, r#"{"access_token":"ya29.second","expires_in":3599}"#));
        assert_eq!(auth.token().unwrap(), "ya29.second");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reauth_errors_fall_back_to_gcloud() {
        let refusal = r#"{"error":"invalid_grant","error_description":"reauth related error (invalid_rapt)","error_subtype":"invalid_rapt"}"#;
        let server = MockServer::start(vec![Reply::json(400, refusal)]);
        let path = adc_file("reauth", USER_ADC);
        let (auth, runs) = auth(Some(path.clone()), &server, Ok("ya29.from-gcloud"));
        assert_eq!(auth.token().unwrap(), "ya29.from-gcloud");
        let how = auth.describe().unwrap();
        assert!(how.starts_with("gcloud auth print-access-token") && how.contains("invalid_grant"), "{how}");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(server.requests().len(), 1, "a refusal is not retried");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn when_gcloud_also_fails_a_sign_in_is_required() {
        let server = MockServer::start(vec![Reply::json(
            400,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
        )]);
        let path = adc_file("both", USER_ADC);
        let (auth, _) = auth(
            Some(path.clone()),
            &server,
            Err("Reauthentication failed. cannot prompt during non-interactive execution."),
        );
        let e = auth.token().unwrap_err();
        assert!(e.is_login_required(), "{e:?}");
        let text = e.to_string();
        assert!(text.contains("invalid_grant") && text.contains("Reauthentication failed"), "{text}");
        assert!(!text.contains("1//refresh") && !text.contains("shh"), "{text}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn server_errors_are_retried_and_network_failures_are_not_a_sign_in() {
        let server = MockServer::start(vec![
            Reply::json(503, "{}"),
            Reply::json(200, r#"{"access_token":"ya29.after-retry","expires_in":3599}"#),
        ]);
        let path = adc_file("retry", USER_ADC);
        let (auth, runs) = auth(Some(path.clone()), &server, Err("unused"));
        assert_eq!(auth.token().unwrap(), "ya29.after-retry");
        assert_eq!(server.requests().len(), 2);

        // Nothing listens there: a network error, without asking gcloud.
        let unreachable = Auth::new(Some(path.clone()), "http://127.0.0.1:9/token".into(), Box::new(|| Ok("x".into())));
        let e = unreachable.token().unwrap_err();
        assert!(matches!(e, GcpError::Network(_)), "{e:?}");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_gcloud_fallback_decision() {
        let refused = |code: &str, message: &str| {
            AdcOutcome::Refused(GcpError::Api { status: 400, code: code.into(), message: message.into() })
        };
        let ask = |outcome: AdcOutcome| match decide(outcome) {
            Next::AskGcloud(reason) => Some(reason),
            _ => None,
        };
        assert!(matches!(decide(AdcOutcome::Token("t".into(), 3600.0)), Next::Use(t, _) if t == "t"));
        assert!(matches!(decide(AdcOutcome::Failed(GcpError::Network("reset".into()))), Next::Fail(_)));
        assert!(ask(AdcOutcome::Missing).unwrap().contains("no Application Default Credentials"));
        assert!(ask(AdcOutcome::Other("service_account".into())).unwrap().contains("service_account"));
        assert!(ask(AdcOutcome::Unreadable("not JSON".into())).is_some());
        assert!(ask(refused("invalid_grant", "reauth related error (invalid_rapt)")).unwrap().contains("invalid_rapt"));

        // What counts as a refusal of the refresh token.
        let api = |status: u16, code: &str, message: &str| GcpError::Api {
            status,
            code: code.into(),
            message: message.into(),
        };
        assert!(is_refusal(&api(400, "invalid_grant", "Token has been expired or revoked.")));
        assert!(is_refusal(&api(400, "invalid_rapt", "")));
        assert!(is_refusal(&api(500, "HTTP500", "Reauthentication required")));
        assert!(is_refusal(&api(401, "invalid_client", "The OAuth client was deleted.")));
        assert!(!is_refusal(&api(503, "HTTP503", "unavailable")));
        assert!(!is_refusal(&GcpError::Network("connection reset".into())));
    }

    #[test]
    fn what_the_adc_file_holds() {
        assert!(matches!(read_adc(None), Adc::Missing));
        assert!(matches!(read_adc(Some(Path::new("/nonexistent/gcli/adc.json"))), Adc::Missing));
        let p = adc_file("sa", r#"{"type":"service_account","private_key":"-----BEGIN"}"#);
        assert!(matches!(read_adc(Some(&p)), Adc::Other(k) if k == "service_account"));
        std::fs::write(&p, "not json").unwrap();
        assert!(matches!(read_adc(Some(&p)), Adc::Unreadable(_)));
        std::fs::write(&p, r#"{"type":"authorized_user","client_id":"c"}"#).unwrap();
        assert!(matches!(read_adc(Some(&p)), Adc::Unreadable(_)));
        std::fs::write(&p, USER_ADC).unwrap();
        assert!(matches!(read_adc(Some(&p)), Adc::User { ref refresh_token, .. } if refresh_token == "1//refresh"));
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn missing_adc_uses_gcloud_and_caches_its_token() {
        let server = MockServer::start(Vec::new());
        let (auth, runs) = auth(None, &server, Ok("ya29.gcloud"));
        assert_eq!(auth.token().unwrap(), "ya29.gcloud");
        assert_eq!(auth.token().unwrap(), "ya29.gcloud");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "kept for 50 minutes");
        assert!(server.requests().is_empty());
    }
}
