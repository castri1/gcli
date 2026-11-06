//! Cloud Build API v1: the builds of the client's region, read-only. Nothing here starts,
//! cancels or retries a build. A build's log text lives in Cloud Logging ([`log`]).
//!
//! Only the fields gcli shows are asked for ([`FIELDS`]): a build also carries its steps'
//! arguments, environment and secrets' names, which it has no use for and never reads.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::logging::{self, Entry, quote};
use super::monitoring::rfc3339;
use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode};

/// The statuses of a build that ended without doing its work. `CANCELLED` is not one: someone
/// (or a newer build) stopped it.
pub const FAILED: [&str; 4] = ["FAILURE", "TIMEOUT", "INTERNAL_ERROR", "EXPIRED"];

/// The most the API returns in a page; asking for more silently falls back to a small default.
const MAX_PAGE: usize = 1000;

/// The fields of a build, for one build and (inside `builds(…)`) for a list.
const BUILD_FIELDS: &str = "id,status,statusDetail,createTime,startTime,finishTime,substitutions,tags,buildTriggerId,\
source,sourceProvenance,failureInfo,logUrl,steps(id,name,status,timing,exitCode)";

/// Log lines read for a build at most (a little under what Cloud Logging's client reads in a call,
/// to tell when there is more). A failing step past them is not found: [`Log::cut`] says so.
pub const LOG_LINES: usize = 9_000;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Build {
    pub id: String,
    /// `PENDING`, `QUEUED`, `WORKING`, `SUCCESS`, `FAILURE`, `INTERNAL_ERROR`, `TIMEOUT`,
    /// `CANCELLED` or `EXPIRED`.
    pub status: String,
    pub status_detail: String,
    pub create_time: String,
    pub start_time: String,
    pub finish_time: String,
    pub substitutions: BTreeMap<String, String>,
    pub tags: Vec<String>,
    pub build_trigger_id: String,
    pub source: Source,
    pub source_provenance: Provenance,
    pub failure_info: FailureInfo,
    pub steps: Vec<Step>,
    pub log_url: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Source {
    pub git_source: GitSource,
    pub repo_source: RepoSource,
}

/// Where a build's source actually came from, once resolved.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Provenance {
    pub resolved_git_source: GitSource,
    pub resolved_repo_source: RepoSource,
    pub resolved_connected_repository: ConnectedRepository,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GitSource {
    pub url: String,
    pub revision: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RepoSource {
    pub repo_name: String,
    pub branch_name: String,
    pub commit_sha: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConnectedRepository {
    pub repository: String,
    pub revision: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct FailureInfo {
    /// `USER_BUILD_STEP`, `FETCH_SOURCE_FAILED`, `PUSH_FAILED`…
    #[serde(rename = "type")]
    pub kind: String,
    pub detail: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Step {
    pub id: String,
    /// The builder's image.
    pub name: String,
    /// Steps that never ran stay `QUEUED`.
    pub status: String,
    pub timing: TimeSpan,
    pub exit_code: Option<i64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TimeSpan {
    pub start_time: String,
    pub end_time: String,
}

impl Step {
    /// Its id, or the builder's name without its registry.
    pub fn label(&self) -> &str {
        if self.id.is_empty() { super::run::last_segment(&self.name) } else { &self.id }
    }

    pub fn started(&self) -> Option<f64> {
        parse_rfc3339(&self.timing.start_time)
    }

    pub fn ended(&self) -> Option<f64> {
        parse_rfc3339(&self.timing.end_time)
    }

    pub fn failed(&self) -> bool {
        matches!(self.status.as_str(), "FAILURE" | "TIMEOUT" | "INTERNAL_ERROR")
    }

    /// How long it ran, up to `now` when still running. `None`: it never ran.
    pub fn secs(&self, now: f64) -> Option<f64> {
        let start = self.started()?;
        Some(self.ended().unwrap_or(now) - start)
    }
}

impl Build {
    pub fn created(&self) -> Option<f64> {
        parse_rfc3339(&self.create_time)
    }

    pub fn started(&self) -> Option<f64> {
        parse_rfc3339(&self.start_time)
    }

    pub fn finished(&self) -> Option<f64> {
        parse_rfc3339(&self.finish_time)
    }

    /// When it ended, or was last heard of: what its age counts from.
    pub fn at(&self) -> Option<f64> {
        self.finished().or_else(|| self.started()).or_else(|| self.created())
    }

    pub fn failed(&self) -> bool {
        FAILED.contains(&self.status.as_str())
    }

    pub fn running(&self) -> bool {
        is_running(&self.status)
    }

    /// How long it ran, up to `now` when still running. `None`: it never started.
    pub fn secs(&self, now: f64) -> Option<f64> {
        let start = self.started()?;
        Some(self.finished().unwrap_or(now) - start)
    }

    /// The first 8 characters of its id, enough to tell builds apart and to find one again.
    pub fn short_id(&self) -> &str {
        self.id.get(..8).unwrap_or(&self.id)
    }

    /// The full commit, from what triggered the build or where its source came from.
    pub fn commit(&self) -> &str {
        let (p, s) = (&self.source_provenance, &self.source);
        [
            self.substitution("COMMIT_SHA"),
            &p.resolved_repo_source.commit_sha,
            &p.resolved_git_source.revision,
            &p.resolved_connected_repository.revision,
            &s.repo_source.commit_sha,
            &s.git_source.revision,
        ]
        .into_iter()
        .find(|c| !c.is_empty())
        .unwrap_or_default()
    }

    /// The commit, short when it looks like a hash (a git source's revision can be a branch).
    pub fn short_commit(&self) -> &str {
        let commit = self.commit();
        match commit.get(..7) {
            Some(short) if commit.len() >= 40 => short,
            _ => commit,
        }
    }

    pub fn branch(&self) -> &str {
        [self.substitution("BRANCH_NAME"), &self.source.repo_source.branch_name]
            .into_iter()
            .find(|b| !b.is_empty())
            .unwrap_or_default()
    }

    /// `owner/name`, a repo's name or its URL.
    pub fn repo(&self) -> &str {
        let (p, s) = (&self.source_provenance, &self.source);
        [
            self.substitution("REPO_FULL_NAME"),
            self.substitution("REPO_NAME"),
            &p.resolved_repo_source.repo_name,
            &p.resolved_git_source.url,
            super::run::last_segment(&p.resolved_connected_repository.repository),
            &s.repo_source.repo_name,
            &s.git_source.url,
        ]
        .into_iter()
        .find(|r| !r.is_empty())
        .unwrap_or_default()
    }

    /// The trigger's name when the build has one, else its id, else empty (started by hand).
    pub fn trigger(&self) -> &str {
        let name = self.substitution("TRIGGER_NAME");
        if name.is_empty() { &self.build_trigger_id } else { name }
    }

    fn substitution(&self, key: &str) -> &str {
        self.substitutions.get(key).map_or("", String::as_str)
    }

    /// The first step that failed, with its number.
    pub fn failing_step(&self) -> Option<(usize, &Step)> {
        self.steps.iter().enumerate().find(|(_, s)| s.failed())
    }

    /// The build's page in the Cloud console: `logUrl` as Google gave it, or one made from its id.
    pub fn console_url(&self, gcp: &Gcp) -> String {
        if self.log_url.starts_with("https://") {
            return self.log_url.clone();
        }
        super::console_url(&gcp.project, &format!("cloud-build/builds;region={}/{}", location(gcp), self.id))
    }
}

pub fn is_running(status: &str) -> bool {
    matches!(status, "PENDING" | "QUEUED" | "WORKING")
}

/// Cloud Build has no "every region": a client without a region reads `global`, the only one
/// it can name (`locations/-` answers the same).
pub fn location(gcp: &Gcp) -> &str {
    if gcp.region.is_empty() { "global" } else { &gcp.region }
}

fn builds_url(gcp: &Gcp) -> String {
    format!(
        "https://cloudbuild.googleapis.com/v1/projects/{}/locations/{}/builds",
        uri_encode(&gcp.project),
        uri_encode(location(gcp))
    )
}

/// The conditions given (empty ones left out), all of which must hold.
pub fn all_of(conditions: &[&str]) -> String {
    conditions.iter().filter(|c| !c.is_empty()).copied().collect::<Vec<_>>().join(" AND ")
}

/// The filter for builds that failed.
pub fn failed_filter() -> String {
    let any: Vec<String> = FAILED.iter().map(|s| format!("status={}", quote(s))).collect();
    format!("({})", any.join(" OR "))
}

/// Pages through the builds matching `filter` (may be empty), newest first; `each` returns false
/// to stop. A page holds `page_size` builds at most.
pub fn pages(gcp: &Gcp, filter: &str, page_size: usize, each: impl FnMut(Vec<Build>) -> bool) -> Result<()> {
    let size = page_size.clamp(1, MAX_PAGE).to_string();
    let fields = format!("builds({BUILD_FIELDS}),nextPageToken");
    let mut query = vec![("pageSize", size.as_str()), ("fields", fields.as_str())];
    if !filter.is_empty() {
        query.push(("filter", filter));
    }
    gcp.pages(&builds_url(gcp), &query, "builds", Opts::default(), each)
}

/// The newest builds matching `filter`, at most `limit`.
pub fn recent(gcp: &Gcp, filter: &str, limit: usize) -> Result<Vec<Build>> {
    let mut out: Vec<Build> = Vec::new();
    pages(gcp, filter, limit, |page| {
        out.extend(page);
        out.len() < limit
    })?;
    out.truncate(limit);
    Ok(out)
}

/// The statuses of the builds matching `filter`, newest first; `each` returns false to stop.
pub fn statuses(gcp: &Gcp, filter: &str, mut each: impl FnMut(&str) -> bool) -> Result<()> {
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Status {
        status: String,
    }
    let query = [("pageSize", "50"), ("fields", "builds(status),nextPageToken"), ("filter", filter)];
    gcp.pages(&builds_url(gcp), &query, "builds", Opts::default(), |page: Vec<Status>| {
        page.iter().all(|b| each(&b.status))
    })
}

/// One build, by its full id or (searching the newest builds) by the start of it.
pub fn find(gcp: &Gcp, id: &str) -> Result<Build> {
    let id = full_id(gcp, id)?;
    let url = format!("{}/{}", builds_url(gcp), uri_encode(&id));
    gcp.get(&url, &[("fields", BUILD_FIELDS)])
}

/// A full build id, 36 characters long, from the start of one as lists show it.
fn full_id(gcp: &Gcp, start: &str) -> Result<String> {
    if start.len() >= 36 {
        return Ok(start.to_string());
    }
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct Id {
        id: String,
    }
    let size = MAX_PAGE.to_string();
    let query = [("pageSize", size.as_str()), ("fields", "builds(id),nextPageToken")];
    let mut found: Vec<String> = Vec::new();
    let mut pages_read = 0;
    gcp.pages(&builds_url(gcp), &query, "builds", Opts::default(), |page: Vec<Id>| {
        pages_read += 1;
        found.extend(page.into_iter().map(|b| b.id).filter(|id| id.starts_with(start)));
        found.is_empty() && pages_read < 3
    })?;
    match found.len() {
        0 => Err(super::GcpError::Api {
            status: 404,
            code: "NOT_FOUND".into(),
            message: format!(
                "no build starting with '{start}' among the newest of {} ({}): give its full id",
                gcp.project,
                location(gcp)
            ),
        }),
        1 => Ok(found.remove(0)),
        n => Err(super::GcpError::Api {
            status: 400,
            code: "AMBIGUOUS".into(),
            message: format!("{n} builds start with '{start}': give more of the id"),
        }),
    }
}

/// A build's log as read from Cloud Logging, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Log {
    pub lines: Vec<LogLine>,
    /// More lines exist than were read ([`LOG_LINES`]).
    pub cut: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    /// The step's number; `None` for Cloud Build's own lines (fetching source, `DONE`…).
    pub step: Option<usize>,
    /// The text, without the `Step #1 - "id": ` that Cloud Build puts in front.
    pub text: String,
}

impl LogLine {
    /// The line as the log viewer and `--log` show it. The step's id stays out: it can hold a
    /// word like "fail" that would make every line of the step look like an error.
    pub fn shown(&self) -> String {
        match self.step {
            Some(n) => format!("Step #{n}: {}", self.text),
            None => self.text.clone(),
        }
    }

    /// The lines of a log entry: usually one, though a payload can hold several.
    fn of(entry: &Entry) -> Vec<LogLine> {
        let message = entry.message();
        let label = entry.labels.get("build_step").map_or("", String::as_str);
        let step = label
            .strip_prefix("Step #")
            .and_then(|rest| rest[..rest.find(|c: char| !c.is_ascii_digit()).unwrap_or(rest.len())].parse().ok());
        let bare = |line: &str| match step {
            Some(_) => {
                let text = line.strip_prefix(label).unwrap_or(line);
                text.strip_prefix(':').unwrap_or(text).trim_start_matches(' ').to_string()
            }
            None => line.to_string(),
        };
        if message.is_empty() {
            return vec![LogLine { step, text: String::new() }];
        }
        message.lines().map(|line| LogLine { step, text: bare(line) }).collect()
    }
}

/// The filter of a build's log entries. Without a `timestamp` condition Cloud Logging searches
/// only the last 24 hours, so the build's own times bound it: a build can log a little before
/// its recorded start, and a running build has no end yet.
fn log_filter(gcp: &Gcp, build: &Build) -> String {
    let name = format!("projects/{}/logs/cloudbuild", gcp.project);
    let mut filter = format!(
        "logName={} AND resource.type=\"build\" AND resource.labels.build_id={}",
        quote(&name),
        quote(&build.id)
    );
    if let Some(from) = build.created() {
        filter.push_str(&format!(" AND timestamp>={}", quote(&rfc3339(from - 60.0))));
    }
    if let Some(to) = build.finished() {
        filter.push_str(&format!(" AND timestamp<={}", quote(&rfc3339(to + 300.0))));
    }
    filter
}

/// A build's log text, oldest first. Empty when Cloud Logging holds none of it: past its
/// retention (30 days by default), or the build logged only to Cloud Storage.
pub fn log(gcp: &Gcp, build: &Build) -> Result<Log> {
    // One more than kept, to know whether there was more.
    let mut entries = logging::entries(gcp, &log_filter(gcp, build), false, LOG_LINES + 1)?;
    let cut = entries.len() > LOG_LINES;
    entries.truncate(LOG_LINES);
    Ok(Log { lines: entries.iter().flat_map(LogLine::of).collect(), cut })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::GcpError;
    use crate::gcp::mock::{MockServer, Reply};

    /// A failed build as Cloud Build gives it: a trigger's, from a connected Git repository.
    pub(crate) const FAILED_BUILD: &str = r#"{
        "id":"1a2b3c4d-1111-2222-3333-444455556666","status":"FAILURE",
        "createTime":"2026-11-05T10:00:00.123456789Z","startTime":"2026-11-05T10:00:05Z","finishTime":"2026-11-05T10:03:05Z",
        "substitutions":{"_SERVICE":"shop","COMMIT_SHA":"0123456789abcdef0123456789abcdef01234567","BRANCH_NAME":"main",
                         "REPO_FULL_NAME":"acme/shop","TRIGGER_NAME":"shop-main-deploy","_SECRET_THING":"hunter2"},
        "tags":["trigger-77"],"buildTriggerId":"77",
        "source":{"gitSource":{"url":"https://github.com/acme/shop.git","revision":"main"}},
        "sourceProvenance":{"resolvedGitSource":{"url":"https://github.com/acme/shop.git","revision":"0123456789abcdef0123456789abcdef01234567"}},
        "failureInfo":{"type":"USER_BUILD_STEP","detail":"Build step failure: build step 1 \"gcr.io/cloud-builders/docker\" failed: step exited with non-zero status: 1"},
        "logUrl":"https://console.cloud.google.com/cloud-build/builds;region=us-east1/1a2b3c4d?project=123",
        "steps":[
          {"id":"pull","name":"gcr.io/cloud-builders/docker","status":"SUCCESS","timing":{"startTime":"2026-11-05T10:00:05Z","endTime":"2026-11-05T10:01:05Z"}},
          {"id":"build-or-fail","name":"gcr.io/cloud-builders/docker","status":"FAILURE","exitCode":1,"timing":{"startTime":"2026-11-05T10:01:05Z","endTime":"2026-11-05T10:03:05Z"}},
          {"id":"deploy","name":"gcr.io/google.com/cloudsdktool/cloud-sdk","status":"QUEUED"}
        ]}"#;

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()))
    }

    #[test]
    fn a_build_says_what_it_built_and_what_failed() {
        let b: Build = serde_json::from_str(FAILED_BUILD).unwrap();
        assert!(b.failed() && !b.running());
        assert_eq!(b.short_id(), "1a2b3c4d");
        assert_eq!(b.short_commit(), "0123456");
        assert_eq!((b.branch(), b.repo(), b.trigger()), ("main", "acme/shop", "shop-main-deploy"));
        assert_eq!(b.secs(0.0), Some(180.0));
        let (n, step) = b.failing_step().unwrap();
        assert_eq!((n, step.label(), step.exit_code), (1, "build-or-fail", Some(1)));
        assert_eq!(b.steps[0].secs(0.0), Some(60.0));
        assert_eq!(b.steps[2].secs(0.0), None, "it never ran");
        assert_eq!(b.steps[2].label(), "deploy");

        // A build started by hand: no trigger, and the rest from where its source came from.
        let hand: Build = serde_json::from_str(
            r#"{"id":"short","status":"WORKING","startTime":"2026-11-05T10:00:00Z",
                "source":{"repoSource":{"repoName":"orders","branchName":"dev"}},
                "sourceProvenance":{"resolvedRepoSource":{"repoName":"orders","commitSha":"abc1234"}},
                "steps":[{"name":"gcr.io/cloud-builders/npm","status":"WORKING","timing":{"startTime":"2026-11-05T10:00:00Z"}}]}"#,
        )
        .unwrap();
        let start = hand.started().unwrap();
        assert_eq!(hand.short_id(), "short");
        assert_eq!((hand.commit(), hand.short_commit()), ("abc1234", "abc1234"));
        assert_eq!((hand.branch(), hand.repo(), hand.trigger()), ("dev", "orders", ""));
        assert!(hand.running() && hand.failing_step().is_none());
        assert_eq!(hand.secs(start + 42.0), Some(42.0), "running: up to now");
        assert_eq!(hand.steps[0].label(), "npm");
        assert_eq!(hand.steps[0].secs(start + 7.0), Some(7.0));
    }

    #[test]
    fn lists_the_regions_builds_within_the_api_page_limit() {
        let server = MockServer::start(vec![
            Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD},{{"id":"b"}}],"nextPageToken":"next"}}"#)),
            Reply::json(200, r#"{"builds":[{"id":"c"},{"id":"d"}],"nextPageToken":"more"}"#),
        ]);
        let gcp = client(&server);
        let got = recent(&gcp, &all_of(&["", "tags=\"x\""]), 3).unwrap();
        assert_eq!(got.iter().map(|b| b.short_id()).collect::<Vec<_>>(), ["1a2b3c4d", "b", "c"]);
        let requests = server.requests();
        assert_eq!(requests.len(), 2, "stops once it has enough");
        let first = &requests[0].path;
        assert!(
            first.starts_with(
                "/cloudbuild/v1/projects/my-platform-123456/locations/us-east1/builds?pageSize=3&fields=builds%28id%2C"
            ),
            "{first}"
        );
        assert!(first.ends_with("&filter=tags%3D%22x%22"), "{first}");
        assert!(requests[1].path.contains("pageToken=next"));
        assert_eq!(requests[0].method, "GET");

        let server = MockServer::start(vec![Reply::json(200, r#"{"builds":[]}"#)]);
        recent(&client(&server), "", 5000).unwrap();
        let path = &server.requests()[0].path;
        assert!(path.contains("pageSize=1000&"), "over the page limit the API falls back to 20: {path}");
        assert!(!path.contains("filter="), "{path}");
    }

    #[test]
    fn asks_only_for_what_it_shows() {
        for secret in ["args", "secretEnv", "availableSecrets", "env"] {
            assert!(!BUILD_FIELDS.split([',', '(', ')']).any(|f| f == secret), "{secret}");
        }
        assert_eq!(
            failed_filter(),
            r#"(status="FAILURE" OR status="TIMEOUT" OR status="INTERNAL_ERROR" OR status="EXPIRED")"#
        );
    }

    #[test]
    fn a_client_without_region_reads_global() {
        let server = MockServer::start(vec![Reply::json(200, "{}")]);
        let gcp = Gcp::with("p", "", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        recent(&gcp, "", 1).unwrap();
        assert!(server.requests()[0].path.starts_with("/cloudbuild/v1/projects/p/locations/global/builds?"));
    }

    #[test]
    fn finds_a_build_by_the_start_of_its_id() {
        let full = "1a2b3c4d-1111-2222-3333-444455556666";
        let server = MockServer::start(vec![
            Reply::json(200, &format!(r#"{{"builds":[{{"id":"ffff"}},{{"id":"{full}"}}]}}"#)),
            Reply::json(200, FAILED_BUILD),
        ]);
        let b = find(&client(&server), "1a2b3c4d").unwrap();
        assert_eq!(b.id, full);
        let requests = server.requests();
        assert!(requests[0].path.contains("builds%28id%29"), "{}", requests[0].path);
        assert!(requests[1].path.starts_with(&format!(
            "/cloudbuild/v1/projects/my-platform-123456/locations/us-east1/builds/{full}?fields="
        )));

        // A full id goes straight there, and an unknown start says so.
        let server =
            MockServer::start(vec![Reply::json(200, FAILED_BUILD), Reply::json(200, r#"{"builds":[{"id":"ffff"}]}"#)]);
        find(&client(&server), full).unwrap();
        assert_eq!(server.requests().len(), 1);
        let e = find(&client(&server), "1a2b").unwrap_err();
        assert!(matches!(&e, GcpError::Api { status: 404, message, .. } if message.contains("'1a2b'")), "{e:?}");
        let server = MockServer::start(vec![Reply::json(200, r#"{"builds":[{"id":"1a2b-a"},{"id":"1a2b-b"}]}"#)]);
        let e = find(&client(&server), "1a2b").unwrap_err();
        assert!(e.short().contains("2 builds start with '1a2b'"), "{e:?}");
    }

    #[test]
    fn a_log_is_read_from_cloud_logging_within_the_builds_times() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"entries":[
                {"textPayload":"FETCHSOURCE\nstarting build\n","labels":{"build_step":"MAIN"}},
                {"textPayload":"Step #1 - \"build-or-fail\": npm ci","labels":{"build_step":"Step #1 - \"build-or-fail\""}},
                {"textPayload":"Step #1 - \"build-or-fail\":","labels":{"build_step":"Step #1 - \"build-or-fail\""}},
                {"textPayload":"Step #12: unnamed","labels":{"build_step":"Step #12"}},
                {"textPayload":"Copying gs://x","labels":{"build_step":"gsutil:STDOUT"}}
            ]}"#,
        )]);
        let gcp = client(&server);
        let build: Build = serde_json::from_str(FAILED_BUILD).unwrap();
        let log = log(&gcp, &build).unwrap();
        assert!(!log.cut);
        let shown: Vec<String> = log.lines.iter().map(LogLine::shown).collect();
        assert_eq!(
            shown,
            ["FETCHSOURCE", "starting build", "Step #1: npm ci", "Step #1: ", "Step #12: unnamed", "Copying gs://x"]
        );
        assert_eq!(log.lines[2].step, Some(1));
        assert_eq!(log.lines[5].step, None);
        let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
        assert_eq!(body["orderBy"], "timestamp asc");
        let filter = body["filter"].as_str().unwrap();
        assert_eq!(
            filter,
            "logName=\"projects/my-platform-123456/logs/cloudbuild\" AND resource.type=\"build\" \
             AND resource.labels.build_id=\"1a2b3c4d-1111-2222-3333-444455556666\" \
             AND timestamp>=\"2026-11-05T09:59:00Z\" AND timestamp<=\"2026-11-05T10:08:05Z\""
        );
    }

    #[test]
    fn a_log_too_long_says_it_was_cut() {
        let entry = r#"{"textPayload":"line","labels":{"build_step":"MAIN"}}"#;
        let page = format!(r#"{{"entries":[{}],"nextPageToken":"more"}}"#, vec![entry; 1000].join(","));
        let server = MockServer::start(vec![]);
        server.route("entries:list", Reply::json(200, &page));
        let build: Build = serde_json::from_str(FAILED_BUILD).unwrap();
        let got = log(&client(&server), &build).unwrap();
        assert!(got.cut);
        assert_eq!(got.lines.len(), LOG_LINES);
    }

    #[test]
    fn a_running_builds_log_has_no_end_and_an_id_cannot_change_the_filter() {
        let server = MockServer::start(vec![Reply::json(200, "{}")]);
        let build = Build {
            id: r#"x" OR severity>=DEBUG"#.into(),
            create_time: "2026-11-05T10:00:00Z".into(),
            ..Build::default()
        };
        let got = log(&client(&server), &build).unwrap();
        assert!(got.lines.is_empty() && !got.cut);
        let body: serde_json::Value = serde_json::from_str(&server.requests()[0].body).unwrap();
        let filter = body["filter"].as_str().unwrap();
        assert!(filter.contains(r#"build_id="x\" OR severity>=DEBUG""#), "{filter}");
        assert!(filter.contains("timestamp>=") && !filter.contains("timestamp<="), "{filter}");
    }
}
