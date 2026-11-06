//! What a job's attempts did, from the logs Cloud Scheduler writes: how many failed in a row and
//! since when, and what each one answered.
//!
//! Cloud Logging allows about 60 reads a minute per project, so the jobs in question are read
//! together, and what comes back says how far it reaches: when a read ends before a run of failed
//! attempts does, the run is "at least" that long. Log text is data, and passes through
//! `redact_tokens` before leaving for a report or an agent.

use std::collections::HashMap;

use serde_json::Value;

use crate::gcp::error::redact_tokens;
use crate::gcp::logging::{self, Entry, quote};
use crate::gcp::monitoring::rfc3339;
use crate::gcp::scheduler::Job;
use crate::gcp::{Gcp, Result};
use crate::util::{fmt_duration, fmt_utc, parse_rfc3339};

/// How far back attempts are read.
pub const WINDOW_SECS: f64 = 86_400.0;
pub const WINDOW_TEXT: &str = "last 24 h";
/// How far back a job's last success is looked for: the default retention of Cloud Logging.
const RETENTION_DAYS: f64 = 30.0;
/// Entries read for many jobs at most, three reads when pages are full. The newest come first, so
/// a run of failures that the read does not reach the start of stays "at least".
const MAX_BULK: usize = 3000;
/// Entries read for one job at most: a read, when the page is full.
const MAX_ONE_JOB: usize = 1000;
/// Jobs named in one read at most: a filter has a size limit, and this many failing jobs is news
/// enough without their history. The others count from their last attempt.
const MAX_JOBS_IN_A_READ: usize = 100;

const STARTED: &str = "type.googleapis.com/google.cloud.scheduler.logging.AttemptStarted";
const FINISHED: &str = "type.googleapis.com/google.cloud.scheduler.logging.AttemptFinished";

/// An attempt of a job.
#[derive(Debug, Clone, Default)]
pub struct Attempt {
    pub at: f64,
    /// Started, and its end is not in the logs (yet).
    pub running: bool,
    pub failed: bool,
    /// The log's status name; a success has none.
    pub status: String,
    /// The answer's HTTP status, when an answer came.
    pub http: Option<i64>,
    /// Scheduler's own words: `URL_UNREACHABLE-UNREACHABLE_5xx. Original HTTP response code number = 503`.
    pub detail: String,
    /// From its start to its end, when both were read.
    pub took: Option<f64>,
}

/// The HTTP status in Scheduler's own words. A 0 means no answer came.
fn http_in(detail: &str) -> Option<i64> {
    let (_, rest) = detail.split_once("response code number = ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().filter(|code| *code > 0)
}

fn kind(e: &Entry) -> &str {
    e.json_payload.as_ref().and_then(|p| p.get("@type")).and_then(Value::as_str).unwrap_or_default()
}

impl Attempt {
    /// A finished attempt, from its log entry.
    fn finished(e: &Entry) -> Option<Attempt> {
        if kind(e) != FINISHED {
            return None;
        }
        let payload = e.json_payload.as_ref()?;
        let text = |key: &str| payload.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let status = text("status");
        let detail = text("debugInfo");
        let http = e.http_request.as_ref().map(|r| r.status).filter(|s| *s > 0).or_else(|| http_in(&detail));
        let failed = (!status.is_empty() && status != "OK") || e.severity == "ERROR";
        Some(Attempt { at: e.at()?, running: false, failed, status, http, detail, took: None })
    }

    /// `HTTP 503`, or what Scheduler says when no answer came (`URL_ERROR-ERROR_DNS`). Log text:
    /// it comes back redacted.
    pub fn answer(&self) -> String {
        match self.http {
            Some(h) => format!("HTTP {h}"),
            None => redact_tokens(self.detail.split(". ").next().unwrap_or_default()),
        }
    }
}

/// A run of failed attempts, ending with the job's last.
#[derive(Debug, Clone)]
pub struct Streak {
    pub failed: usize,
    /// The first failed attempt read.
    pub since: f64,
    /// The read ended before the run did: it may be longer and older than this.
    pub at_least: bool,
    /// The newest attempt.
    pub last: Attempt,
}

impl Streak {
    /// The run in `attempts` (newest first), when the newest finished one failed.
    pub fn of<'a>(attempts: impl IntoIterator<Item = &'a Attempt>) -> Option<Streak> {
        let mut streak: Option<Streak> = None;
        for attempt in attempts.into_iter().filter(|a| !a.running) {
            if !attempt.failed {
                if let Some(s) = &mut streak {
                    s.at_least = false;
                }
                return streak;
            }
            let s = streak.get_or_insert_with(|| Streak {
                failed: 0,
                since: attempt.at,
                at_least: true,
                last: attempt.clone(),
            });
            s.failed += 1;
            s.since = attempt.at;
        }
        streak
    }

    /// When the logs say nothing of a job whose status says it failed: its last attempt alone.
    pub fn from_job(job: &Job) -> Option<Streak> {
        let at = job.last_attempt()?;
        Some(Streak { failed: 1, since: at, at_least: true, last: Attempt { at, failed: true, ..Attempt::default() } })
    }

    /// The run for a list, where room is short: `96+ in a row · 23h 55m+`; `+` is "at least".
    pub fn short(&self, now: f64) -> String {
        let long = fmt_duration(now - self.since);
        match (self.failed, self.at_least) {
            (1, false) => format!("first failure {long} ago"),
            (n, false) => format!("{n} in a row · {long}"),
            (n, true) => format!("{n}+ in a row · {long}+"),
        }
    }

    /// `at least 96 in a row, for at least 23h 55m`
    pub fn text(&self, now: f64) -> String {
        let long = fmt_duration(now - self.since);
        match (self.failed, self.at_least) {
            (1, false) => format!("first failure, {long} ago"),
            (n, false) => format!("{n} in a row for {long}"),
            (n, true) => format!("at least {n} in a row, for at least {long}"),
        }
    }
}

/// A job as the logs know it: its region and id.
pub type JobKey = (String, String);

pub fn key(job: &Job) -> JobKey {
    (job.location().to_string(), job.id().to_string())
}

fn entry_key(e: &Entry) -> Option<JobKey> {
    let labels = &e.resource.labels;
    Some((labels.get("location")?.clone(), labels.get("job_id")?.clone()))
}

/// The runs of failures read for many jobs at once.
#[derive(Debug, Default)]
pub struct History {
    streaks: HashMap<JobKey, Streak>,
}

impl History {
    pub fn streak(&self, key: &JobKey) -> Option<&Streak> {
        self.streaks.get(key)
    }
}

/// The conditions every read here shares.
fn base_filter(gcp: &Gcp) -> String {
    let mut filter = "resource.type=\"cloud_scheduler_job\"".to_string();
    if !gcp.region.is_empty() {
        filter.push_str(&format!(" AND resource.labels.location={}", quote(&gcp.region)));
    }
    filter
}

/// The last 24 hours of `jobs`' finished attempts, in as few reads as they fit: usually one.
pub fn read(gcp: &Gcp, jobs: &[JobKey], now: f64) -> Result<History> {
    if jobs.is_empty() {
        return Ok(History::default());
    }
    let mut ids: Vec<&str> = jobs.iter().take(MAX_JOBS_IN_A_READ).map(|(_, id)| id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let names: Vec<String> = ids.into_iter().map(quote).collect();
    let filter = format!(
        "{} AND resource.labels.job_id=({}) AND jsonPayload.\"@type\"={} AND timestamp>={}",
        base_filter(gcp),
        names.join(" OR "),
        quote(FINISHED),
        quote(&rfc3339(now - WINDOW_SECS)),
    );
    let read = logging::entries(gcp, &filter, true, MAX_BULK)?;
    let mut attempts: HashMap<JobKey, Vec<Attempt>> = HashMap::new();
    for e in &read.list {
        if let (Some(key), Some(attempt)) = (entry_key(e), Attempt::finished(e)) {
            attempts.entry(key).or_default().push(attempt);
        }
    }
    let streaks = attempts.into_iter().filter_map(|(key, list)| Some((key, Streak::of(&list)?))).collect();
    Ok(History { streaks })
}

/// A job's recent attempts, newest first.
#[derive(Debug, Clone, Default)]
pub struct Attempts {
    pub list: Vec<Attempt>,
    /// Google has more: an older part of the window was not read.
    pub cut: bool,
}

/// When an entry happened: an attempt's start counts from when it was due, because its log entry
/// can come after the end of a quick attempt.
fn event_time(e: &Entry) -> Option<f64> {
    let due = e.json_payload.as_ref().and_then(|p| p.get("scheduledTime")).and_then(Value::as_str);
    due.and_then(parse_rfc3339).filter(|_| kind(e) == STARTED).or_else(|| e.at())
}

/// Puts each attempt's start and end together, whatever order they were logged in. Attempts of a
/// job do not overlap, so an end belongs to the start before it. Newest first.
fn pair(entries: &[Entry]) -> Vec<Attempt> {
    let mut events: Vec<(f64, &Entry)> = entries.iter().filter_map(|e| Some((event_time(e)?, e))).collect();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut out = Vec::new();
    let mut open: Option<f64> = None;
    for (at, e) in events {
        if kind(e) == STARTED {
            open = Some(at);
        } else if let Some(mut attempt) = Attempt::finished(e) {
            if let Some(start) = open.take() {
                attempt.took = Some((attempt.at - start).max(0.0));
                attempt.at = start;
            }
            out.push(attempt);
        }
    }
    if let Some(at) = open {
        out.push(Attempt { at, running: true, ..Attempt::default() });
    }
    out.reverse();
    out
}

/// `job`'s attempts of the last 24 hours, with how long each took.
pub fn attempts(gcp: &Gcp, job: &Job, now: f64) -> Result<Attempts> {
    let filter = format!(
        "resource.type=\"cloud_scheduler_job\" AND resource.labels.location={} AND resource.labels.job_id={} AND timestamp>={}",
        quote(job.location()),
        quote(job.id()),
        quote(&rfc3339(now - WINDOW_SECS)),
    );
    let read = logging::entries(gcp, &filter, true, MAX_ONE_JOB)?;
    Ok(Attempts { list: pair(&read.list), cut: read.more })
}

/// When a job last worked, as far back as its logs go.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LastSuccess {
    At(f64),
    /// None in what Cloud Logging keeps.
    None,
    /// Google stopped searching before finding one.
    Unknown,
}

impl LastSuccess {
    pub fn text(self, now: f64) -> String {
        match self {
            LastSuccess::At(t) => format!("{} · {}", fmt_utc(t), super::relative(t, now)),
            LastSuccess::None => format!("none in the logs of the last {RETENTION_DAYS} days"),
            LastSuccess::Unknown => "the logs did not say".into(),
        }
    }
}

/// The last success of `job` before `before` (the window read already). A success is a 2xx answer,
/// and `httpRequest.status` is indexed, so this usually takes one read; on a month of logs Google
/// may stop at its time limit and hand back a token, which is followed a few times.
pub fn last_success(gcp: &Gcp, job: &Job, before: f64, now: f64) -> Result<LastSuccess> {
    let filter = format!(
        "resource.type=\"cloud_scheduler_job\" AND logName={} AND resource.labels.location={} AND resource.labels.job_id={} AND httpRequest.status>=200 AND httpRequest.status<300 AND timestamp>={} AND timestamp<{}",
        quote(&format!("projects/{}/logs/cloudscheduler.googleapis.com%2Fexecutions", gcp.project)),
        quote(job.location()),
        quote(job.id()),
        quote(&rfc3339(now - RETENTION_DAYS * 86_400.0)),
        quote(&rfc3339(before)),
    );
    let read = logging::entries(gcp, &filter, true, 1)?;
    Ok(match read.list.first().and_then(Entry::at) {
        Some(t) => LastSuccess::At(t),
        None if read.more => LastSuccess::Unknown,
        None => LastSuccess::None,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};
    use serde_json::json;

    /// A finished attempt's log entry, as Cloud Scheduler writes them.
    pub(crate) fn finished(job: &str, at: &str, http: i64) -> Value {
        let mut payload = json!({
            "@type": FINISHED,
            "jobName": format!("projects/p/locations/us-east1/jobs/{job}"),
            "targetType": "HTTP",
            "url": "https://x/y?key=hunter2",
            "debugInfo": format!("URL_UNREACHABLE-UNREACHABLE_5xx. Original HTTP response code number = {http}"),
        });
        if http >= 400 {
            let status = match http {
                401 => "UNAUTHENTICATED",
                503 => "UNAVAILABLE",
                504 => "DEADLINE_EXCEEDED",
                _ => "INTERNAL",
            };
            payload["status"] = json!(status);
        }
        json!({
            "insertId": at,
            "timestamp": at,
            "severity": if http >= 400 { "ERROR" } else { "INFO" },
            "jsonPayload": payload,
            "httpRequest": { "status": http },
            "resource": { "type": "cloud_scheduler_job", "labels": { "location": "us-east1", "job_id": job } },
        })
    }

    pub(crate) fn started(job: &str, at: &str) -> Value {
        json!({
            "insertId": at,
            "timestamp": at,
            "severity": "INFO",
            "jsonPayload": { "@type": STARTED, "scheduledTime": at },
            "resource": { "type": "cloud_scheduler_job", "labels": { "location": "us-east1", "job_id": job } },
        })
    }

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()))
    }

    fn logged(entries: Value) -> Reply {
        Reply::json(200, &json!({ "entries": entries }).to_string())
    }

    fn at(text: &str) -> f64 {
        parse_rfc3339(text).unwrap()
    }

    fn key_of(id: &str) -> JobKey {
        ("us-east1".to_string(), id.to_string())
    }

    #[test]
    fn many_jobs_are_read_together_and_a_run_ends_at_a_success() {
        let fake = MockServer::start(vec![logged(json!([
            finished("shop-nightly", "2026-11-05T12:00:00Z", 503),
            finished("orders-sync", "2026-11-05T11:50:00Z", 504),
            finished("shop-nightly", "2026-11-05T11:00:00Z", 503),
            finished("orders-sync", "2026-11-05T10:50:00Z", 200),
            finished("orders-sync", "2026-11-05T09:50:00Z", 503),
            finished("shop-nightly", "2026-11-05T10:00:00Z", 500),
        ]))]);
        let now = at("2026-11-05T12:30:00Z");
        let jobs = [key_of("shop-nightly"), key_of("orders-sync")];
        let history = read(&client(&fake), &jobs, now).unwrap();

        let shop = history.streak(&key_of("shop-nightly")).unwrap();
        assert_eq!((shop.failed, shop.at_least, shop.since), (3, true, at("2026-11-05T10:00:00Z")));
        assert_eq!((shop.last.http, shop.last.status.as_str()), (Some(503), "UNAVAILABLE"));
        assert_eq!(shop.text(now), "at least 3 in a row, for at least 2h 30m");
        assert_eq!(shop.short(now), "3+ in a row · 2h 30m+");

        let orders = history.streak(&key_of("orders-sync")).unwrap();
        assert_eq!((orders.failed, orders.at_least), (1, false), "the success before it ends the run");
        assert_eq!(orders.text(now), "first failure, 40m 00s ago");
        assert_eq!(orders.short(now), "first failure 40m 00s ago");

        // One read, with what the jobs need and no more.
        let requests = fake.requests();
        assert_eq!(requests.len(), 1, "one read");
        let body: Value = serde_json::from_str(&requests[0].body).unwrap();
        let filter = body["filter"].as_str().unwrap();
        assert!(
            filter.starts_with("resource.type=\"cloud_scheduler_job\" AND resource.labels.location=\"us-east1\""),
            "{filter}"
        );
        assert!(filter.contains(r#"resource.labels.job_id=("orders-sync" OR "shop-nightly")"#), "{filter}");
        assert!(
            filter.contains("AttemptFinished") && filter.contains("timestamp>=\"2026-11-04T12:30:00Z\""),
            "{filter}"
        );
        assert_eq!(body["orderBy"], "timestamp desc");
    }

    #[test]
    fn no_job_in_question_reads_nothing() {
        let fake = MockServer::start(vec![]);
        assert!(read(&client(&fake), &[], 0.0).unwrap().streaks.is_empty());
        assert!(fake.requests().is_empty());
    }

    #[test]
    fn a_read_that_stops_early_is_an_at_least() {
        let mut page = json!({ "entries": [finished("shop-nightly", "2026-11-05T12:00:00Z", 503)] });
        page["nextPageToken"] = json!("");
        let fake = MockServer::start(vec![Reply::json(200, &page.to_string())]);
        let history = read(&client(&fake), &[key_of("shop-nightly")], at("2026-11-05T12:30:00Z")).unwrap();
        let s = history.streak(&key_of("shop-nightly")).unwrap();
        assert_eq!((s.failed, s.at_least), (1, true));
        assert_eq!(s.text(at("2026-11-05T12:30:00Z")), "at least 1 in a row, for at least 30m 00s");
        // Newest success first: no run at all.
        let fine = [
            Attempt { failed: false, at: 2.0, ..Attempt::default() },
            Attempt { failed: true, at: 1.0, ..Attempt::default() },
        ];
        assert!(Streak::of(&fine).is_none());
        // A run with a success under it, of more than one: exact.
        let ended = [
            Attempt { failed: true, at: 3.0, ..Attempt::default() },
            Attempt { failed: true, at: 2.0, ..Attempt::default() },
            Attempt { failed: false, at: 1.0, ..Attempt::default() },
        ];
        let s = Streak::of(&ended).unwrap();
        assert_eq!((s.failed, s.at_least, s.since), (2, false, 2.0));
        assert_eq!(s.text(62.0), "2 in a row for 1m 00s");
        assert_eq!(s.short(62.0), "2 in a row · 1m 00s");
    }

    #[test]
    fn an_attempt_says_how_it_ended() {
        let entry = |v: Value| serde_json::from_value::<Entry>(v).unwrap();
        let ok = Attempt::finished(&entry(finished("a", "2026-11-05T12:00:00Z", 200))).unwrap();
        assert!(!ok.failed && ok.status.is_empty());
        assert_eq!(ok.answer(), "HTTP 200");
        let mut no_http = finished("a", "2026-11-05T12:00:00Z", 503);
        no_http.as_object_mut().unwrap().remove("httpRequest");
        let got = Attempt::finished(&entry(no_http)).unwrap();
        assert_eq!(got.http, Some(503), "from Scheduler's words");
        let nothing = json!({
            "timestamp": "2026-11-05T12:00:00Z", "severity": "ERROR",
            "jsonPayload": { "@type": FINISHED, "status": "UNAVAILABLE", "debugInfo": "URL_UNREACHABLE-UNREACHABLE_NO_RESPONSE. Original HTTP response code number = 0" },
        });
        let got = Attempt::finished(&entry(nothing)).unwrap();
        assert!(got.failed);
        assert_eq!((got.http, got.answer().as_str()), (None, "URL_UNREACHABLE-UNREACHABLE_NO_RESPONSE"));
        assert_eq!(http_in("nothing of the kind"), None);
    }

    #[test]
    fn one_jobs_attempts_with_how_long_each_took() {
        let job: Job = serde_json::from_str(crate::gcp::scheduler::tests::JOB).unwrap();
        let fake = MockServer::start(vec![logged(json!([
            started("shop-nightly", "2026-11-05T13:00:00Z"),
            finished("shop-nightly", "2026-11-05T12:05:00Z", 504),
            started("shop-nightly", "2026-11-05T12:00:00Z"),
            finished("shop-nightly", "2026-11-05T11:00:30Z", 200),
            started("shop-nightly", "2026-11-05T11:00:00Z"),
            finished("shop-nightly", "2026-11-05T10:00:30Z", 503),
        ]))]);
        let read = attempts(&client(&fake), &job, at("2026-11-05T13:00:10Z")).unwrap();
        assert!(!read.cut);
        let list = &read.list;
        assert_eq!(list.len(), 4);
        assert!(list[0].running);
        assert_eq!((list[1].took, list[1].http, list[1].failed), (Some(300.0), Some(504), true));
        assert_eq!(list[1].at, at("2026-11-05T12:00:00Z"), "when it started, as its schedule says");
        assert_eq!((list[2].took, list[2].failed), (Some(30.0), false));
        assert_eq!(list[3].took, None, "its start is before the window");
        let s = Streak::of(list).unwrap();
        assert_eq!((s.failed, s.at_least, s.last.http), (1, false, Some(504)), "a running attempt does not count");
        let body: Value = serde_json::from_str(&fake.requests()[0].body).unwrap();
        assert!(body["filter"].as_str().unwrap().contains("resource.labels.job_id=\"shop-nightly\""), "{body}");
    }

    #[test]
    fn a_quick_attempts_start_can_be_logged_after_its_end() {
        let job: Job = serde_json::from_str(crate::gcp::scheduler::tests::JOB).unwrap();
        let mut late = started("shop-nightly", "2026-11-05T13:30:07Z");
        late["jsonPayload"]["scheduledTime"] = json!("2026-11-05T13:30:01Z");
        let fake =
            MockServer::start(vec![logged(json!([late, finished("shop-nightly", "2026-11-05T13:30:05Z", 401)]))]);
        let read = attempts(&client(&fake), &job, at("2026-11-05T14:00:00Z")).unwrap();
        assert_eq!(read.list.len(), 1, "{:?}", read.list);
        assert!(!read.list[0].running);
        assert_eq!(read.list[0].took, Some(4.0));
    }

    #[test]
    fn the_last_success_is_followed_a_little_and_says_when_it_cannot_tell() {
        let job: Job = serde_json::from_str(crate::gcp::scheduler::tests::JOB).unwrap();
        let now = at("2026-11-05T13:00:00Z");
        let before = now - WINDOW_SECS;
        let fake = MockServer::start(vec![
            // Google stops at its time limit once, then finds it.
            Reply::json(200, r#"{"nextPageToken":"go-on"}"#),
            logged(json!([finished("shop-nightly", "2026-11-01T13:00:00Z", 200)])),
            Reply::json(200, "{}"),
        ]);
        // Then it never gets there.
        for _ in 0..10 {
            fake.push(Reply::json(200, r#"{"nextPageToken":"gave-up"}"#));
        }
        let gcp = client(&fake);
        let found = last_success(&gcp, &job, before, now).unwrap();
        assert_eq!(found, LastSuccess::At(at("2026-11-01T13:00:00Z")));
        assert_eq!(found.text(now), "2026-11-01 13:00 UTC · 4d ago");
        let none = last_success(&gcp, &job, before, now).unwrap();
        assert_eq!(none, LastSuccess::None);
        assert_eq!(none.text(now), "none in the logs of the last 30 days");
        let unknown = last_success(&gcp, &job, before, now).unwrap();
        assert_eq!(unknown, LastSuccess::Unknown);
        assert!(unknown.text(now).contains("did not say"));
        let requests = fake.requests();
        assert_eq!(requests.len(), 13, "as many as it takes, up to a limit");
        let body: Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["pageSize"], 1);
        let filter = body["filter"].as_str().unwrap();
        assert!(filter.contains("httpRequest.status>=200 AND httpRequest.status<300"), "{filter}");
        assert!(filter.contains("logName=\"projects/my-platform-123456/logs/cloudscheduler"), "{filter}");
        assert!(filter.contains("timestamp<\"2026-11-04T13:00:00Z\""), "{filter}");
    }

    #[test]
    fn a_job_with_no_log_counts_from_its_last_attempt() {
        let job: Job = serde_json::from_str(crate::gcp::scheduler::tests::JOB).unwrap();
        let s = Streak::from_job(&job).unwrap();
        assert_eq!((s.failed, s.at_least, s.last.http), (1, true, None));
        assert_eq!(s.since, at("2026-11-05T03:00:00Z"));
        assert!(Streak::from_job(&Job::default()).is_none());
    }
}
