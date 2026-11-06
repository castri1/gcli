//! Cloud Logging API v2: a project's log entries. Reads are free but limited to about 60 requests
//! a minute per project, so callers ask for what they show and no more.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Gcp, Opts, Result};
use crate::util::parse_rfc3339;

/// Pages read for one [`entries`] call at most: a page can come back empty with a token, when
/// Google stops searching at its own time limit.
const MAX_PAGES: usize = 10;
/// The most the API returns in a page.
const PAGE_SIZE: usize = 1000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Entry {
    pub insert_id: String,
    pub log_name: String,
    pub timestamp: String,
    /// `DEFAULT`, `DEBUG`, `INFO`, `NOTICE`, `WARNING`, `ERROR`, `CRITICAL`, `ALERT`, `EMERGENCY`.
    pub severity: String,
    pub text_payload: String,
    pub json_payload: Option<Value>,
    pub proto_payload: Option<Value>,
    pub http_request: Option<HttpRequest>,
    pub resource: Resource,
    pub labels: BTreeMap<String, String>,
    pub trace: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HttpRequest {
    pub request_method: String,
    pub request_url: String,
    pub status: i64,
    /// `"0.123s"`.
    pub latency: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Resource {
    #[serde(rename = "type")]
    pub kind: String,
    pub labels: BTreeMap<String, String>,
}

impl Entry {
    pub fn at(&self) -> Option<f64> {
        parse_rfc3339(&self.timestamp)
    }

    /// The line to show: the text, a structured log's message, the request, or the JSON itself.
    pub fn message(&self) -> String {
        if !self.text_payload.is_empty() {
            return self.text_payload.clone();
        }
        if let Some(payload) = &self.json_payload {
            for key in ["message", "msg", "error", "textPayload"] {
                if let Some(text) = payload.get(key).and_then(Value::as_str).filter(|t| !t.is_empty()) {
                    return text.to_string();
                }
            }
            return payload.to_string();
        }
        if let Some(r) = &self.http_request {
            return format!("{} {} {} {}", r.request_method, r.status, r.request_url, r.latency).trim().to_string();
        }
        if let Some(method) = self.proto_payload.as_ref().and_then(|p| p.get("methodName")).and_then(Value::as_str) {
            return method.to_string();
        }
        String::new()
    }

    /// `WARNING` or worse.
    pub fn is_problem(&self) -> bool {
        matches!(self.severity.as_str(), "WARNING" | "ERROR" | "CRITICAL" | "ALERT" | "EMERGENCY")
    }
}

/// `text` as a quoted string of a logging filter, so a name can't change what the filter asks.
pub fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Page {
    entries: Vec<Entry>,
    next_page_token: String,
}

/// What a read found.
#[derive(Debug, Default)]
pub struct Entries {
    pub list: Vec<Entry>,
    /// Google has more to give: over `max`, or it stopped searching at its own time limit
    /// before the end of the filter's time range (so an empty list does not mean there are none).
    pub more: bool,
}

/// Up to `max` entries of the client's project that match `filter`, newest first or oldest first.
/// A filter without a `timestamp` condition covers only the last 24 hours.
pub fn entries(gcp: &Gcp, filter: &str, newest_first: bool, max: usize) -> Result<Entries> {
    read(gcp, filter, newest_first, max, MAX_PAGES)
}

/// [`entries`] in exactly one request, for what polls and must stay inside the rate limit.
pub fn first_page(gcp: &Gcp, filter: &str, newest_first: bool, max: usize) -> Result<Entries> {
    read(gcp, filter, newest_first, max, 1)
}

fn read(gcp: &Gcp, filter: &str, newest_first: bool, max: usize, pages: usize) -> Result<Entries> {
    let url = "https://logging.googleapis.com/v2/entries:list";
    let order = if newest_first { "timestamp desc" } else { "timestamp asc" };
    let mut all = Vec::new();
    let mut token = String::new();
    for _ in 0..pages {
        let mut body = json!({
            "resourceNames": [format!("projects/{}", gcp.project)],
            "filter": filter,
            "orderBy": order,
            "pageSize": max.saturating_sub(all.len()).clamp(1, PAGE_SIZE),
        });
        if !token.is_empty() {
            body["pageToken"] = json!(token);
        }
        let page: Page = gcp.post(url, &body, Opts { idempotent: true, ..Opts::default() })?;
        all.extend(page.entries);
        token = page.next_page_token;
        if token.is_empty() || all.len() >= max {
            break;
        }
    }
    let more = !token.is_empty() || all.len() > max;
    all.truncate(max);
    Ok(Entries { list: all, more })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    #[test]
    fn reads_pages_until_it_has_enough() {
        let server = MockServer::start(vec![
            Reply::json(200, r#"{"nextPageToken":"empty-page"}"#),
            Reply::json(
                200,
                r#"{"entries":[
                    {"insertId":"a","timestamp":"2026-11-05T10:00:00.5Z","severity":"ERROR","textPayload":"boom"},
                    {"insertId":"b","timestamp":"2026-11-05T09:00:00Z","severity":"INFO","jsonPayload":{"message":"hello","n":1}}
                ],"nextPageToken":"more"}"#,
            ),
            Reply::json(200, r#"{"entries":[{"insertId":"c"},{"insertId":"d"}],"nextPageToken":"more"}"#),
        ]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let read = entries(&gcp, "severity>=ERROR", true, 3).unwrap();
        assert!(read.more, "the last page had a token");
        let got = read.list;
        assert_eq!(got.iter().map(|e| e.insert_id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(got[0].at(), parse_rfc3339("2026-11-05T10:00:00.5Z"));
        assert!(got[0].is_problem() && !got[1].is_problem());
        let requests = server.requests();
        assert_eq!(requests.len(), 3, "stops once it has enough");
        assert_eq!(requests[0].path, "/logging/v2/entries:list");
        let first: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(first["resourceNames"][0], "projects/p");
        assert_eq!(first["orderBy"], "timestamp desc");
        assert_eq!(first["pageSize"], 3);
        let last: Value = serde_json::from_str(&requests[2].body).unwrap();
        assert_eq!(last["pageToken"], "more");
        assert_eq!(last["pageSize"], 1, "only what is still missing");
    }

    #[test]
    fn a_poll_is_one_request_and_says_when_google_stopped_early() {
        let server = MockServer::start(vec![Reply::json(200, r#"{"nextPageToken":"gave-up"}"#)]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let read = first_page(&gcp, "severity>=ERROR", false, 5).unwrap();
        assert!(read.list.is_empty() && read.more, "empty, and it does not mean there are none");
        assert_eq!(server.requests().len(), 1);
        let body: Value = serde_json::from_str(&server.requests()[0].body).unwrap();
        assert_eq!(body["orderBy"], "timestamp asc");
    }

    #[test]
    fn a_line_for_every_kind_of_entry() {
        let parsed = |text: &str| serde_json::from_str::<Entry>(text).unwrap().message();
        assert_eq!(parsed(r#"{"textPayload":"plain"}"#), "plain");
        assert_eq!(parsed(r#"{"jsonPayload":{"msg":"structured"}}"#), "structured");
        assert_eq!(parsed(r#"{"jsonPayload":{"a":1}}"#), r#"{"a":1}"#);
        assert_eq!(
            parsed(
                r#"{"httpRequest":{"requestMethod":"GET","status":503,"requestUrl":"https://x/y","latency":"1.5s"}}"#
            ),
            "GET 503 https://x/y 1.5s"
        );
        assert_eq!(parsed(r#"{"protoPayload":{"methodName":"google.run.Update"}}"#), "google.run.Update");
        assert_eq!(parsed("{}"), "");
    }

    #[test]
    fn a_quoted_name_stays_one_string() {
        assert_eq!(quote("shop"), r#""shop""#);
        assert_eq!(quote(r#"a" OR severity>=DEBUG \"#), r#""a\" OR severity>=DEBUG \\""#);
    }
}
