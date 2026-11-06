//! Crons: the environment's Cloud Scheduler jobs. Which ones failed, since when, and most likely
//! why; which are late, paused, never ran, or call a Cloud Run service that is gone. The terminal
//! UI, the commands and the MCP server all read jobs through here.
//!
//! Read-only: nothing here runs, pauses, resumes, changes or deletes a job.

pub mod cmd;
pub mod history;
pub mod report;

use std::collections::HashSet;

use serde_json::{Map, Value, json};

use crate::apps::{Column, Tone};
use crate::builds::{is_service_name, read_all};
use crate::config::AppsConvention;
use crate::findings::{self, Finding, Level};
use crate::gcp::error::redact_tokens;
use crate::gcp::monitoring::rfc3339;
use crate::gcp::run::{self, Service};
use crate::gcp::scheduler::{self, Job, Outcome, code_text};
use crate::gcp::{Gcp, GcpError, Result};
use crate::util::{fmt_duration, fmt_utc};
use history::{Attempts, History, JobKey, LastSuccess, Streak, WINDOW_SECS};

/// What a negative status code means, as far as anyone can tell.
pub const NEGATIVE_CODE_EXPLAINED: &str = "A negative status code is not a google.rpc code. Every job seen with -1 had no last attempt and no entry in its logs, so gcli reads it as no attempt yet; after an attempt, gcli cannot tell what it means.";

/// How late a job must be, on top of its attempt deadline, to count as overdue.
const OVERDUE_GRACE_SECS: f64 = 900.0;
/// How long a job may exist before never having run is worth saying.
const NEVER_RAN_AFTER_SECS: f64 = 86_400.0;
/// An attempt deadline when a job has none: Cloud Scheduler's default.
const DEFAULT_DEADLINE_SECS: f64 = 180.0;

// ---- which app a job belongs to ------------------------------------------------------------------

/// What is known of the environment's Cloud Run services, to tell what a job calls.
#[derive(Debug, Clone, Default)]
pub struct Known {
    services: Vec<KnownService>,
    /// What follows a service's name in its own URLs (`-123456.us-east1.run.app`): a host with one
    /// of these and an unknown name is a service of this project and region that is gone. A host
    /// with none may be another project's, so nothing can be said of it.
    suffixes: HashSet<String>,
    /// The services were read: without them, no job can be said to call nothing.
    read: bool,
}

#[derive(Debug, Clone)]
struct KnownService {
    id: String,
    hosts: Vec<String>,
    timeout: f64,
}

fn is_run_host(host: &str) -> bool {
    host.ends_with(".run.app")
}

impl Known {
    pub fn new(services: &[Service]) -> Known {
        let mut suffixes = HashSet::new();
        let services = services
            .iter()
            .map(|s| {
                let urls = s.urls.iter().chain(std::iter::once(&s.uri));
                let hosts: Vec<String> = urls.filter_map(|u| scheduler::host_of(u)).collect();
                for host in &hosts {
                    if let Some(suffix) = host.strip_prefix(s.id()).filter(|_| is_run_host(host)) {
                        suffixes.insert(suffix.to_string());
                    }
                }
                KnownService { id: s.id().to_string(), hosts, timeout: s.timeout_secs() }
            })
            .collect();
        Known { services, suffixes, read: true }
    }

    fn by_host(&self, host: &str) -> Option<&KnownService> {
        self.services.iter().find(|s| s.hosts.iter().any(|h| h == host))
    }

    fn by_id(&self, id: &str) -> Option<&KnownService> {
        self.services.iter().find(|s| s.id == id)
    }

    /// The name of the service `host` would be, when it has this environment's form of URL.
    fn missing_in(&self, host: &str) -> Option<String> {
        self.suffixes.iter().find_map(|suffix| host.strip_suffix(suffix.as_str())).map(str::to_string)
    }

    /// The service whose name, with a dash, starts `job`; the longest when several do.
    fn by_job_name(&self, job: &str) -> Option<&KnownService> {
        self.services
            .iter()
            .filter(|s| job.strip_prefix(s.id.as_str()).is_some_and(|rest| rest.starts_with('-')))
            .max_by_key(|s| s.id.len())
    }
}

/// What a job calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A Cloud Run service of the environment: the job's app.
    App(String),
    /// A Cloud Run service that does not exist any more.
    Gone(String),
    /// Nothing says.
    Unknown,
}

impl AppsConvention {
    /// What `job` calls. A host that is a service's own URL says it under any convention. On the
    /// whiteghost platform, also `<slug>.<domain>` (the load balancer's host for an app) and a job
    /// named `<slug>-<name>`.
    pub fn job_target(self, job: &Job, known: &Known) -> Target {
        if !known.read {
            return Target::Unknown;
        }
        let host = job.host();
        if let Some(host) = &host {
            if let Some(service) = known.by_host(host) {
                return Target::App(service.id.clone());
            }
            if let Some(name) = known.missing_in(host).filter(|n| is_service_name(n)) {
                return Target::Gone(name);
            }
            if is_run_host(host) {
                return Target::Unknown;
            }
        }
        match self {
            AppsConvention::Run => Target::Unknown,
            AppsConvention::Whiteghost => {
                let id = job.id();
                if let Some(label) = host.as_deref().and_then(|h| h.split('.').next()) {
                    if known.by_id(label).is_some() {
                        return Target::App(label.to_string());
                    }
                    // Both the job's name and its host name the same app, and it has no service.
                    let named = id.strip_prefix(label).is_some_and(|rest| rest.starts_with('-'));
                    if named && is_service_name(label) {
                        return Target::Gone(label.to_string());
                    }
                }
                known.by_job_name(id).map_or(Target::Unknown, |s| Target::App(s.id.clone()))
            }
        }
    }
}

// ---- a job, and what it finds ----------------------------------------------------------------------

/// A job with what the environment says of it.
#[derive(Debug, Clone)]
pub struct Row {
    pub job: Job,
    pub target: Target,
    /// The target service's request timeout, in seconds.
    pub timeout: Option<f64>,
    /// The run of failures that the job's last attempt ends, once the logs were read.
    pub streak: Option<Streak>,
}

fn secs_text(secs: f64) -> String {
    format!("{secs:.0}s")
}

impl Row {
    pub fn new(job: Job, convention: AppsConvention, known: &Known) -> Row {
        let target = convention.job_target(&job, known);
        let timeout = match &target {
            Target::App(app) => known.by_id(app).map(|s| s.timeout),
            _ => None,
        };
        Row { job, target, timeout, streak: None }
    }

    /// The app it belongs to, or belonged to.
    pub fn app(&self) -> Option<&str> {
        match &self.target {
            Target::App(app) | Target::Gone(app) => Some(app),
            Target::Unknown => None,
        }
    }

    /// The service to read logs of: one that exists.
    pub fn live_app(&self) -> Option<&str> {
        match &self.target {
            Target::App(app) => Some(app),
            _ => None,
        }
    }

    fn failed_code(&self) -> Option<i64> {
        match self.job.outcome() {
            Outcome::Failed(code) => Some(code),
            _ => None,
        }
    }

    fn http(&self) -> Option<i64> {
        self.streak.as_ref()?.last.http
    }

    /// What the logs say the last attempt answered, when it is more than the job's own code does.
    fn log_status(&self) -> Option<&str> {
        let status = self.streak.as_ref()?.last.status.as_str();
        (!status.is_empty() && self.failed_code().is_none_or(|c| code_text(c) != status)).then_some(status)
    }

    /// `UNKNOWN (the log says UNAUTHENTICATED) · HTTP 401`, or Scheduler's words with no HTTP status.
    fn failure_text(&self, code: i64) -> String {
        let mut text = code_text(code);
        if let Some(status) = self.log_status() {
            text.push_str(&format!(" (the log says {status})"));
        }
        let answer = self.streak.as_ref().map(|s| s.last.answer()).unwrap_or_default();
        if !answer.is_empty() {
            text.push_str(&format!(" · {answer}"));
        }
        text
    }

    /// The overdue time, past what a slow attempt explains.
    fn late_by(&self, now: f64) -> Option<f64> {
        let next = self.job.next_run().filter(|_| self.job.enabled())?;
        let allowed = OVERDUE_GRACE_SECS + self.job.deadline_secs().unwrap_or(DEFAULT_DEADLINE_SECS);
        (now - next > allowed).then_some(now - next)
    }

    /// Most likely why the job fails, from what it answered and how it calls its target.
    pub fn likely_cause(&self) -> Option<String> {
        let code = self.failed_code()?;
        let http = self.http();
        let refused = matches!(http, Some(401 | 403)) || (http.is_none() && matches!(code, 7 | 16));
        let redirected = http.is_some_and(|h| (300..400).contains(&h));
        if refused || redirected {
            let host = self.job.host().filter(|h| !is_run_host(h));
            return Some(match host {
                Some(host) => format!(
                    "It calls {host}, not the service's own Cloud Run URL: an identity-aware proxy in front of it may be turning the job away, whatever token it sends."
                ),
                None if redirected => "The target answers with a redirect instead of doing the work: check the URL.".into(),
                None => "The job's identity was refused: check that its service account may call the target, and the token's audience.".into(),
            });
        }
        let deadline = http == Some(504)
            || code == 4
            || self.streak.as_ref().is_some_and(|s| s.last.status == "DEADLINE_EXCEEDED");
        if deadline {
            return Some(self.deadline_cause());
        }
        Some(match http {
            Some(404) => "The app has no such path: the job and the app's routes disagree.".to_string(),
            Some(405) => {
                let method = self.job.target_kind();
                format!("The app does not accept {method} on this path: the job and the app's routes disagree.")
            }
            Some(429) => "The app or something in front of it is limiting requests.".to_string(),
            Some(h @ 400..=499) => format!("The app refused the request (HTTP {h})."),
            Some(500) => "The app's handler fails: its logs say why.".to_string(),
            Some(502 | 503) => {
                "The service, or something it depends on, did not answer: it may be crashing, failing to start or out of capacity. Its logs say which."
                    .to_string()
            }
            Some(h @ 500..=599) => format!("The target answered HTTP {h}: its logs say why."),
            _ => return None,
        })
    }

    fn deadline_cause(&self) -> String {
        match (self.job.deadline_secs(), self.timeout) {
            (Some(deadline), Some(timeout)) if deadline < timeout => format!(
                "The attempt deadline ({}) is shorter than the service's request timeout ({}): the job gives up first, and the request may go on running.",
                secs_text(deadline),
                secs_text(timeout)
            ),
            (Some(deadline), Some(timeout)) if timeout < deadline => format!(
                "The service's request timeout ({}) is shorter than the attempt deadline ({}): Cloud Run cuts the request first, so a longer deadline would not help. The work takes longer than the timeout.",
                secs_text(timeout),
                secs_text(deadline)
            ),
            _ => "The work takes longer than it is allowed.".to_string(),
        }
    }

    /// What is wrong with this job, the most telling first.
    pub fn findings(&self, now: f64) -> Vec<Finding> {
        let id = self.job.id();
        let outcome = self.job.outcome();
        if !self.job.enabled() && !self.job.state.is_empty() {
            let state =
                if self.job.state == "PAUSED" { "paused".to_string() } else { format!("state {}", self.job.state) };
            let last = match outcome {
                Outcome::Failed(code) => format!(", and its last attempt failed: {}", code_text(code)),
                Outcome::NotYet => ", and never attempted".to_string(),
                _ => String::new(),
            };
            return vec![Finding::warn(id, format!("{state}{last}"))];
        }
        let mut found = Vec::new();
        if let Target::Gone(service) = &self.target {
            let mut f = Finding::bad(id, format!("orphan: Cloud Run service {service} does not exist any more"));
            if let Some(code) = self.failed_code() {
                f = f.hint(format!("Every attempt fails with it: {}.", self.failure_text(code)));
            }
            found.push(f);
        } else {
            match outcome {
                Outcome::Failed(code) => {
                    let how_long = match &self.streak {
                        Some(s) => s.text(now),
                        None => {
                            self.job.last_attempt().map_or(String::new(), |t| format!("{} ago", fmt_duration(now - t)))
                        }
                    };
                    let mut what = format!("last attempt failed: {}", self.failure_text(code));
                    if !how_long.is_empty() {
                        what.push_str(&format!(" · {how_long}"));
                    }
                    if let Some(app) = self.app() {
                        what.push_str(&format!(" · app {app}"));
                    }
                    let mut f = Finding::bad(id, what);
                    if let Some(cause) = self.likely_cause() {
                        f = f.hint(cause);
                    }
                    found.push(f);
                }
                Outcome::Unclear(code) => {
                    found.push(
                        Finding::warn(id, format!("status code {code} after an attempt")).hint(NEGATIVE_CODE_EXPLAINED),
                    );
                }
                Outcome::NotYet => {
                    if let Some(existed) = self.job.updated().map(|t| now - t).filter(|s| *s > NEVER_RAN_AFTER_SECS) {
                        let mut f = Finding::warn(
                            id,
                            format!("never attempted, {} after its last change", fmt_duration(existed)),
                        );
                        if let Some(next) = self.job.next_run() {
                            f = f.hint(format!(
                                "Its next run is {}: a schedule that comes round rarely may just not have come up yet.",
                                fmt_utc(next)
                            ));
                        }
                        found.push(f);
                    }
                }
                Outcome::Succeeded => {}
            }
        }
        if let Some(late) = self.late_by(now) {
            found.push(
                Finding::bad(id, format!("overdue: its next run was due {} ago", fmt_duration(late)))
                    .hint("It is enabled and no attempt has started: Cloud Scheduler is not running it."),
            );
        }
        found
    }

    /// The worst of its findings.
    pub fn level(&self, now: f64) -> Option<Level> {
        self.findings(now).iter().map(|f| f.level).max()
    }

    /// As `--json` and the MCP server list it: small, as a project can have over a hundred.
    pub fn json(&self, now: f64) -> Value {
        let job = &self.job;
        let mut v = Map::new();
        v.insert("job".into(), json!(job.id()));
        v.insert("app".into(), json!(self.app()));
        v.insert("schedule".into(), json!(job.schedule));
        v.insert("timeZone".into(), json!(job.time_zone));
        v.insert("state".into(), json!(job.state));
        v.insert("result".into(), json!(job.outcome().text()));
        let some = |text: &str| Some(text.to_string()).filter(|t| !t.is_empty());
        v.insert("lastRun".into(), json!(some(&job.last_attempt_time)));
        v.insert("nextRun".into(), json!(some(&job.schedule_time)));
        if let Some(level) = self.level(now) {
            v.insert("level".into(), json!(level));
        }
        if matches!(self.target, Target::Gone(_)) {
            v.insert("orphan".into(), json!(true));
        }
        if let Some(late) = self.late_by(now) {
            v.insert("overdueSeconds".into(), json!(late.round()));
        }
        if self.failed_code().is_some() {
            v.insert("failure".into(), self.failure_json());
        }
        if let Some(code) = job.status.code.filter(|c| *c < 0) {
            v.insert("statusCode".into(), json!(code));
        }
        Value::Object(v)
    }

    /// The failure as far as it is known. Log text passes through `redact_tokens`.
    fn failure_json(&self) -> Value {
        let job = &self.job;
        let mut v = Map::new();
        if let Some(code) = job.status.code {
            v.insert("code".into(), json!(code));
            v.insert("codeName".into(), json!(code_text(code)));
        }
        if !job.status.message.is_empty() {
            v.insert("message".into(), json!(redact_tokens(&job.status.message)));
        }
        if let Some(s) = &self.streak {
            v.insert("attemptsInARow".into(), json!(s.failed));
            v.insert("atLeast".into(), json!(s.at_least));
            v.insert("failingSince".into(), json!(rfc3339(s.since)));
            if let Some(http) = s.last.http {
                v.insert("httpStatus".into(), json!(http));
            }
            if !s.last.status.is_empty() {
                v.insert("logStatus".into(), json!(s.last.status));
            }
            if !s.last.detail.is_empty() {
                v.insert("error".into(), json!(redact_tokens(&s.last.detail)));
            }
        }
        Value::Object(v)
    }

    /// What it calls and how, for a job's detail and the agents.
    pub fn target_json(&self) -> Value {
        let job = &self.job;
        let mut v = Map::new();
        v.insert("kind".into(), json!(job.target_kind()));
        if let Some(url) = job.url() {
            v.insert("url".into(), json!(redact_tokens(&url)));
        }
        if let Some(auth) = job.auth_kind() {
            v.insert("auth".into(), json!(auth));
        }
        if let Some(deadline) = job.deadline_secs() {
            v.insert("attemptDeadlineSeconds".into(), json!(deadline));
        }
        if let Some(timeout) = self.timeout {
            v.insert("serviceTimeoutSeconds".into(), json!(timeout));
        }
        Value::Object(v)
    }
}

// ---- the list ---------------------------------------------------------------------------------------

/// `5m ago`, `in 3h`; the coarse time a list shows.
pub fn relative(t: f64, now: f64) -> String {
    let span = (t - now).abs() as u64;
    let short = match span {
        0..=59 => format!("{span}s"),
        60..=3599 => format!("{}m", span / 60),
        3600..=86_399 => format!("{}h", span / 3600),
        _ => format!("{}d", span / 86_400),
    };
    if t > now { format!("in {short}") } else { format!("{short} ago") }
}

/// A job's mark and result, and how it should look.
pub fn result(job: &Job) -> (String, Tone) {
    let outcome = job.outcome();
    if job.state == "PAUSED" {
        return ("· paused".to_string(), Tone::Warn);
    }
    match outcome {
        Outcome::Succeeded => ("✓ ok".to_string(), Tone::Plain),
        Outcome::NotYet => ("· no attempt yet".to_string(), Tone::Muted),
        Outcome::Failed(_) => (format!("✗ {}", outcome.text()), Tone::Bad),
        Outcome::Unclear(_) => (format!("? {}", outcome.text()), Tone::Warn),
    }
}

/// The schedule, with its time zone unless UTC.
pub fn schedule_text(job: &Job) -> String {
    if job.time_zone.is_empty() || matches!(job.time_zone.as_str(), "Etc/UTC" | "UTC") {
        job.schedule.clone()
    } else {
        format!("{} ({})", job.schedule, job.time_zone)
    }
}

/// The list's columns.
pub fn columns() -> Vec<Column> {
    let col = |head, right, drop| Column { head, right, drop };
    vec![
        col("RESULT", false, 0),
        col("JOB", false, 0),
        col("APP", false, 2),
        col("SCHEDULE", false, 4),
        col("LAST RUN", true, 1),
        col("NEXT RUN", true, 3),
        col("NOTES", false, 0),
    ]
}

impl Row {
    /// What the list says of it, in a few words.
    fn notes(&self, now: f64) -> (String, Tone) {
        if let Target::Gone(service) = &self.target {
            return (format!("orphan: service {service} is gone"), Tone::Bad);
        }
        let mut notes: Vec<String> = Vec::new();
        if let Some(s) = self.streak.as_ref().filter(|_| self.failed_code().is_some()) {
            if !s.last.answer().is_empty() {
                notes.push(s.last.answer());
            }
            notes.push(s.short(now));
        }
        if let Some(late) = self.late_by(now) {
            notes.push(format!("overdue by {}", fmt_duration(late)));
        }
        if !notes.is_empty() {
            return (notes.join(" · "), Tone::Bad);
        }
        match self.findings(now).first() {
            Some(f) if f.level == Level::Warn => (f.what.clone(), Tone::Warn),
            _ => (String::new(), Tone::Muted),
        }
    }
}

/// A row's cells, in [`columns`]' order.
pub fn cells(row: &Row, now: f64) -> Vec<(String, Tone)> {
    let job = &row.job;
    let when = |t: Option<f64>| t.map(|t| relative(t, now)).unwrap_or_else(|| "—".into());
    let app = row.app().unwrap_or("—").to_string();
    let app_tone = match row.target {
        Target::App(_) => Tone::Plain,
        Target::Gone(_) => Tone::Bad,
        Target::Unknown => Tone::Muted,
    };
    vec![
        result(job),
        (job.id().to_string(), Tone::Plain),
        (app, app_tone),
        (schedule_text(job), Tone::Muted),
        (when(job.last_attempt()), Tone::Muted),
        (when(job.next_run()), Tone::Muted),
        row.notes(now),
    ]
}

/// The environment's jobs and what could be read about them.
#[derive(Debug, Default)]
pub struct Listing {
    /// The jobs in trouble first, then by name.
    pub rows: Vec<Row>,
    /// Without the services the list still says what ran; no job can be called an orphan.
    pub services_error: Option<GcpError>,
    /// Without the logs the list says which failed, not since when.
    pub history_error: Option<GcpError>,
    /// The attempts were read, or could not be.
    pub history_read: bool,
}

fn lost() -> GcpError {
    GcpError::Network("a read failed unexpectedly".into())
}

/// Every job of the environment's region, or of every region it has.
pub fn read_jobs(gcp: &Gcp) -> Result<Vec<Job>> {
    if !gcp.region.is_empty() {
        return scheduler::jobs_in(gcp, &gcp.region);
    }
    let locations = scheduler::locations(gcp)?;
    let mut all = Vec::new();
    for read in read_all(&locations, |l: &String| scheduler::jobs_in(gcp, l)) {
        all.extend(read?);
    }
    Ok(all)
}

impl Listing {
    /// The jobs and the services, at the same time. The attempts come after: [`Self::read_history`].
    pub fn read(gcp: &Gcp, convention: AppsConvention, now: f64) -> Result<Listing> {
        let (jobs, services) = std::thread::scope(|s| {
            let services = s.spawn(|| run::all_services(gcp));
            let jobs = read_jobs(gcp);
            (jobs, services.join().unwrap_or_else(|_| Err(lost())))
        });
        let jobs = jobs?;
        let (known, services_error) = match services {
            Ok(services) => (Known::new(&services), None),
            Err(e) if e.is_login_required() => return Err(e),
            Err(e) => (Known::default(), Some(e)),
        };
        let mut rows: Vec<Row> = jobs.into_iter().map(|j| Row::new(j, convention, &known)).collect();
        rows.sort_by_cached_key(|r| (std::cmp::Reverse(r.level(now)), r.job.id().to_string()));
        Ok(Listing { rows, services_error, ..Listing::default() })
    }

    /// The jobs whose attempts the list wants: those whose last attempt failed.
    pub fn in_question(&self) -> Vec<JobKey> {
        self.rows.iter().filter(|r| r.failed_code().is_some()).map(|r| history::key(&r.job)).collect()
    }

    /// Puts what the logs said on each job in question.
    pub fn apply_history(&mut self, read: Result<History>) {
        self.history_read = true;
        let history = match read {
            Ok(history) => history,
            Err(e) => {
                self.history_error = Some(e);
                return;
            }
        };
        for row in self.rows.iter_mut().filter(|r| r.failed_code().is_some()) {
            let found = history.streak(&history::key(&row.job)).cloned();
            row.streak = found.or_else(|| Streak::from_job(&row.job));
        }
    }

    /// The jobs, the services and the attempts of the jobs in question.
    pub fn load(gcp: &Gcp, convention: AppsConvention, now: f64) -> Result<Listing> {
        let mut listing = Listing::read(gcp, convention, now)?;
        let history = history::read(gcp, &listing.in_question(), now);
        if let Some(e) = history.as_ref().err().filter(|e| e.is_login_required()) {
            return Err(e.clone());
        }
        listing.apply_history(history);
        Ok(listing)
    }

    /// What every job finds, the worst first.
    pub fn findings(&self, now: f64) -> Vec<Finding> {
        findings(&self.rows, now)
    }

    /// The findings of a check. Services that could not be read are an error, as what could not
    /// be read is not a clean bill of health; the logs only add detail to what the jobs say.
    pub fn check(&self, now: f64) -> Result<Vec<Finding>> {
        match &self.services_error {
            Some(e) => Err(e.clone()),
            None => Ok(self.findings(now)),
        }
    }

    /// Jobs with something that needs attention.
    pub fn failing(&self, now: f64) -> usize {
        self.rows.iter().filter(|r| r.level(now) == Some(Level::Bad)).count()
    }

    /// What `gcli crons --check` and `cron_failures` answer.
    pub fn failures_json(&self, now: f64) -> Value {
        let found = self.findings(now);
        let failing: Vec<Value> = self
            .rows
            .iter()
            .filter(|r| r.level(now) == Some(Level::Bad))
            .map(|r| {
                let mut v = r.json(now);
                v["target"] = r.target_json();
                v
            })
            .collect();
        let mut out = json!({
            "jobs": self.rows.len(),
            "failing": failing.len(),
            "window": history::WINDOW_TEXT,
            "findings": found,
            "failingJobs": failing,
        });
        self.add_errors(&mut out);
        out
    }

    /// What could not be read.
    pub fn add_errors(&self, out: &mut Value) {
        if let Some(e) = &self.services_error {
            out["servicesError"] = json!(e.short());
        }
        if let Some(e) = &self.history_error {
            out["historyError"] = json!(e.short());
        }
    }
}

/// What `rows` find, the worst first.
pub fn findings(rows: &[Row], now: f64) -> Vec<Finding> {
    let mut found: Vec<Finding> = rows.iter().flat_map(|r| r.findings(now)).collect();
    findings::sort(&mut found);
    found
}

// ---- one job -----------------------------------------------------------------------------------------

/// A job with what its detail shows.
#[derive(Debug, Clone)]
pub struct Detail {
    pub row: Row,
    /// Its attempts of the last 24 hours.
    pub attempts: Result<Attempts>,
    /// When it last worked, read when none of those attempts did.
    pub last_success: Option<LastSuccess>,
    pub services_error: Option<GcpError>,
}

/// The job `id`: read directly in the client's region, or found among every region's.
fn find_job(gcp: &Gcp, id: &str) -> Result<Job> {
    if !gcp.region.is_empty() {
        return scheduler::find_in(gcp, &gcp.region, id);
    }
    read_jobs(gcp)?.into_iter().find(|j| j.id() == id).ok_or_else(|| GcpError::Api {
        status: 404,
        code: "NOT_FOUND".into(),
        message: format!("no Cloud Scheduler job '{id}' in {}", gcp.project),
    })
}

impl Detail {
    pub fn load(gcp: &Gcp, convention: AppsConvention, id: &str, now: f64) -> Result<Detail> {
        let (job, services, attempts) = std::thread::scope(|s| {
            let services = s.spawn(|| run::all_services(gcp));
            let job = find_job(gcp, id);
            let attempts = job.as_ref().ok().map(|j| history::attempts(gcp, j, now));
            (job, services.join().unwrap_or_else(|_| Err(lost())), attempts)
        });
        let job = job?;
        let (known, services_error) = match services {
            Ok(services) => (Known::new(&services), None),
            Err(e) if e.is_login_required() => return Err(e),
            Err(e) => (Known::default(), Some(e)),
        };
        let attempts = attempts.unwrap_or_else(|| Ok(Attempts::default()));
        let mut row = Row::new(job, convention, &known);
        let mut last_success = None;
        if row.failed_code().is_some() {
            if let Ok(read) = &attempts {
                row.streak = Streak::of(&read.list);
                // No success in what was read: its logs can tell when the last one was.
                if read.list.iter().all(|a| a.failed || a.running) {
                    let oldest = read.list.last().filter(|_| read.cut).map(|a| a.at);
                    let before = oldest.unwrap_or(now - WINDOW_SECS);
                    let read = history::last_success(gcp, &row.job, before, now);
                    last_success = Some(read.unwrap_or(LastSuccess::Unknown));
                }
            }
            if row.streak.is_none() {
                row.streak = Streak::from_job(&row.job);
            }
        }
        Ok(Detail { row, attempts, last_success, services_error })
    }

    pub fn json(&self, now: f64) -> Value {
        let row = &self.row;
        let job = &row.job;
        let mut v = row.json(now);
        v["location"] = json!(job.location());
        v["target"] = row.target_json();
        if !job.description.is_empty() {
            v["description"] = json!(redact_tokens(&job.description));
        }
        if !job.user_update_time.is_empty() {
            v["lastChanged"] = json!(job.user_update_time);
        }
        if let Some(retry) = &job.retry_config {
            v["retryCount"] = json!(retry.retry_count);
        }
        if let Some(code) = job.status.code.filter(|c| *c < 0) {
            v["statusCodeMeans"] = json!(format!("{code}: {NEGATIVE_CODE_EXPLAINED}"));
        }
        v["findings"] = json!(row.findings(now));
        match &self.attempts {
            Ok(read) => {
                v["window"] = json!(history::WINDOW_TEXT);
                v["attempts"] = json!(read.list.iter().map(attempt_json).collect::<Vec<_>>());
                v["attemptsCut"] = json!(read.cut);
            }
            Err(e) => v["attemptsError"] = json!(e.short()),
        }
        match self.last_success {
            Some(LastSuccess::At(t)) => v["lastSuccess"] = json!(rfc3339(t)),
            Some(other) => v["lastSuccessNote"] = json!(other.text(now)),
            None => {}
        }
        if let Some(e) = &self.services_error {
            v["servicesError"] = json!(e.short());
        }
        v
    }
}

fn attempt_json(a: &history::Attempt) -> Value {
    let mut v = Map::new();
    v.insert("time".into(), json!(rfc3339(a.at)));
    v.insert("result".into(), json!(attempt_result(a)));
    if let Some(http) = a.http {
        v.insert("httpStatus".into(), json!(http));
    }
    if let Some(took) = a.took {
        v.insert("seconds".into(), json!(took.round()));
    }
    if !a.detail.is_empty() && a.failed {
        v.insert("error".into(), json!(redact_tokens(&a.detail)));
    }
    Value::Object(v)
}

/// How an attempt ended, in a word.
pub fn attempt_result(a: &history::Attempt) -> String {
    if a.running {
        "running".into()
    } else if !a.failed {
        "ok".into()
    } else if a.status.is_empty() {
        "failed".into()
    } else {
        a.status.clone()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::crons::history::Attempt;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::util::parse_rfc3339;

    pub(crate) const PROJECT: &str = "my-platform-123456";
    pub(crate) const NOW: &str = "2026-11-05T12:30:00Z";

    pub(crate) fn now() -> f64 {
        parse_rfc3339(NOW).unwrap()
    }

    /// A Cloud Run service as the API lists it.
    pub(crate) fn service(name: &str, timeout: &str) -> String {
        format!(
            r#"{{"name":"projects/{PROJECT}/locations/us-east1/services/{name}","uri":"https://{name}-abc123-ue.a.run.app",
                 "urls":["https://{name}-123456789.us-east1.run.app","https://{name}-abc123-ue.a.run.app"],
                 "template":{{"timeout":"{timeout}"}}}}"#
        )
    }

    /// A job as the API lists it, `extra` being JSON members to add or override with.
    pub(crate) fn job(id: &str, url: &str, extra: &str) -> String {
        format!(
            r#"{{"name":"projects/{PROJECT}/locations/us-east1/jobs/{id}","schedule":"*/15 * * * *","timeZone":"Etc/UTC",
                 "state":"ENABLED","attemptDeadline":"300s","userUpdateTime":"2026-01-01T00:00:00Z",
                 "lastAttemptTime":"2026-11-05T12:15:00Z","scheduleTime":"2026-11-05T12:45:00Z",
                 "httpTarget":{{"uri":"{url}","httpMethod":"POST","oidcToken":{{}}}}{extra}}}"#
        )
    }

    /// A later member overrides an earlier one, as `extra` means to; a null leaves it out, as
    /// the client does with what Google sends.
    fn parsed(text: &str) -> Job {
        let mut v: Value = serde_json::from_str(text).unwrap();
        v.as_object_mut().unwrap().retain(|_, member| !member.is_null());
        serde_json::from_value(v).unwrap()
    }

    fn known() -> Known {
        let services: Vec<Service> =
            ["shop", "shop-api", "orders"].iter().map(|n| serde_json::from_str(&service(n, "600s")).unwrap()).collect();
        Known::new(&services)
    }

    const DIRECT: &str = "https://shop-123456789.us-east1.run.app/cron/x";
    const BALANCED: &str = "https://shop.apps.example.com/cron/x";

    fn row(id: &str, url: &str, extra: &str, convention: AppsConvention) -> Row {
        Row::new(parsed(&job(id, url, extra)), convention, &known())
    }

    /// A row of a job that failed, its last attempt answering `http`.
    fn failed(url: &str, code: i64, http: i64, status: &str) -> Row {
        let mut r = row("shop-nightly", url, &format!(r#","status":{{"code":{code}}}"#), AppsConvention::Whiteghost);
        r.streak = Some(Streak {
            failed: 4,
            since: now() - 3600.0,
            at_least: true,
            last: Attempt {
                at: now() - 900.0,
                failed: true,
                status: status.into(),
                http: Some(http),
                ..Attempt::default()
            },
        });
        r
    }

    fn one(r: &Row) -> Finding {
        let mut found = r.findings(now());
        assert_eq!(found.len(), 1, "{found:?}");
        found.remove(0)
    }

    #[test]
    fn a_host_names_the_app_under_any_convention() {
        for convention in [AppsConvention::Run, AppsConvention::Whiteghost] {
            for host in ["shop-123456789.us-east1.run.app", "shop-abc123-ue.a.run.app"] {
                let r = row("nightly", &format!("https://{host}/cron/x"), "", convention);
                assert_eq!(r.target, Target::App("shop".into()), "{host}");
                assert_eq!((r.app(), r.live_app(), r.timeout), (Some("shop"), Some("shop"), Some(600.0)));
            }
            // A service of this project and region that is not there any more, in both URL forms.
            for host in ["gone-123456789.us-east1.run.app", "gone-abc123-ue.a.run.app"] {
                let r = row("nightly", &format!("https://{host}/cron/x"), "", convention);
                assert_eq!(r.target, Target::Gone("gone".into()), "{host}");
                assert_eq!((r.app(), r.live_app(), r.timeout), (Some("gone"), None, None));
            }
            // Another project's or another region's: nothing can be said, so it is no orphan.
            for host in ["gone-999.us-east1.run.app", "gone-123456789.europe-west1.run.app"] {
                let r = row("shop-nightly", &format!("https://{host}/cron/x"), "", convention);
                assert_eq!(r.target, Target::Unknown, "{host}");
            }
        }
    }

    #[test]
    fn the_whiteghost_convention_also_reads_the_host_of_the_load_balancer_and_the_jobs_name() {
        let w = AppsConvention::Whiteghost;
        assert_eq!(row("shop-nightly", BALANCED, "", w).target, Target::App("shop".into()));
        assert_eq!(row("any", BALANCED, "", w).target, Target::App("shop".into()));
        // The longest name that fits.
        let external = "https://api.example.com/x";
        assert_eq!(row("shop-api-sync", external, "", w).target, Target::App("shop-api".into()));
        assert_eq!(row("shop-sync", external, "", w).target, Target::App("shop".into()));
        assert_eq!(row("shopping-sync", external, "", w).target, Target::Unknown, "a name, not a prefix of one");
        // Both the job and the host name an app with no service.
        let r = row("gone-sync", "https://gone.apps.example.com/x", "", w);
        assert_eq!(r.target, Target::Gone("gone".into()));
        // A host alone says nothing: it may be anyone's.
        assert_eq!(row("sync", "https://gone.apps.example.com/x", "", w).target, Target::Unknown);
        // Under plain Cloud Run conventions a host that isn't a service's says nothing.
        assert_eq!(row("shop-nightly", BALANCED, "", AppsConvention::Run).target, Target::Unknown);
        // Not an HTTP target.
        let pubsub =
            parsed(r#"{"name":"projects/p/locations/us-east1/jobs/orders-sync","pubsubTarget":{"topicName":"t"}}"#);
        assert_eq!(w.job_target(&pubsub, &known()), Target::App("orders".into()));
        assert_eq!(AppsConvention::Run.job_target(&pubsub, &known()), Target::Unknown);
        // Without the services read, nothing is called gone.
        let unread = parsed(&job("gone-sync", "https://gone-123456789.us-east1.run.app/x", ""));
        assert_eq!(w.job_target(&unread, &Known::default()), Target::Unknown);
        // A name no service could have is not taken from a URL.
        let r = row("x", "https://a_b-123456789.us-east1.run.app/x", "", w);
        assert_eq!(r.target, Target::Unknown);
    }

    #[test]
    fn a_failed_last_attempt_is_bad_and_says_the_code_how_long_the_status_and_the_app() {
        let f = one(&failed(DIRECT, 14, 503, "UNAVAILABLE"));
        assert_eq!(f.level, Level::Bad);
        assert_eq!(f.subject, "shop-nightly");
        assert_eq!(
            f.what,
            "last attempt failed: UNAVAILABLE · HTTP 503 · at least 4 in a row, for at least 1h 00m · app shop"
        );
        assert!(f.hint.unwrap().contains("did not answer"));
        // Before the logs are read, or without them.
        let mut r = failed(DIRECT, 14, 503, "UNAVAILABLE");
        r.streak = None;
        assert_eq!(one(&r).what, "last attempt failed: UNAVAILABLE · 15m 00s ago · app shop");
        // An app no one can name is left out.
        let mut r = failed("https://elsewhere.example.com/x", 13, 500, "INTERNAL");
        r.target = Target::Unknown;
        assert!(!one(&r).what.contains("app "), "{}", one(&r).what);
        assert!(one(&r).hint.unwrap().contains("its logs say why"));
    }

    #[test]
    fn a_code_and_a_log_that_differ_are_both_shown() {
        let f = one(&failed(DIRECT, 2, 401, "UNAUTHENTICATED"));
        assert!(f.what.contains("UNKNOWN (the log says UNAUTHENTICATED) · HTTP 401"), "{}", f.what);
    }

    #[test]
    fn with_no_http_status_the_logs_own_words_are_shown() {
        let mut r = failed(DIRECT, 14, 0, "UNAVAILABLE");
        let last = &mut r.streak.as_mut().unwrap().last;
        last.http = None;
        last.detail = "URL_ERROR-ERROR_DNS token=hunter2. Original HTTP response code number = 0".into();
        let what = one(&r).what;
        assert!(what.contains("UNAVAILABLE · URL_ERROR-ERROR_DNS token=[redacted] · at least 4"), "{what}");
        assert!(cells(&r, now())[6].0.starts_with("URL_ERROR-ERROR_DNS token=[redacted] · 4+ in a row"));
        assert_eq!(
            r.json(now())["failure"]["error"],
            "URL_ERROR-ERROR_DNS token=[redacted] Original HTTP response code number = 0"
        );
    }

    #[test]
    fn being_overdue_is_bad_past_the_grace_and_the_deadline() {
        let at = |secs_ago: f64| {
            let t = rfc3339(now() - secs_ago);
            row("shop-nightly", DIRECT, &format!(r#","scheduleTime":"{t}""#), AppsConvention::Whiteghost)
        };
        // 15 minutes plus the 300 s deadline: 1200 s.
        assert!(at(1190.0).findings(now()).is_empty());
        let f = one(&at(1210.0));
        assert_eq!(f.level, Level::Bad);
        assert_eq!(f.what, "overdue: its next run was due 20m 10s ago");
        assert_eq!(at(1210.0).json(now())["overdueSeconds"], 1210.0);
        assert!(cells(&at(1210.0), now())[6].0.contains("overdue by 20m 10s"));
        // Without a schedule time, nothing can be said.
        let mut r = at(5000.0);
        r.job.schedule_time.clear();
        assert!(r.findings(now()).is_empty());
        // A job failing and overdue says both.
        let mut both = failed(DIRECT, 14, 503, "UNAVAILABLE");
        both.job.schedule_time = rfc3339(now() - 5000.0);
        assert_eq!(both.findings(now()).len(), 2);
    }

    #[test]
    fn a_target_that_is_gone_is_an_orphan() {
        let gone = "https://gone-123456789.us-east1.run.app/x";
        let f = one(&row("gone-sync", gone, "", AppsConvention::Whiteghost));
        assert_eq!(f.level, Level::Bad);
        assert_eq!(f.what, "orphan: Cloud Run service gone does not exist any more");
        // Its failures are no separate news.
        let r = row("gone-sync", gone, r#","status":{"code":5}"#, AppsConvention::Whiteghost);
        let f = one(&r);
        assert!(f.what.starts_with("orphan") && f.hint.unwrap().contains("NOT_FOUND"));
        assert_eq!(r.json(now())["orphan"], true);
        assert_eq!(cells(&r, now())[2], ("gone".to_string(), Tone::Bad));
        assert_eq!(cells(&r, now())[6].0, "orphan: service gone is gone");
    }

    #[test]
    fn a_paused_job_is_a_warning_whatever_its_last_attempt_did() {
        let f = one(&row("shop-nightly", DIRECT, r#","state":"PAUSED""#, AppsConvention::Whiteghost));
        assert_eq!((f.level, f.what.as_str()), (Level::Warn, "paused"));
        let r = row("shop-nightly", DIRECT, r#","state":"PAUSED","status":{"code":14}"#, AppsConvention::Whiteghost);
        assert_eq!(one(&r).what, "paused, and its last attempt failed: UNAVAILABLE");
        assert_eq!(one(&r).level, Level::Warn);
        assert_eq!(result(&r.job), ("· paused".to_string(), Tone::Warn));
        // Paused long ago, with no next run: not late, and only said once.
        let r = row(
            "shop-nightly",
            DIRECT,
            r#","state":"PAUSED","status":{"code":-1},"lastAttemptTime":null,"scheduleTime":"2026-01-01T00:00:00Z""#,
            AppsConvention::Whiteghost,
        );
        assert_eq!(one(&r).what, "paused, and never attempted");
        let r = row("shop-nightly", DIRECT, r#","state":"UPDATE_FAILED""#, AppsConvention::Whiteghost);
        assert_eq!(one(&r).what, "state UPDATE_FAILED");
    }

    #[test]
    fn never_attempted_is_a_warning_once_the_job_is_over_a_day_old() {
        let never = |updated: &str| {
            let extra = format!(
                r#","status":{{"code":-1}},"lastAttemptTime":null,"userUpdateTime":"{updated}","scheduleTime":"2026-11-09T08:30:00Z""#
            );
            row("shop-weekly", DIRECT, &extra, AppsConvention::Whiteghost)
        };
        let f = one(&never("2026-11-03T12:30:00Z"));
        assert_eq!(f.level, Level::Warn);
        assert_eq!(f.what, "never attempted, 48h 00m after its last change");
        let hint = f.hint.unwrap();
        assert!(hint.contains("2026-11-09 08:30 UTC") && hint.contains("may just not have come up"), "{hint}");
        // A new job has had no time.
        assert!(never("2026-11-05T00:00:00Z").findings(now()).is_empty());
        // -1 and no attempt is no failure, and says so.
        let r = never("2026-11-03T12:30:00Z");
        assert_eq!(r.job.outcome(), Outcome::NotYet);
        assert_eq!(result(&r.job), ("· no attempt yet".to_string(), Tone::Muted));
        assert_eq!(r.json(now())["statusCode"], -1);
        assert_eq!(r.json(now())["result"], "no attempt yet");
    }

    #[test]
    fn a_negative_code_after_an_attempt_is_said_plainly() {
        let r = row("shop-nightly", DIRECT, r#","status":{"code":-1}"#, AppsConvention::Whiteghost);
        let f = one(&r);
        assert_eq!((f.level, f.what.as_str()), (Level::Warn, "status code -1 after an attempt"));
        assert!(f.hint.unwrap().contains("cannot tell what it means"));
        assert_eq!(result(&r.job), ("? code -1".to_string(), Tone::Warn));
    }

    #[test]
    fn a_healthy_job_finds_nothing() {
        let r = row("shop-nightly", DIRECT, r#","status":{}"#, AppsConvention::Whiteghost);
        assert!(r.findings(now()).is_empty());
        assert_eq!(r.level(now()), None);
        assert!(r.likely_cause().is_none());
        assert_eq!(cells(&r, now())[6].0, "");
        assert!(r.json(now()).get("level").is_none() && r.json(now()).get("failure").is_none());
    }

    #[test]
    fn a_deadline_shorter_than_the_services_timeout_is_mentioned() {
        // The deadline is 300 s and the service's timeout 600 s.
        let hint = one(&failed(DIRECT, 4, 504, "DEADLINE_EXCEEDED")).hint.unwrap();
        assert!(
            hint.contains("attempt deadline (300s) is shorter than the service's request timeout (600s)"),
            "{hint}"
        );
        // The other way round: the deadline is not what cuts it.
        let mut r = failed(DIRECT, 4, 504, "DEADLINE_EXCEEDED");
        r.timeout = Some(60.0);
        let hint = one(&r).hint.unwrap();
        assert!(hint.contains("request timeout (60s) is shorter than the attempt deadline (300s)"), "{hint}");
        // Either one unknown, or equal: only what is known.
        r.timeout = None;
        assert_eq!(one(&r).hint.unwrap(), "The work takes longer than it is allowed.");
        r.timeout = Some(300.0);
        assert_eq!(one(&r).hint.unwrap(), "The work takes longer than it is allowed.");
        // Only a job that hit the deadline.
        let hint = one(&failed(DIRECT, 14, 503, "UNAVAILABLE")).hint.unwrap();
        assert!(!hint.contains("deadline"), "{hint}");
    }

    #[test]
    fn a_load_balancer_that_turns_a_job_away_is_mentioned() {
        for (code, http, status) in [(2, 401, "UNAUTHENTICATED"), (7, 403, "PERMISSION_DENIED"), (2, 302, "UNKNOWN")] {
            let hint = one(&failed(BALANCED, code, http, status)).hint.unwrap();
            assert!(hint.contains("shop.apps.example.com") && hint.contains("identity-aware proxy"), "{http}: {hint}");
            // Straight to the Cloud Run URL, no proxy is in the way.
            let hint = one(&failed(DIRECT, code, http, status)).hint.unwrap();
            assert!(!hint.contains("identity-aware proxy"), "{http}: {hint}");
        }
        assert!(one(&failed(DIRECT, 16, 401, "UNAUTHENTICATED")).hint.unwrap().contains("service account"));
        assert!(one(&failed(DIRECT, 2, 302, "UNKNOWN")).hint.unwrap().contains("redirect"));
        // Not for any other failure through the load balancer.
        let hint = one(&failed(BALANCED, 13, 500, "INTERNAL")).hint.unwrap();
        assert!(!hint.contains("identity-aware proxy"), "{hint}");
    }

    #[test]
    fn other_answers_have_their_likely_cause() {
        for (code, http, expected) in [
            (5, 404, "no such path"),
            (9, 405, "does not accept POST"),
            (8, 429, "limiting requests"),
            (9, 400, "refused the request (HTTP 400)"),
            (13, 500, "handler fails"),
            (13, 502, "did not answer"),
            (14, 503, "did not answer"),
            (13, 599, "HTTP 599"),
        ] {
            let hint = one(&failed(DIRECT, code, http, "X")).hint.unwrap();
            assert!(hint.contains(expected), "{http}: {hint}");
        }
        // With no answer, the code alone may do; otherwise it says nothing.
        let mut r = failed(DIRECT, 7, 0, "");
        r.streak = None;
        assert!(one(&r).hint.unwrap().contains("refused"));
        let mut r = failed(DIRECT, 13, 0, "");
        r.streak = None;
        assert!(one(&r).hint.is_none());
    }

    #[test]
    fn times_read_either_way() {
        assert_eq!(relative(now() - 300.0, now()), "5m ago");
        assert_eq!(relative(now() + 7200.0, now()), "in 2h");
        assert_eq!(relative(now() - 30.0, now()), "30s ago");
        assert_eq!(relative(now() + 3.0 * 86_400.0, now()), "in 3d");
        let job = parsed(&job("a", DIRECT, r#","timeZone":"America/Bogota""#));
        assert_eq!(schedule_text(&job), "*/15 * * * * (America/Bogota)");
        assert_eq!(schedule_text(&Job { schedule: "x".into(), time_zone: "Etc/UTC".into(), ..Job::default() }), "x");
    }

    fn fake(jobs: &[String]) -> MockServer {
        let fake = MockServer::start(vec![]);
        fake.route("/jobs", Reply::json(200, &format!(r#"{{"jobs":[{}]}}"#, jobs.join(","))));
        fake.route(
            "/services",
            Reply::json(200, &format!(r#"{{"services":[{},{}]}}"#, service("shop", "300s"), service("orders", "300s"))),
        );
        fake
    }

    fn client(fake: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with(PROJECT, "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()))
    }

    #[test]
    fn a_listing_puts_the_jobs_in_trouble_first_and_reads_only_their_attempts() {
        let fake = fake(&[
            job("a-healthy", DIRECT, r#","status":{}"#),
            job("orders-sync", "https://orders-123456789.us-east1.run.app/x", r#","status":{"code":14}"#),
            job("b-paused", DIRECT, r#","state":"PAUSED""#),
            job("shop-nightly", DIRECT, r#","status":{"code":13}"#),
        ]);
        fake.route(
            "entries:list",
            Reply::json(
                200,
                &json!({ "entries": [
                    history::tests::finished("orders-sync", "2026-11-05T12:15:00Z", 503),
                    history::tests::finished("orders-sync", "2026-11-05T12:00:00Z", 503),
                    history::tests::finished("orders-sync", "2026-11-05T11:45:00Z", 200),
                ] })
                .to_string(),
            ),
        );
        let listing = Listing::load(&client(&fake), AppsConvention::Whiteghost, now()).unwrap();
        assert_eq!(
            listing.rows.iter().map(|r| r.job.id()).collect::<Vec<_>>(),
            ["orders-sync", "shop-nightly", "b-paused", "a-healthy"]
        );
        assert_eq!(listing.failing(now()), 2);
        assert!(listing.history_read && listing.history_error.is_none());
        // What the logs said, on the one they spoke of; the other counts from its last attempt.
        let orders = &listing.rows[0];
        assert_eq!(orders.streak.as_ref().map(|s| (s.failed, s.at_least)), Some((2, false)));
        let shop = &listing.rows[1];
        assert_eq!(shop.streak.as_ref().map(|s| (s.failed, s.at_least, s.last.http)), Some((1, true, None)));
        let shown = cells(orders, now());
        assert_eq!(shown.len(), columns().len());
        assert_eq!(shown[0], ("✗ UNAVAILABLE".to_string(), Tone::Bad));
        assert_eq!(shown[1].0, "orders-sync");
        assert_eq!(shown[2].0, "orders");
        assert_eq!(shown[3].0, "*/15 * * * *");
        assert_eq!(shown[4].0, "15m ago");
        assert_eq!(shown[5].0, "in 15m");
        assert_eq!(shown[6], ("HTTP 503 · 2 in a row · 30m 00s".to_string(), Tone::Bad));
        // The findings, the worst first.
        let found = listing.findings(now());
        assert_eq!(
            found.iter().map(|f| (f.subject.as_str(), f.level)).collect::<Vec<_>>(),
            [("orders-sync", Level::Bad), ("shop-nightly", Level::Bad), ("b-paused", Level::Warn)]
        );
        // One log read, only of the jobs in question. Nothing here changes anything.
        let requests = fake.requests();
        let logs: Vec<_> = requests.iter().filter(|r| r.path.ends_with("entries:list")).collect();
        assert_eq!(logs.len(), 1);
        let filter = serde_json::from_str::<Value>(&logs[0].body).unwrap()["filter"].as_str().unwrap().to_string();
        assert!(filter.contains(r#"job_id=("orders-sync" OR "shop-nightly")"#), "{filter}");
        assert!(requests.iter().all(|r| r.method == "GET" || r.path.ends_with("entries:list")), "it only reads");
        // The job's own secrets are nowhere in what it gives.
        let all = listing.failures_json(now()).to_string();
        assert!(!all.contains("hunter2"), "{all}");
    }

    #[test]
    fn nothing_failing_reads_no_logs() {
        let fake = fake(&[job("shop-nightly", DIRECT, r#","status":{}"#)]);
        let listing = Listing::load(&client(&fake), AppsConvention::Whiteghost, now()).unwrap();
        assert_eq!(listing.failing(now()), 0);
        assert!(!fake.requests().iter().any(|r| r.path.contains("entries:list")));
    }

    #[test]
    fn logs_that_cannot_be_read_leave_the_findings_and_say_so() {
        let fake = fake(&[job("shop-nightly", DIRECT, r#","status":{"code":14}"#)]);
        fake.route(
            "entries:list",
            Reply::json(403, r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no logs"}}"#),
        );
        let gcp = client(&fake);
        let listing = Listing::load(&gcp, AppsConvention::Whiteghost, now()).unwrap();
        assert!(listing.history_error.is_some());
        assert!(listing.rows[0].streak.is_none());
        assert_eq!(listing.failures_json(now())["historyError"], "PERMISSION_DENIED: no logs");
        let found = listing.check(now()).unwrap();
        assert_eq!(found[0].what, "last attempt failed: UNAVAILABLE · 15m 00s ago · app shop");
    }

    #[test]
    fn a_read_that_fails_is_not_a_clean_bill_of_health() {
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no"}}"#;
        let fake = MockServer::start(vec![]);
        fake.route("/jobs", Reply::json(403, denied));
        fake.route("/services", Reply::json(200, "{}"));
        assert!(Listing::load(&client(&fake), AppsConvention::Run, now()).is_err());
        // The services unread: the list still goes, but no check can say all is well.
        let fake = MockServer::start(vec![]);
        fake.route("/jobs", Reply::json(200, &format!(r#"{{"jobs":[{}]}}"#, job("a", DIRECT, ""))));
        fake.route("/services", Reply::json(403, denied));
        let gcp = client(&fake);
        let listing = Listing::load(&gcp, AppsConvention::Whiteghost, now()).unwrap();
        assert!(listing.services_error.is_some());
        assert_eq!(listing.rows[0].target, Target::Unknown);
        assert!(listing.check(now()).is_err());
    }

    #[test]
    fn an_environment_with_no_region_reads_every_location() {
        let fake = MockServer::start(vec![]);
        fake.route("locations/us-east1/jobs", Reply::json(200, &format!(r#"{{"jobs":[{}]}}"#, job("a", DIRECT, ""))));
        fake.route("locations/europe-west1/jobs", Reply::json(200, r#"{"jobs":[]}"#));
        fake.route(
            "/locations?",
            Reply::json(200, r#"{"locations":[{"locationId":"us-east1"},{"locationId":"europe-west1"}]}"#),
        );
        let gcp = Gcp::with(PROJECT, "", crate::gcp::auth::tests::fixed("t"), Some(fake.url()));
        assert_eq!(read_jobs(&gcp).unwrap().len(), 1);
        assert_eq!(find_job(&gcp, "a").unwrap().id(), "a");
        assert!(matches!(find_job(&gcp, "b"), Err(GcpError::Api { status: 404, .. })));
    }

    /// What a job's detail reads, with a run of failures that goes past the window.
    pub(crate) fn fake_detail() -> MockServer {
        let fake = MockServer::start(vec![]);
        fake.route(
            "/jobs/shop-nightly",
            Reply::json(
                200,
                &job("shop-nightly", DIRECT, r#","status":{"code":4},"description":"Close the day token=hunter2""#),
            ),
        );
        fake.route("/services", Reply::json(200, &format!(r#"{{"services":[{}]}}"#, service("shop", "600s"))));
        fake.route(
            "entries:list",
            Reply::json(
                200,
                &json!({ "entries": [
                    history::tests::finished("shop-nightly", "2026-11-05T12:20:00Z", 504),
                    history::tests::started("shop-nightly", "2026-11-05T12:15:00Z"),
                    history::tests::finished("shop-nightly", "2026-11-05T12:05:00Z", 504),
                    history::tests::started("shop-nightly", "2026-11-05T12:00:00Z"),
                ] })
                .to_string(),
            ),
        );
        fake
    }

    #[test]
    fn a_jobs_detail_says_its_attempts_and_when_it_last_worked() {
        let fake = fake_detail();
        let d = Detail::load(&client(&fake), AppsConvention::Whiteghost, "shop-nightly", now()).unwrap();
        assert_eq!(d.row.app(), Some("shop"));
        assert_eq!(d.row.timeout, Some(600.0));
        let s = d.row.streak.as_ref().unwrap();
        assert_eq!((s.failed, s.at_least, s.last.http), (2, true, Some(504)));
        let read = d.attempts.as_ref().unwrap();
        assert_eq!(read.list[0].took, Some(300.0));
        // The answer to the last-success read is the same scripted page: an entry at that time.
        assert_eq!(d.last_success, Some(LastSuccess::At(parse_rfc3339("2026-11-05T12:20:00Z").unwrap())));
        let v = d.json(now());
        assert_eq!(v["app"], "shop");
        assert_eq!(v["failure"]["codeName"], "DEADLINE_EXCEEDED");
        assert_eq!(v["failure"]["httpStatus"], 504);
        assert_eq!(v["target"]["url"], "https://shop-123456789.us-east1.run.app/cron/x");
        assert_eq!(
            (v["target"]["attemptDeadlineSeconds"].as_f64(), v["target"]["serviceTimeoutSeconds"].as_f64()),
            (Some(300.0), Some(600.0))
        );
        assert_eq!(v["target"]["auth"], "OIDC");
        assert_eq!(v["attempts"][0]["result"], "DEADLINE_EXCEEDED");
        assert_eq!(v["attempts"][0]["seconds"], 300.0);
        assert_eq!(v["description"], "Close the day token=[redacted]");
        assert!(v["findings"][0]["hint"].as_str().unwrap().contains("attempt deadline (300s)"));
        assert!(!v.to_string().contains("hunter2"), "{v}");
        assert_eq!(fake.requests().iter().filter(|r| r.path.ends_with("entries:list")).count(), 2);
    }

    #[test]
    fn a_last_success_that_cannot_be_read_is_said() {
        let fake = MockServer::start(vec![]);
        fake.route("/jobs/shop-nightly", Reply::json(200, &job("shop-nightly", DIRECT, r#","status":{"code":14}"#)));
        fake.route("/services", Reply::json(200, "{}"));
        // The window's read comes first and finds nothing; the next is refused.
        fake.push(Reply::json(200, "{}"));
        fake.push(Reply::json(403, r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no"}}"#));
        let d = Detail::load(&client(&fake), AppsConvention::Whiteghost, "shop-nightly", now()).unwrap();
        assert_eq!(d.last_success, Some(LastSuccess::Unknown));
        assert!(d.json(now())["lastSuccessNote"].as_str().unwrap().contains("did not say"));
    }

    #[test]
    fn a_job_that_works_reads_only_its_attempts() {
        let fake = MockServer::start(vec![]);
        fake.route("/jobs/shop-nightly", Reply::json(200, &job("shop-nightly", DIRECT, "")));
        fake.route("/services", Reply::json(200, "{}"));
        fake.route("entries:list", Reply::json(200, "{}"));
        let d = Detail::load(&client(&fake), AppsConvention::Whiteghost, "shop-nightly", now()).unwrap();
        assert!(d.last_success.is_none() && d.row.streak.is_none());
        assert_eq!(fake.requests().iter().filter(|r| r.path.ends_with("entries:list")).count(), 1);
        // No such job.
        let fake = MockServer::start(vec![]);
        fake.route(
            "/jobs/",
            Reply::json(404, r#"{"error":{"code":404,"status":"NOT_FOUND","message":"Job not found."}}"#),
        );
        fake.route("/services", Reply::json(200, "{}"));
        assert!(Detail::load(&client(&fake), AppsConvention::Whiteghost, "nope", now()).is_err());
    }
}
