//! Minimal Google Cloud client: JSON over one shared HTTP agent, OAuth access tokens from
//! Application Default Credentials or gcloud ([`auth`]), retries, page tokens and Google's
//! error bodies mapped to [`GcpError`].
//!
//! Adding a service = a file with typed `#[derive(Deserialize)]` structs and functions that
//! call `gcp.get(url, query)` (or `gcp.pages(…)` for lists, `gcp.post`/`gcp.patch` for
//! changes). Google's REST APIs are all alike (`https://<service>.googleapis.com/<version>/…`,
//! camelCase JSON, `nextPageToken`), so no per-service SDK is needed.
//!
//! `GCLI_API_ROOT` (like the AWS SDKs' `AWS_ENDPOINT_URL`) sends every call to another server:
//! `https://run.googleapis.com/v2/…` goes to `$GCLI_API_ROOT/run/v2/…`, the token endpoint to
//! `$GCLI_API_ROOT/oauth2/token`. For tests and local fakes.

pub mod artifacts;
pub mod auth;
pub mod bigquery;
pub mod budgets;
pub mod build;
pub mod certmanager;
pub mod compute;
pub mod error;
pub mod gcloud;
pub mod http;
pub mod logging;
#[cfg(test)]
pub mod mock;
pub mod monitoring;
pub mod run;
pub mod secrets;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

pub use auth::Auth;
pub use error::{GcpError, Result};

use crate::util::uri_encode;

const MAX_ATTEMPTS: u32 = 4;
/// First wait before a retry; it doubles each time.
const BACKOFF_MS: u64 = if cfg!(test) { 1 } else { 250 };
/// A list stops after this many pages, whatever the server says (a token that never ends).
const MAX_PAGES: usize = 1000;

/// How a call is made.
#[derive(Clone, Copy, Default)]
pub struct Opts<'a> {
    /// The project whose quota (and, for BigQuery, bill) the call uses: `x-goog-user-project`.
    /// Without it, the project of the credentials' OAuth client, or the resource's own.
    pub quota_project: Option<&'a str>,
    /// May be sent again after a server error or a lost connection, when it may already
    /// have run. Reads always may; a change only when repeating it does no harm.
    pub idempotent: bool,
}

pub struct Gcp {
    pub project: String,
    /// Empty: every region (Cloud Run's `locations/-`).
    pub region: String,
    auth: Arc<Auth>,
    /// `GCLI_API_ROOT`, read once.
    root: Option<String>,
}

/// `GCLI_API_ROOT`, without a trailing slash.
pub fn api_root() -> Option<String> {
    std::env::var("GCLI_API_ROOT").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty())
}

/// `url` as sent: unchanged, or for a `https://<service>.googleapis.com/<path>` URL with a
/// root set, `<root>/<service>/<path>`.
pub fn api_url(root: Option<&str>, url: &str) -> String {
    let Some(root) = root else { return url.to_string() };
    let Some(rest) = url.strip_prefix("https://") else { return url.to_string() };
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    match host.strip_suffix(".googleapis.com") {
        Some(service) => format!("{root}/{service}/{path}"),
        None => url.to_string(),
    }
}

/// Runs `call` until it succeeds, fails for good, or [`MAX_ATTEMPTS`] are spent, waiting
/// longer each time. Throttling is always retried (Google turned the request away);
/// server errors and lost connections only when `idempotent`.
pub(crate) fn retrying<T>(idempotent: bool, backoff_ms: u64, mut call: impl FnMut() -> Result<T>) -> Result<T> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        match call() {
            Err(e) if (if idempotent { e.is_retryable() } else { e.is_throttling() }) && attempt < MAX_ATTEMPTS => {
                std::thread::sleep(Duration::from_millis(backoff_ms * 2u64.pow(attempt)));
            }
            other => return other,
        }
    }
}

impl Gcp {
    /// A client for `project` and `region` with the process-wide credentials.
    pub fn new(project: &str, region: &str) -> Arc<Gcp> {
        Gcp::with(project, region, auth::shared(), api_root())
    }

    pub fn with(project: &str, region: &str, auth: Arc<Auth>, root: Option<String>) -> Arc<Gcp> {
        Arc::new(Gcp { project: project.to_string(), region: region.to_string(), auth, root })
    }

    /// Forgets the cached token (after a sign-in).
    pub fn reset_credentials(&self) {
        self.auth.invalidate();
    }

    /// Fails fast with `LoginRequired` when no token can be had.
    pub fn check_credentials(&self) -> Result<()> {
        self.auth.token().map(|_| ())
    }

    /// GET a resource or a page of a list.
    pub fn get<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)]) -> Result<T> {
        self.get_with(url, query, Opts::default())
    }

    pub fn get_with<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)], opts: Opts) -> Result<T> {
        let opts = Opts { idempotent: true, ..opts };
        parse(&self.send("GET", url, query, None, opts)?)
    }

    /// GET a resource exactly as Google sent it, strings uncleaned: for a change that sends
    /// it back, where every field must stay as it is. Never shown.
    pub fn get_exact(&self, url: &str) -> Result<Value> {
        let bytes = self.send("GET", url, &[], None, Opts { idempotent: true, ..Opts::default() })?;
        serde_json::from_slice(if bytes.is_empty() { b"{}" } else { &bytes }).map_err(unexpected)
    }

    /// POST a JSON body (a query, an action such as `:run`).
    pub fn post<T: DeserializeOwned>(&self, url: &str, body: &Value, opts: Opts) -> Result<T> {
        parse(&self.send("POST", url, &[], Some(body), opts)?)
    }

    /// PATCH a resource; `query` usually names the fields changed (`updateMask`).
    pub fn patch<T: DeserializeOwned>(&self, url: &str, query: &[(&str, &str)], body: &Value, opts: Opts) -> Result<T> {
        parse(&self.send("PATCH", url, query, Some(body), opts)?)
    }

    /// Lists every item of a paginated list: each page's `field` array is handed to `each`,
    /// which returns false to stop; `nextPageToken` leads to the next page.
    pub fn pages<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
        field: &str,
        opts: Opts,
        mut each: impl FnMut(Vec<T>) -> bool,
    ) -> Result<()> {
        let mut token: Option<String> = None;
        let mut seen = HashSet::new();
        for _ in 0..MAX_PAGES {
            let mut q: Vec<(&str, &str)> = query.to_vec();
            if let Some(t) = &token {
                q.push(("pageToken", t));
            }
            let page: Value = self.get_with(url, &q, opts)?;
            let items: Vec<T> = match page.get(field) {
                Some(v) => serde_json::from_value(v.clone()).map_err(unexpected)?,
                None => Vec::new(),
            };
            if !each(items) {
                return Ok(());
            }
            match page.get("nextPageToken").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                // A token seen before would loop forever.
                Some(next) if seen.insert(next.to_string()) => token = Some(next.to_string()),
                _ => return Ok(()),
            }
        }
        Ok(())
    }

    /// [`pages`](Self::pages) collected into one list.
    pub fn list<T: DeserializeOwned>(
        &self,
        url: &str,
        query: &[(&str, &str)],
        field: &str,
        opts: Opts,
    ) -> Result<Vec<T>> {
        let mut all = Vec::new();
        self.pages(url, query, field, opts, |page: Vec<T>| {
            all.extend(page);
            true
        })?;
        Ok(all)
    }

    /// Sends a request with a token, retrying as [`retrying`] says. A 401 means the token was
    /// refused (expired early, revoked): it is renewed once, and a second refusal asks for a
    /// sign-in.
    fn send(
        &self,
        method: &str,
        url: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
        opts: Opts,
    ) -> Result<Vec<u8>> {
        let mut full = api_url(self.root.as_deref(), url);
        if !query.is_empty() {
            let q: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v))).collect();
            full.push(if full.contains('?') { '&' } else { '?' });
            full.push_str(&q.join("&"));
        }
        let payload = body.map(|b| serde_json::to_vec(b).unwrap_or_default()).unwrap_or_default();
        let mut renewed = false;
        loop {
            let token = self.auth.token()?;
            let result = retrying(opts.idempotent, BACKOFF_MS, || {
                let mut headers = vec![
                    ("authorization".to_string(), format!("Bearer {token}")),
                    ("accept".to_string(), "application/json".to_string()),
                ];
                if body.is_some() {
                    headers.push(("content-type".to_string(), "application/json".to_string()));
                }
                if let Some(project) = opts.quota_project.filter(|p| !p.is_empty()) {
                    headers.push(("x-goog-user-project".to_string(), project.to_string()));
                }
                let resp = http::send(method, &full, &headers, payload.clone())?;
                if resp.ok() { Ok(resp.body) } else { Err(error::api_error(resp.status, &resp.body)) }
            });
            match result {
                Err(e) if e.is_unauthenticated() && !renewed => {
                    renewed = true;
                    self.auth.invalidate();
                }
                Err(e) if e.is_unauthenticated() => return Err(GcpError::LoginRequired { detail: e.short() }),
                other => return other,
            }
        }
    }
}

fn unexpected(e: serde_json::Error) -> GcpError {
    GcpError::Network(format!("unexpected response from Google Cloud: {e}"))
}

/// Deserializes a response body. Absent fields fall back to their defaults (explicit `null`s
/// are dropped first), and strings lose terminal control sequences: names, labels and
/// messages are shown in a terminal and must not be able to drive it.
fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let bytes = if bytes.is_empty() { b"{}".as_slice() } else { bytes };
    let mut value: Value = serde_json::from_slice(bytes).map_err(unexpected)?;
    clean(&mut value);
    T::deserialize(value).map_err(unexpected)
}

fn clean(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.retain(|_, v| !v.is_null());
            map.values_mut().for_each(clean);
        }
        Value::Array(items) => items.iter_mut().for_each(clean),
        Value::String(s) if s.chars().any(crate::util::is_unsafe) => *s = crate::util::sanitize(s),
        _ => {}
    }
}

/// The Cloud console page `path` of `project` (`run/detail/us-east1/app`).
pub fn console_url(project: &str, path: &str) -> String {
    format!("https://console.cloud.google.com/{path}?project={}", uri_encode(project))
}

#[cfg(test)]
mod tests {
    use super::mock::{MockServer, Reply};
    use super::*;

    fn client(server: &MockServer) -> Arc<Gcp> {
        Gcp::with("my-project", "us-east1", auth::tests::fixed("ya29.test"), Some(server.url()))
    }

    #[test]
    fn urls_follow_the_api_root() {
        let url = "https://run.googleapis.com/v2/projects/p/locations/us-east1/services";
        assert_eq!(api_url(None, url), url);
        assert_eq!(
            api_url(Some("http://127.0.0.1:8080"), url),
            "http://127.0.0.1:8080/run/v2/projects/p/locations/us-east1/services"
        );
        assert_eq!(
            api_url(Some("http://localhost:1"), "https://oauth2.googleapis.com/token"),
            "http://localhost:1/oauth2/token"
        );
        // Only Google APIs move.
        assert_eq!(api_url(Some("http://localhost:1"), "https://api.github.com/repos"), "https://api.github.com/repos");
    }

    #[test]
    fn pages_are_followed_until_the_last() {
        let server = MockServer::start(vec![
            Reply::json(200, r#"{"items":[{"name":"a"},{"name":"b"}],"nextPageToken":"t/2"}"#),
            Reply::json(200, r#"{"items":[{"name":"c"}],"nextPageToken":"t3"}"#),
            Reply::json(200, r#"{"items":[{"name":"d","note":null}]}"#),
        ]);
        #[derive(serde::Deserialize)]
        struct Item {
            name: String,
            #[serde(default)]
            note: String,
        }
        let gcp = client(&server);
        let items: Vec<Item> = gcp
            .list("https://example.googleapis.com/v1/items", &[("pageSize", "2")], "items", Opts::default())
            .unwrap();
        assert_eq!(items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
        assert_eq!(items[3].note, "");
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        assert_eq!(
            paths,
            [
                "/example/v1/items?pageSize=2",
                "/example/v1/items?pageSize=2&pageToken=t%2F2",
                "/example/v1/items?pageSize=2&pageToken=t3"
            ]
        );
        assert_eq!(server.requests()[0].header("authorization"), Some("Bearer ya29.test"));
        assert_eq!(server.requests()[0].header("x-goog-user-project"), None);
    }

    #[test]
    fn a_repeated_page_token_ends_the_list() {
        let server = MockServer::start(vec![
            Reply::json(200, r#"{"items":[1],"nextPageToken":"same"}"#),
            Reply::json(200, r#"{"items":[2],"nextPageToken":"same"}"#),
        ]);
        let items: Vec<u32> =
            client(&server).list("https://x.googleapis.com/v1/i", &[], "items", Opts::default()).unwrap();
        assert_eq!(items, [1, 2]);
        // Stopping early.
        let server = MockServer::start(vec![Reply::json(200, r#"{"items":[1],"nextPageToken":"more"}"#)]);
        let mut pages = 0;
        client(&server)
            .pages("https://x.googleapis.com/v1/i", &[], "items", Opts::default(), |_: Vec<u32>| {
                pages += 1;
                false
            })
            .unwrap();
        assert_eq!((pages, server.requests().len()), (1, 1));
    }

    #[test]
    fn retries_server_errors_and_maps_google_errors() {
        let server = MockServer::start(vec![
            Reply::json(503, r#"{"error":{"code":503,"status":"UNAVAILABLE","message":"try again"}}"#),
            Reply::json(429, r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"slow down"}}"#),
            Reply::json(200, r#"{"ok":true}"#),
            Reply::json(
                403,
                r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Permission denied on resource project my-project."}}"#,
            ),
        ]);
        let gcp = client(&server);
        let v: Value = gcp.get("https://run.googleapis.com/v2/x", &[]).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(server.requests().len(), 3);
        let e = gcp.get::<Value>("https://run.googleapis.com/v2/x", &[]).unwrap_err();
        assert!(matches!(&e, GcpError::Api { status: 403, code, .. } if code == "PERMISSION_DENIED"), "{e:?}");
        assert_eq!(server.requests().len(), 4, "a 403 is not retried");
    }

    #[test]
    fn changes_are_not_repeated_after_a_server_error() {
        let server = MockServer::start(vec![
            Reply::json(500, r#"{"error":{"code":500,"status":"INTERNAL","message":"boom"}}"#),
            Reply::json(200, "{}"),
        ]);
        let gcp = client(&server);
        let e = gcp
            .post::<Value>("https://run.googleapis.com/v2/x:run", &serde_json::json!({}), Opts::default())
            .unwrap_err();
        assert!(matches!(e, GcpError::Api { status: 500, .. }));
        assert_eq!(server.requests().len(), 1);
        // Unless the caller says it is safe, and with the quota project as asked.
        let server = MockServer::start(vec![Reply::json(500, "{}"), Reply::json(200, r#"{"done":true}"#)]);
        let gcp = client(&server);
        let opts = Opts { quota_project: Some("billing-project"), idempotent: true };
        let v: Value = gcp
            .post(
                "https://bigquery.googleapis.com/bigquery/v2/projects/p/queries",
                &serde_json::json!({"query": "SELECT 1"}),
                opts,
            )
            .unwrap();
        assert_eq!(v["done"], true);
        let r = &server.requests()[1];
        assert_eq!((r.method.as_str(), r.path.as_str()), ("POST", "/bigquery/bigquery/v2/projects/p/queries"));
        assert_eq!(r.header("x-goog-user-project"), Some("billing-project"));
        assert_eq!(r.header("content-type"), Some("application/json"));
        assert_eq!(r.body, r#"{"query":"SELECT 1"}"#);
    }

    #[test]
    fn a_refused_token_is_renewed_once_then_a_sign_in_is_required() {
        let unauthenticated = r#"{"error":{"code":401,"status":"UNAUTHENTICATED","message":"Request had invalid authentication credentials."}}"#;
        let server = MockServer::start(vec![Reply::json(401, unauthenticated), Reply::json(200, r#"{"ok":1}"#)]);
        let gcp = client(&server);
        assert!(gcp.get::<Value>("https://run.googleapis.com/v2/x", &[]).is_ok());
        server.push(Reply::json(401, unauthenticated));
        server.push(Reply::json(401, unauthenticated));
        let e = gcp.get::<Value>("https://run.googleapis.com/v2/x", &[]).unwrap_err();
        assert!(e.is_login_required(), "{e:?}");
        assert!(!e.to_string().contains("ya29"), "{e}");
    }

    #[test]
    fn responses_are_cleaned() {
        let server =
            MockServer::start(vec![Reply::json(200, "{\"name\":\"svc\\u001b]52;c;aGk=\\u0007x\",\"labels\":null}")]);
        #[derive(serde::Deserialize)]
        struct S {
            name: String,
            #[serde(default)]
            labels: std::collections::HashMap<String, String>,
        }
        let s: S = client(&server).get("https://run.googleapis.com/v2/s", &[]).unwrap();
        assert_eq!(s.name, "svcx");
        assert!(s.labels.is_empty());
    }
}
