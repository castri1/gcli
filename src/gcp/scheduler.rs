//! Cloud Scheduler API v1: a project's jobs, read-only. Nothing here runs, pauses, resumes, changes
//! or deletes a job.
//!
//! A job's headers and body can hold secrets, so they are never read: the `fields` mask leaves
//! them out of Google's answer, and [`Job`] has nowhere to keep them.

use serde::Deserialize;

use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

/// What a job's read asks for: never its headers or its body. Of a token, only that there is one.
const JOB_FIELDS: &str = "name,description,schedule,timeZone,state,status,lastAttemptTime,scheduleTime,\
                          attemptDeadline,userUpdateTime,retryConfig,\
                          httpTarget(uri,httpMethod,oidcToken/serviceAccountEmail,oauthToken/serviceAccountEmail),\
                          pubsubTarget/topicName,appEngineHttpTarget/relativeUri";

/// The most the API returns in a page.
const PAGE_SIZE: &str = "500";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Job {
    /// `projects/{project}/locations/{region}/jobs/{id}`
    pub name: String,
    pub description: String,
    /// A cron expression.
    pub schedule: String,
    pub time_zone: String,
    /// `ENABLED`, `PAUSED`, `DISABLED` or `UPDATE_FAILED`.
    pub state: String,
    /// What the last attempt answered.
    pub status: Status,
    pub last_attempt_time: String,
    /// The next run, or the next retry of a failed one. A paused job has none.
    pub schedule_time: String,
    /// `300s`
    pub attempt_deadline: String,
    /// When someone last changed the job. Google keeps no creation time.
    pub user_update_time: String,
    pub retry_config: Option<RetryConfig>,
    pub http_target: Option<HttpTarget>,
    pub pubsub_target: Option<Present>,
    pub app_engine_http_target: Option<Present>,
}

/// A `google.rpc.Status`, as far as a job's last attempt goes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Status {
    pub code: Option<i64>,
    pub message: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RetryConfig {
    pub retry_count: u32,
    pub max_retry_duration: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HttpTarget {
    pub uri: String,
    pub http_method: String,
    pub oidc_token: Option<Present>,
    pub oauth_token: Option<Present>,
}

/// Something the job has: what it holds is of no interest and is not kept.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Present {}

/// How a job's last attempt went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Succeeded,
    /// No attempt yet. Google answers code -1 for these jobs: not a `google.rpc` code, but every
    /// job seen with it had no last attempt and no entry in its logs, so it does not say that
    /// anything failed.
    NotYet,
    /// A `google.rpc` code other than OK.
    Failed(i64),
    /// A negative code on a job that has had an attempt: nothing says what that means.
    Unclear(i64),
}

/// The name of a `google.rpc` code.
pub fn code_name(code: i64) -> Option<&'static str> {
    const NAMES: [&str; 17] = [
        "OK",
        "CANCELLED",
        "UNKNOWN",
        "INVALID_ARGUMENT",
        "DEADLINE_EXCEEDED",
        "NOT_FOUND",
        "ALREADY_EXISTS",
        "PERMISSION_DENIED",
        "RESOURCE_EXHAUSTED",
        "FAILED_PRECONDITION",
        "ABORTED",
        "OUT_OF_RANGE",
        "UNIMPLEMENTED",
        "INTERNAL",
        "UNAVAILABLE",
        "DATA_LOSS",
        "UNAUTHENTICATED",
    ];
    usize::try_from(code).ok().and_then(|c| NAMES.get(c)).copied()
}

/// `UNAVAILABLE`, or `code 99` for one with no name.
pub fn code_text(code: i64) -> String {
    code_name(code).map_or_else(|| format!("code {code}"), str::to_string)
}

impl Outcome {
    /// `UNAVAILABLE`, `ok`, `no attempt yet`.
    pub fn text(self) -> String {
        match self {
            Outcome::Succeeded => "ok".into(),
            Outcome::NotYet => "no attempt yet".into(),
            Outcome::Failed(code) => code_text(code),
            Outcome::Unclear(code) => format!("code {code}"),
        }
    }
}

/// The host of `url`, lower-cased, without its user or port.
pub fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

impl Job {
    /// The job's id (`shop-nightly`).
    pub fn id(&self) -> &str {
        super::run::last_segment(&self.name)
    }

    /// Its region, from its name.
    pub fn location(&self) -> &str {
        self.name.split('/').nth(3).unwrap_or_default()
    }

    pub fn outcome(&self) -> Outcome {
        match self.status.code {
            Some(code) if code > 0 => Outcome::Failed(code),
            Some(code) if code < 0 && self.last_attempt().is_some() => Outcome::Unclear(code),
            _ if self.last_attempt().is_none() => Outcome::NotYet,
            _ => Outcome::Succeeded,
        }
    }

    pub fn last_attempt(&self) -> Option<f64> {
        parse_rfc3339(&self.last_attempt_time)
    }

    pub fn next_run(&self) -> Option<f64> {
        parse_rfc3339(&self.schedule_time)
    }

    pub fn updated(&self) -> Option<f64> {
        parse_rfc3339(&self.user_update_time)
    }

    pub fn enabled(&self) -> bool {
        self.state == "ENABLED"
    }

    /// How long an attempt may take, in seconds.
    pub fn deadline_secs(&self) -> Option<f64> {
        self.attempt_deadline.trim().strip_suffix('s')?.parse().ok()
    }

    /// The host an HTTP job calls.
    pub fn host(&self) -> Option<String> {
        host_of(&self.http_target.as_ref()?.uri)
    }

    /// The URL an HTTP job calls, without its user, query or fragment, which can hold a credential.
    pub fn url(&self) -> Option<String> {
        let uri = &self.http_target.as_ref()?.uri;
        let (scheme, rest) = uri.split_once("://")?;
        let rest = rest.split(['?', '#']).next()?;
        let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
        let authority = authority.rsplit('@').next()?;
        let slash = if path.is_empty() { "" } else { "/" };
        Some(format!("{scheme}://{authority}{slash}{path}"))
    }

    /// What it calls and how: `POST`, a Pub/Sub topic or App Engine.
    pub fn target_kind(&self) -> String {
        match &self.http_target {
            Some(t) if t.http_method.is_empty() => "HTTP".into(),
            Some(t) => t.http_method.clone(),
            None if self.pubsub_target.is_some() => "Pub/Sub".into(),
            None if self.app_engine_http_target.is_some() => "App Engine".into(),
            None => String::new(),
        }
    }

    /// The kind of token an HTTP job sends, never the token's details.
    pub fn auth_kind(&self) -> Option<&'static str> {
        let target = self.http_target.as_ref()?;
        Some(if target.oidc_token.is_some() {
            "OIDC"
        } else if target.oauth_token.is_some() {
            "OAuth"
        } else {
            "none"
        })
    }

    /// The job's page in the Cloud console.
    pub fn console_url(&self, project: &str) -> String {
        super::console_url(project, &format!("cloudscheduler/jobs/edit/{}/{}", self.location(), self.id()))
    }
}

fn jobs_url(gcp: &Gcp, location: &str) -> String {
    format!(
        "https://cloudscheduler.googleapis.com/v1/projects/{}/locations/{}/jobs",
        uri_encode(&gcp.project),
        uri_encode(location)
    )
}

/// The jobs of a location.
pub fn jobs_in(gcp: &Gcp, location: &str) -> Result<Vec<Job>> {
    let fields = format!("jobs({JOB_FIELDS}),nextPageToken");
    let query = [("pageSize", PAGE_SIZE), ("fields", fields.as_str())];
    gcp.list(&jobs_url(gcp, location), &query, "jobs", Opts::default())
}

/// The job `id` of a location.
pub fn find_in(gcp: &Gcp, location: &str, id: &str) -> Result<Job> {
    gcp.get(&format!("{}/{}", jobs_url(gcp, location), uri_encode(id)), &[("fields", JOB_FIELDS)])
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Location {
    location_id: String,
}

/// The locations of the client's project: Cloud Scheduler has no "every region".
pub fn locations(gcp: &Gcp) -> Result<Vec<String>> {
    let url = format!("https://cloudscheduler.googleapis.com/v1/projects/{}/locations", uri_encode(&gcp.project));
    let all: Vec<Location> = gcp.list(&url, &[("pageSize", "100")], "locations", Opts::default())?;
    Ok(all.into_iter().map(|l| l.location_id).filter(|l| !l.is_empty()).collect())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    /// A job as Google would answer without the `fields` mask, with what must never come out.
    pub(crate) const JOB: &str = r#"{
        "name":"projects/my-platform-123456/locations/us-east1/jobs/shop-nightly",
        "description":"Close the day",
        "schedule":"0 3 * * *","timeZone":"Etc/UTC","state":"ENABLED",
        "status":{"code":14,"message":"Unavailable"},
        "lastAttemptTime":"2026-11-05T03:00:00Z","scheduleTime":"2026-11-06T03:00:00Z",
        "attemptDeadline":"300s","userUpdateTime":"2026-10-01T00:00:00Z",
        "retryConfig":{"retryCount":2,"maxRetryDuration":"0s"},
        "httpTarget":{"uri":"https://user:hunter2@shop-123456789.us-east1.run.app:443/cron/close?key=hunter2#x",
                      "httpMethod":"POST","headers":{"X-Api-Key":"hunter2"},"body":"aHVudGVyMg==",
                      "oidcToken":{"serviceAccountEmail":"robot@p.iam.gserviceaccount.com","audience":"hunter2"}}
    }"#;

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()))
    }

    #[test]
    fn lists_a_regions_jobs_page_by_page_and_asks_only_what_it_may_show() {
        let server = MockServer::start(vec![
            Reply::json(200, &format!(r#"{{"jobs":[{JOB}],"nextPageToken":"more"}}"#)),
            Reply::json(200, r#"{"jobs":[{"name":"projects/p/locations/us-east1/jobs/orders-sync"}]}"#),
        ]);
        let jobs = jobs_in(&client(&server), "us-east1").unwrap();
        assert_eq!(jobs.iter().map(Job::id).collect::<Vec<_>>(), ["shop-nightly", "orders-sync"]);
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.method == "GET"), "it only reads");
        let first = &requests[0].path;
        assert!(
            first.starts_with("/cloudscheduler/v1/projects/my-platform-123456/locations/us-east1/jobs?"),
            "{first}"
        );
        assert!(first.contains("pageSize=500") && first.contains("fields="), "{first}");
        assert!(!first.contains("headers") && !first.contains("body"), "never asked for: {first}");
        assert!(requests[1].path.contains("pageToken=more"), "{}", requests[1].path);
    }

    #[test]
    fn a_job_says_what_it_is() {
        let job: Job = serde_json::from_str(JOB).unwrap();
        assert_eq!((job.id(), job.location()), ("shop-nightly", "us-east1"));
        assert_eq!(job.outcome(), Outcome::Failed(14));
        assert_eq!(job.outcome().text(), "UNAVAILABLE");
        assert_eq!(job.deadline_secs(), Some(300.0));
        assert!(job.enabled());
        assert_eq!(job.host().as_deref(), Some("shop-123456789.us-east1.run.app"));
        assert_eq!(job.url().as_deref(), Some("https://shop-123456789.us-east1.run.app:443/cron/close"));
        assert_eq!((job.target_kind().as_str(), job.auth_kind()), ("POST", Some("OIDC")));
        assert_eq!(job.retry_config.as_ref().unwrap().retry_count, 2);
        assert!(job.console_url("p").contains("cloudscheduler/jobs/edit/us-east1/shop-nightly"));
        // Nothing of the headers, the body or the token is kept.
        assert!(!format!("{:?}", job.http_target).contains("X-Api-Key"));
        assert!(!format!("{:?}", job.http_target).contains("robot@"));
        assert!(!format!("{:?}", job.http_target).contains("aHVudGVyMg"));
    }

    #[test]
    fn a_status_with_no_code_is_a_success_and_a_negative_one_is_not_a_failure() {
        let job = |text: &str| serde_json::from_str::<Job>(text).unwrap().outcome();
        let at = r#""lastAttemptTime":"2026-11-05T03:00:00Z""#;
        assert_eq!(job(&format!(r#"{{{at},"status":{{}}}}"#)), Outcome::Succeeded);
        assert_eq!(job(&format!(r#"{{{at}}}"#)), Outcome::Succeeded);
        assert_eq!(job(&format!(r#"{{{at},"status":{{"code":0}}}}"#)), Outcome::Succeeded);
        assert_eq!(job(&format!(r#"{{{at},"status":{{"code":2}}}}"#)), Outcome::Failed(2));
        assert_eq!(job(r#"{"status":{"code":-1}}"#), Outcome::NotYet);
        assert_eq!(job(r#"{"status":{}}"#), Outcome::NotYet);
        assert_eq!(job(&format!(r#"{{{at},"status":{{"code":-1}}}}"#)), Outcome::Unclear(-1));
        assert_eq!(Outcome::Unclear(-1).text(), "code -1");
        assert_eq!(Outcome::NotYet.text(), "no attempt yet");
    }

    #[test]
    fn google_rpc_codes_have_names() {
        for (code, name) in [
            (2, "UNKNOWN"),
            (4, "DEADLINE_EXCEEDED"),
            (7, "PERMISSION_DENIED"),
            (9, "FAILED_PRECONDITION"),
            (13, "INTERNAL"),
            (14, "UNAVAILABLE"),
            (16, "UNAUTHENTICATED"),
        ] {
            assert_eq!(code_name(code), Some(name));
        }
        assert_eq!(code_name(17), None);
        assert_eq!(code_name(-1), None);
        assert_eq!(code_text(99), "code 99");
    }

    #[test]
    fn a_host_is_found_whatever_surrounds_it() {
        for (url, host) in [
            ("https://Shop.example.com/a?b=c", Some("shop.example.com")),
            ("https://u:p@shop.example.com:8443", Some("shop.example.com")),
            ("https://shop.example.com?x=y", Some("shop.example.com")),
            ("not a url", None),
            ("https://", None),
        ] {
            assert_eq!(host_of(url).as_deref(), host, "{url}");
        }
    }

    #[test]
    fn other_targets_are_named_and_nothing_else_is_kept() {
        let pubsub: Job = serde_json::from_str(r#"{"pubsubTarget":{"topicName":"t","data":"aHVudGVy"}}"#).unwrap();
        assert_eq!((pubsub.target_kind().as_str(), pubsub.auth_kind(), pubsub.url()), ("Pub/Sub", None, None));
        let engine: Job = serde_json::from_str(r#"{"appEngineHttpTarget":{"relativeUri":"/x"}}"#).unwrap();
        assert_eq!(engine.target_kind(), "App Engine");
        let plain: Job = serde_json::from_str(r#"{"httpTarget":{"uri":"https://a.example.com"}}"#).unwrap();
        assert_eq!(
            (plain.target_kind().as_str(), plain.auth_kind(), plain.url().as_deref()),
            ("HTTP", Some("none"), Some("https://a.example.com"))
        );
        let oauth: Job =
            serde_json::from_str(r#"{"httpTarget":{"uri":"https://a","oauthToken":{"scope":"x"}}}"#).unwrap();
        assert_eq!(oauth.auth_kind(), Some("OAuth"));
    }

    #[test]
    fn one_job_and_the_locations() {
        let server = MockServer::start(vec![
            Reply::json(200, JOB),
            Reply::json(200, r#"{"locations":[{"locationId":"us-east1"},{"locationId":"europe-west1"}]}"#),
        ]);
        let gcp = client(&server);
        let job = find_in(&gcp, "us-east1", "shop nightly").unwrap();
        assert_eq!(job.id(), "shop-nightly");
        assert_eq!(locations(&gcp).unwrap(), ["us-east1", "europe-west1"]);
        let requests = server.requests();
        assert!(
            requests[0].path.starts_with(
                "/cloudscheduler/v1/projects/my-platform-123456/locations/us-east1/jobs/shop%20nightly?fields="
            ),
            "{}",
            requests[0].path
        );
        assert_eq!(requests[1].path, "/cloudscheduler/v1/projects/my-platform-123456/locations?pageSize=100");
        assert!(requests.iter().all(|r| r.method == "GET"));
    }
}
