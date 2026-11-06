//! Builds: Cloud Build builds per app, why a failed one failed, and the check that matters most
//! on a platform whose deploy command can claim success after a failed build: which apps' latest
//! build failed with no later build to fix it, while they go on running an older revision.
//! The terminal UI, the commands and the MCP server all read builds through here.

pub mod cmd;
pub mod report;

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::apps::{Column, Tone};
use crate::config::AppsConvention;
use crate::gcp::build::{self, Build, Log, LogLine, Step};
use crate::gcp::error::{in_key_block, redact};
use crate::gcp::logging::quote;
use crate::gcp::monitoring::rfc3339;
use crate::gcp::run::{self, Service};
use crate::gcp::{Gcp, GcpError, Result};
use crate::util::{fmt_ago, fmt_duration, looks_like_error};

/// What "last deploy failed" means, for the people and agents who read it.
pub const LAST_DEPLOY_EXPLAINED: &str = "the app's latest finished build failed and no later build succeeded. A deploy command can report success even so: what serves is whatever deployed last.";

/// Failed builds read to find each app's newest failure: more than a project's history holds so far.
const MAX_FAILED: usize = 3000;
/// Reads at the same time when going through the apps.
const READS_AT_ONCE: usize = 8;

/// Log lines kept before and after the first error, and from the end of the failing step.
const BEFORE_ERROR: usize = 5;
const AFTER_ERROR: usize = 20;
const STEP_TAIL: usize = 10;

/// What Cloud Run accepts as a service's name. Whoever can submit a build sets its substitutions
/// to anything, so any other text names no app and goes no further.
fn is_service_name(name: &str) -> bool {
    name.len() <= 63
        && name.starts_with(|c: char| c.is_ascii_lowercase())
        && !name.ends_with('-')
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

impl AppsConvention {
    /// The substitutions of a build that name the Cloud Run service it deploys; the first one set
    /// counts. Cloud Run's continuous deployment writes `_SERVICE_NAME`, and hand-written build
    /// configs usually `_SERVICE`.
    pub fn build_substitutions(self) -> &'static [&'static str] {
        match self {
            AppsConvention::Run => &["_SERVICE_NAME", "_SERVICE"],
            AppsConvention::Whiteghost => &["_SERVICE"],
        }
    }

    /// The app a build belongs to.
    pub fn build_app(self, build: &Build) -> Option<&str> {
        self.build_substitutions()
            .iter()
            .find_map(|key| build.substitutions.get(*key))
            .map(String::as_str)
            .filter(|app| is_service_name(app))
    }

    /// The Cloud Build filter for an app's builds.
    pub fn build_filter(self, app: &str) -> String {
        let any: Vec<String> =
            self.build_substitutions().iter().map(|key| format!("substitutions.{key}={}", quote(app))).collect();
        if any.len() == 1 { any.join("") } else { format!("({})", any.join(" OR ")) }
    }
}

/// An app whose latest finished build failed, with no later build that succeeded.
#[derive(Debug, Clone)]
pub struct Failing {
    pub app: String,
    /// Its newest failed build.
    pub build: Build,
    /// A newer build is running or queued: it may still fix it.
    pub retrying: bool,
    /// The revision serving, when there is one.
    pub serving: String,
    pub serving_since: Option<f64>,
}

impl Failing {
    fn new(build: Build, app: &str, after: After, service: &Service) -> Failing {
        Failing {
            app: app.to_string(),
            build,
            retrying: after.running,
            serving: service.revision().to_string(),
            serving_since: service.revision_ready_at(),
        }
    }

    /// What serves was made before the failed build started: its code never went live.
    /// `false` when a revision came after it: the build failed past its deploy step, or someone
    /// deployed another way.
    pub fn older(&self) -> bool {
        self.serving.is_empty()
            || self.serving_since.zip(self.build.created()).is_none_or(|(serving, created)| serving < created)
    }

    /// `last deploy failed`, or `last build failed` when a revision came after it.
    pub fn headline(&self) -> &'static str {
        if self.older() { "last deploy failed" } else { "last build failed" }
    }

    /// What is serving instead.
    pub fn serving_text(&self) -> String {
        let since = self.serving_since.map(fmt_ago).unwrap_or_else(|| "some time ago".into());
        let mut text = if self.serving.is_empty() {
            "no revision is serving".to_string()
        } else if self.older() {
            format!("still running {} from {since}", self.serving)
        } else {
            format!("{} came after it, {since}", self.serving)
        };
        if self.retrying {
            text.push_str(" · a newer build is running");
        }
        text
    }

    /// The whole of it, in a line.
    pub fn text(&self) -> String {
        let failed = self.build.at().map(|t| format!(" {}", fmt_ago(t))).unwrap_or_default();
        format!("{}{failed}: {}", self.headline(), self.serving_text())
    }

    pub fn json(&self) -> Value {
        let b = &self.build;
        json!({
            "app": self.app,
            "headline": self.headline(),
            "buildId": b.id,
            "status": b.status,
            "failedAt": Some(&b.finish_time).filter(|t| !t.is_empty()),
            "commit": Some(b.commit()).filter(|c| !c.is_empty()),
            "trigger": Some(b.trigger()).filter(|t| !t.is_empty()),
            "failingStep": b.failing_step().map(|(_, s)| s.label()),
            "servingRevision": Some(&self.serving).filter(|r| !r.is_empty()),
            "servingSince": self.serving_since.map(rfc3339),
            "runningOlderRevision": self.older(),
            "newerBuildRunning": self.retrying,
        })
    }
}

/// What the builds after a failed one say.
#[derive(Debug, Clone, Copy, Default)]
struct After {
    succeeded: bool,
    running: bool,
}

/// The builds of `app` created after `failed`: one that succeeded ends the reading.
fn after(gcp: &Gcp, convention: AppsConvention, app: &str, failed: &Build) -> Result<After> {
    let filter =
        build::all_of(&[&convention.build_filter(app), &format!("create_time>{}", quote(&failed.create_time))]);
    let mut seen = After::default();
    build::statuses(gcp, &filter, |status| {
        seen.succeeded |= status == "SUCCESS";
        seen.running |= build::is_running(status);
        !seen.succeeded
    })?;
    Ok(seen)
}

/// Which apps' latest deploy failed.
#[derive(Debug, Clone, Default)]
pub struct Deploys {
    /// Most recent failure first.
    pub failing: Vec<Failing>,
    /// Names that failed builds give and no Cloud Run service has: apps since deleted.
    pub not_services: Vec<String>,
}

impl Deploys {
    pub fn of(&self, app: &str) -> Option<&Failing> {
        self.failing.iter().find(|f| f.app == app)
    }
}

/// Runs `read` over `items`, a few at a time, keeping their order.
fn read_all<T: Sync, U: Send>(items: &[T], read: impl Fn(&T) -> Result<U> + Sync) -> Vec<Result<U>> {
    let read = &read;
    let mut out = Vec::with_capacity(items.len());
    for batch in items.chunks(READS_AT_ONCE) {
        std::thread::scope(|s| {
            let reads: Vec<_> = batch.iter().map(|item| s.spawn(move || read(item))).collect();
            out.extend(reads.into_iter().map(|r| r.join().unwrap_or_else(|_| Err(lost()))));
        });
    }
    out
}

fn lost() -> GcpError {
    GcpError::Network("a read failed unexpectedly".into())
}

/// Every app whose latest build failed with no later success. Reads the environment's failed
/// builds and its services, then, for each app with a failure, the builds after its newest one.
/// A build naming no service is left out: an app is a Cloud Run service, and apps that are gone
/// must not alert for ever.
pub fn failing_deploys(gcp: &Gcp, convention: AppsConvention) -> Result<Deploys> {
    let (failed, services) = std::thread::scope(|s| {
        let services = s.spawn(|| run::all_services(gcp));
        let failed = build::recent(gcp, &build::failed_filter(), MAX_FAILED);
        (failed, services.join().unwrap_or_else(|_| Err(lost())))
    });
    let (failed, services) = (failed?, services?);
    let mut seen = HashSet::new();
    let mut not_services = Vec::new();
    let mut candidates: Vec<(String, Build, &Service)> = Vec::new();
    for b in failed {
        let Some(app) = convention.build_app(&b).map(str::to_string) else { continue };
        if !seen.insert(app.clone()) {
            continue;
        }
        match services.iter().find(|s| s.id() == app) {
            Some(service) => candidates.push((app, b, service)),
            None => not_services.push(app),
        }
    }
    not_services.sort();
    let reads = read_all(&candidates, |(app, b, _)| after(gcp, convention, app, b));
    let mut failing = Vec::new();
    for ((app, b, service), read) in candidates.into_iter().zip(reads) {
        let later = read?;
        if !later.succeeded {
            failing.push(Failing::new(b, &app, later, service));
        }
    }
    failing.sort_by(|a, b| b.build.create_time.cmp(&a.build.create_time));
    Ok(Deploys { failing, not_services })
}

/// One app's last deploy, when it failed.
pub fn last_deploy(gcp: &Gcp, convention: AppsConvention, service: &Service) -> Result<Option<Failing>> {
    let app = service.id();
    let filter = build::all_of(&[&convention.build_filter(app), &build::failed_filter()]);
    let Some(failed) = build::recent(gcp, &filter, 1)?.into_iter().next() else { return Ok(None) };
    let later = after(gcp, convention, app, &failed)?;
    Ok((!later.succeeded).then(|| Failing::new(failed, app, later, service)))
}

/// [`last_deploy`] by the app's name; a name with no service has none.
pub fn app_deploys(gcp: &Gcp, convention: AppsConvention, app: &str) -> Result<Deploys> {
    let service = match run::find_service(gcp, app) {
        Ok(service) => service,
        Err(GcpError::Api { status: 404, .. }) => {
            return Ok(Deploys { not_services: vec![app.to_string()], ..Deploys::default() });
        }
        Err(e) => return Err(e),
    };
    Ok(Deploys { failing: last_deploy(gcp, convention, &service)?.into_iter().collect(), ..Deploys::default() })
}

/// The filter of a list: an app's builds, only the failed ones, or all.
pub fn list_filter(convention: AppsConvention, app: Option<&str>, failed: bool) -> String {
    let app = app.map(|a| convention.build_filter(a)).unwrap_or_default();
    let failed = if failed { build::failed_filter() } else { String::new() };
    build::all_of(&[&app, &failed])
}

/// A list of builds with how the last deploys stand, read at the same time.
pub struct Listing {
    pub builds: Vec<Build>,
    /// Without it the list still says what ran; only the notes are missing.
    pub deploys: Result<Deploys>,
}

impl Listing {
    /// The newest builds, of one app or of all, with the last deploy of that app or of every one.
    pub fn load(
        gcp: &Gcp,
        convention: AppsConvention,
        app: Option<&str>,
        failed: bool,
        limit: usize,
    ) -> Result<Listing> {
        let (builds, deploys) = std::thread::scope(|s| {
            let deploys = s.spawn(|| match app {
                Some(a) => app_deploys(gcp, convention, a),
                None => failing_deploys(gcp, convention),
            });
            let builds = build::recent(gcp, &list_filter(convention, app, failed), limit);
            (builds, deploys.join().unwrap_or_else(|_| Err(lost())))
        });
        Ok(Listing { builds: builds?, deploys })
    }

    pub fn rows(&self, convention: AppsConvention) -> Vec<Row<'_>> {
        self.builds.iter().map(|b| Row::new(b, convention, self.deploys.as_ref().ok())).collect()
    }
}

// ---- the list --------------------------------------------------------------------------------

/// A build's mark and how it should look.
pub fn mark(status: &str) -> (&'static str, Tone) {
    match status {
        "SUCCESS" => ("✓", Tone::Plain),
        s if build::FAILED.contains(&s) => ("✗", Tone::Bad),
        s if build::is_running(s) => ("●", Tone::Warn),
        _ => ("·", Tone::Muted),
    }
}

/// A build as the list shows it.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    pub build: &'a Build,
    pub app: Option<&'a str>,
    /// Set when this build is what makes its app's last deploy a failed one.
    pub failing: Option<&'a Failing>,
}

impl<'a> Row<'a> {
    pub fn new(build: &'a Build, convention: AppsConvention, deploys: Option<&'a Deploys>) -> Row<'a> {
        let app = convention.build_app(build);
        let failing = app.and_then(|a| deploys?.of(a)).filter(|f| f.build.id == build.id);
        Row { build, app, failing }
    }

    /// The row as `--json` and the MCP server give it. Substitutions other than those it reads
    /// never leave: a build config can pass anything in them.
    pub fn json(&self, now: f64) -> Value {
        let b = self.build;
        let some = |text: &'a str| Some(text).filter(|t| !t.is_empty());
        let mut v = json!({
            "id": b.id,
            "status": b.status,
            "app": self.app,
            "commit": some(b.commit()),
            "branch": some(b.branch()),
            "repo": some(b.repo()),
            "trigger": some(b.trigger()),
            "createTime": some(&b.create_time),
            "startTime": some(&b.start_time),
            "finishTime": some(&b.finish_time),
            "durationSeconds": b.secs(now).map(f64::round),
            "failingStep": b.failing_step().map(|(_, s)| s.label()),
        });
        if let Some(f) = self.failing {
            v["lastDeployFailed"] = f.json();
        }
        v
    }
}

/// The list's columns.
pub fn columns() -> Vec<Column> {
    let col = |head, right, drop| Column { head, right, drop };
    vec![
        col("STATUS", false, 0),
        col("APP", false, 0),
        col("ID", false, 3),
        col("COMMIT", false, 4),
        col("AGE", true, 1),
        col("TOOK", true, 2),
        col("TRIGGER", false, 5),
        col("NOTES", false, 0),
    ]
}

/// A row's cells, in [`columns`]' order.
pub fn cells(row: &Row, now: f64) -> Vec<(String, Tone)> {
    let b = row.build;
    let (m, tone) = mark(&b.status);
    let age = b.at().map(|t| fmt_ago(t).trim_end_matches(" ago").to_string()).unwrap_or_default();
    let took = b.secs(now).map(fmt_duration).unwrap_or_default();
    let (notes, notes_tone) = match row.failing {
        Some(f) => (format!("{}: {}", f.headline(), f.serving_text()), Tone::Bad),
        None if b.failed() => (b.failing_step().map(|(_, s)| s.label().to_string()).unwrap_or_default(), Tone::Muted),
        None => (String::new(), Tone::Muted),
    };
    vec![
        (format!("{m} {}", b.status.to_lowercase()), tone),
        (row.app.unwrap_or("—").to_string(), if row.app.is_some() { Tone::Plain } else { Tone::Muted }),
        (b.short_id().to_string(), Tone::Muted),
        (b.short_commit().to_string(), Tone::Muted),
        (age, Tone::Muted),
        (took, Tone::Muted),
        (b.trigger().to_string(), Tone::Muted),
        (notes, notes_tone),
    ]
}

// ---- one build ---------------------------------------------------------------------------------

/// How a step went: its mark, what to say and how it should look.
pub fn step_state(step: &Step, build: &Build, now: f64) -> (&'static str, String, Tone) {
    let took = step.secs(now).map(fmt_duration);
    if step.failed() {
        let exit = step.exit_code.map(|c| format!(" · exit {c}")).unwrap_or_default();
        let how = if step.status == "FAILURE" { String::new() } else { format!(" · {}", step.status.to_lowercase()) };
        return ("✗", format!("{}{exit}{how}", took.unwrap_or_default()), Tone::Bad);
    }
    match step.status.as_str() {
        "SUCCESS" => ("✓", took.unwrap_or_default(), Tone::Plain),
        "WORKING" => ("●", took.unwrap_or_default(), Tone::Warn),
        "QUEUED" | "PENDING" if build.running() => ("·", "queued".into(), Tone::Muted),
        "QUEUED" | "PENDING" => ("·", "not run".into(), Tone::Muted),
        other => ("·", other.to_lowercase(), Tone::Muted),
    }
}

/// Where a step falls in its build: columns from the left and columns long, of `width`.
pub fn bar(step: &Step, build: &Build, width: usize, now: f64) -> Option<(usize, usize)> {
    let start = build.started().or_else(|| step.started())?;
    let whole = build.finished().unwrap_or(now) - start;
    let (from, to) = (step.started()?, step.ended().unwrap_or(now));
    if whole <= 0.0 || width == 0 {
        return None;
    }
    let column = |t: f64| ((t - start) / whole * width as f64).clamp(0.0, width as f64);
    let left = column(from).floor() as usize;
    let long = (column(to).ceil() as usize).saturating_sub(left).max(1);
    Some((left.min(width - 1), long.min(width - left.min(width - 1))))
}

/// Why a build failed: the failing step and the lines of its log that tell.
#[derive(Debug, Clone, Default)]
pub struct Failure {
    /// The step's number in the build.
    pub step: Option<usize>,
    /// The first error's index in the log, or the step's last line when no line says one. Where
    /// a failed build's log opens.
    pub first_error: Option<usize>,
    /// The line at `first_error` says an error.
    pub said: bool,
    /// (index in the log, text). Numbers jump where lines were left out.
    pub excerpt: Vec<(usize, String)>,
    /// Lines read of the log.
    pub lines: usize,
    /// The log has more than was read.
    pub cut: bool,
}

/// A line that says something failed, rather than one that only mentions a word like it: names of
/// files and packages (`error-handler.ts`, `node-domexception@1.0.0`, `Revision...failed`) and
/// warnings don't count, or a deprecation notice would pass for the error.
fn says_error(line: &str) -> bool {
    let mut words = line.split_whitespace().map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()));
    if words.clone().take(3).any(|w| w.eq_ignore_ascii_case("warn") || w.eq_ignore_ascii_case("warning")) {
        return false;
    }
    let prose: Vec<&str> = line
        .split_whitespace()
        .filter(|_| words.next().is_some_and(|w| !w.contains(['/', '.', '_', '-', '@', '=', ':'])))
        .collect();
    looks_like_error(&prose.join(" "))
}

/// The line a failed build's log should open at: the first that says an error in the failing
/// step, else that step's last line (a step can die without saying the word), else the first that
/// says one anywhere (a failure before any step ran).
pub fn first_error(lines: &[LogLine], step: Option<usize>) -> Option<usize> {
    let of_step = |l: &LogLine| step.is_some() && l.step == step;
    lines
        .iter()
        .position(|l| of_step(l) && says_error(&l.text))
        .or_else(|| lines.iter().rposition(of_step))
        .or_else(|| lines.iter().position(|l| says_error(&l.text)))
}

/// A log line as handed on: safe to paste, whatever it held (tokens, escapes), indented as it was.
fn excerpt_line(text: &str) -> String {
    let indent = &text[..text.len() - text.trim_start().len()];
    format!("{indent}{}", redact(text))
}

impl Failure {
    /// Reads `log` for why `build` failed. Other steps' lines stay out: steps can run in parallel.
    pub fn of(build: &Build, log: &Log) -> Failure {
        let step = build.failing_step().map(|(n, _)| n);
        let first_error = first_error(&log.lines, step);
        let kept: Vec<usize> = (0..log.lines.len()).filter(|&i| step.is_none() || log.lines[i].step == step).collect();
        // Nothing of the step was logged: the whole log is what there is.
        let kept = if kept.is_empty() { (0..log.lines.len()).collect() } else { kept };
        let at = first_error.and_then(|e| kept.iter().position(|&i| i == e)).unwrap_or(kept.len().saturating_sub(1));
        let mut chosen: Vec<usize> = Vec::new();
        chosen.extend(&kept[at.saturating_sub(BEFORE_ERROR)..(at + AFTER_ERROR + 1).min(kept.len())]);
        chosen.extend(&kept[kept.len().saturating_sub(STEP_TAIL)..]);
        chosen.sort_unstable();
        chosen.dedup();
        let hidden = in_key_block(log.lines.iter().map(|l| l.text.as_str()));
        let excerpt = chosen
            .into_iter()
            .map(|i| (i, if hidden[i] { "[redacted]".to_string() } else { excerpt_line(&log.lines[i].text) }))
            .collect();
        let said = first_error.is_some_and(|i| says_error(&log.lines[i].text));
        Failure { step, first_error, said, excerpt, lines: log.lines.len(), cut: log.cut }
    }

    /// The first error's line, when one says it.
    pub fn first_error_text(&self) -> Option<&str> {
        let at = self.first_error.filter(|_| self.said)?;
        self.excerpt.iter().find(|(i, _)| *i == at).map(|(_, t)| t.as_str())
    }

    /// As the MCP server and `--json` give it. Line numbers count from 1.
    pub fn json(&self, build: &Build, now: f64) -> Value {
        let step = self.step.and_then(|n| build.steps.get(n).map(|s| (n, s)));
        json!({
            "type": Some(&build.failure_info.kind).filter(|k| !k.is_empty()),
            "detail": Some(crate::gcp::error::redact_tokens(&build.failure_info.detail)).filter(|d| !d.is_empty()),
            "step": step.map(|(n, s)| json!({
                "number": n,
                "id": s.label(),
                "builder": s.name,
                "status": s.status,
                "exitCode": s.exit_code,
                "seconds": s.secs(now).map(f64::round),
            })),
            "firstErrorLine": self.first_error.filter(|_| self.said).map(|i| i + 1),
            "firstError": self.first_error_text(),
            "logLinesRead": self.lines,
            "logCut": self.cut,
            "logLines": self.excerpt.iter().map(|(i, t)| json!({ "line": i + 1, "text": t })).collect::<Vec<_>>(),
        })
    }
}

/// A build with what its detail shows.
pub struct Detail {
    pub build: Build,
    pub app: Option<String>,
    /// Failed builds of an app: how its last deploy stands.
    pub deploy: Option<Result<Deploys>>,
    /// Read for failed builds, and when asked.
    pub log: Option<Result<Log>>,
}

impl Detail {
    /// Finds the build, then reads its log (failed builds, or `with_log`) and how its app's last
    /// deploy stands at the same time.
    pub fn load(gcp: &Gcp, convention: AppsConvention, id: &str, with_log: bool) -> Result<Detail> {
        let build = build::find(gcp, id)?;
        let app = convention.build_app(&build).map(str::to_string);
        let failed = build.failed();
        let (deploy, log) = std::thread::scope(|s| {
            let deploy =
                app.as_deref().filter(|_| failed).map(|app| s.spawn(move || app_deploys(gcp, convention, app)));
            let log = (failed || with_log).then(|| build::log(gcp, &build));
            (deploy.map(|d| d.join().unwrap_or_else(|_| Err(lost()))), log)
        });
        Ok(Detail { build, app, deploy, log })
    }

    /// Why it failed, when it failed and its log was read.
    pub fn failure(&self) -> Option<Failure> {
        match (self.build.failed(), &self.log) {
            (true, Some(Ok(log))) => Some(Failure::of(&self.build, log)),
            _ => None,
        }
    }

    /// What its failure means for the app now, in a line.
    pub fn since(&self) -> Option<String> {
        Some(since_text(&self.build.id, self.deploy.as_ref()?.as_ref().ok()?))
    }

    /// Its app, when this build is what makes the app's last deploy a failed one.
    pub fn failing(&self) -> Option<&Failing> {
        self.deploy.as_ref()?.as_ref().ok()?.failing.first().filter(|f| f.build.id == self.build.id)
    }

    pub fn json(&self, now: f64) -> Value {
        let b = &self.build;
        let row = Row { build: b, app: self.app.as_deref(), failing: self.failing() };
        let mut v = row.json(now);
        let obj = v.as_object_mut().expect("an object");
        obj.insert("statusDetail".into(), json!(Some(&b.status_detail).filter(|d| !d.is_empty())));
        obj.insert("url".into(), json!(Some(&b.log_url).filter(|u| !u.is_empty())));
        let steps: Vec<Value> = b
            .steps
            .iter()
            .enumerate()
            .map(|(n, s)| {
                json!({
                    "number": n,
                    "id": s.label(),
                    "builder": s.name,
                    "status": s.status,
                    "exitCode": s.exit_code,
                    "seconds": s.secs(now).map(f64::round),
                })
            })
            .collect();
        obj.insert("steps".into(), json!(steps));
        if let Some(text) = self.since() {
            obj.insert("since".into(), json!(text));
        }
        if let Some(failure) = self.failure() {
            obj.insert("failure".into(), failure.json(b, now));
        }
        if let Some(Err(e)) = &self.log {
            obj.insert("logError".into(), json!(e.short()));
        }
        if let Some(Err(e)) = &self.deploy {
            obj.insert("lastDeployError".into(), json!(e.short()));
        }
        v
    }
}

/// What failed build `id` means for its app now: the app's last deploy, followed by another
/// failure, or fixed by a later build.
pub fn since_text(id: &str, deploys: &Deploys) -> String {
    match deploys.failing.first() {
        Some(f) if f.build.id == id => f.text(),
        Some(f) => format!("a later build, {}, failed too: {}", f.build.short_id(), f.text()),
        None if !deploys.not_services.is_empty() => "it has no Cloud Run service".into(),
        None => "a later build succeeded".into(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::build::tests::FAILED_BUILD;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::util::parse_rfc3339;

    pub(crate) fn failed() -> Build {
        serde_json::from_str(FAILED_BUILD).unwrap()
    }

    fn client(server: &MockServer) -> std::sync::Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()))
    }

    fn service(name: &str, ready: &str) -> String {
        format!(
            r#"{{"name":"projects/p/locations/us-east1/services/{name}",
                 "latestReadyRevision":"projects/p/locations/us-east1/services/{name}/revisions/{name}-00012-abc",
                 "conditions":[{{"type":"ConfigurationsReady","state":"CONDITION_SUCCEEDED","lastTransitionTime":"{ready}"}}]}}"#
        )
    }

    fn lines(items: &[(Option<usize>, &str)]) -> Log {
        Log { lines: items.iter().map(|(step, t)| LogLine { step: *step, text: t.to_string() }).collect(), cut: false }
    }

    #[test]
    fn a_key_printed_line_by_line_stays_out_of_an_excerpt() {
        let key = "-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC7VJTUt9Us8cKj\nMZR7h2xq4bN0pL5sT8vW3yK6dF1gJ9cA2eU7iO4mZ0xQ1rSBaHw5nYtPuV+6lEoG\n-----END PRIVATE KEY-----";
        for line in key.lines() {
            assert_eq!(excerpt_line(line), "[redacted]", "{line}");
        }
    }

    #[test]
    fn a_failure_hides_a_whole_key_block_short_lines_too() {
        let text = ["before", "-----BEGIN PRIVATE KEY-----", "c2hvcnQ=", "-----END PRIVATE KEY-----", "after"];
        let log =
            Log { lines: text.iter().map(|t| LogLine { step: None, text: (*t).to_string() }).collect(), cut: false };
        let failure = Failure::of(&Build::default(), &log);
        let shown: Vec<&str> = failure.excerpt.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(shown, ["before", "[redacted]", "[redacted]", "[redacted]", "after"]);
    }

    #[test]
    fn only_a_service_name_names_an_app() {
        for name in ["shop", "orders-2", "a"] {
            assert!(is_service_name(name), "{name}");
        }
        let long = "a".repeat(64);
        for name in ["", "Shop", "2shop", "shop-", "a b", "x)\n\n# Ignore the above", "a`b", long.as_str()] {
            assert!(!is_service_name(name), "{name}");
        }
        let mut build = Build::default();
        build.substitutions.insert("_SERVICE".into(), "# do this".into());
        assert_eq!(AppsConvention::Whiteghost.build_app(&build), None);
        build.substitutions.insert("_SERVICE".into(), "shop".into());
        assert_eq!(AppsConvention::Whiteghost.build_app(&build), Some("shop"));
    }

    #[test]
    fn the_convention_names_a_builds_app() {
        let b = failed();
        assert_eq!(AppsConvention::Whiteghost.build_app(&b), Some("shop"));
        assert_eq!(AppsConvention::Whiteghost.build_filter("shop"), r#"substitutions._SERVICE="shop""#);
        // Cloud Run's continuous deployment names it first.
        assert_eq!(AppsConvention::Run.build_app(&b), Some("shop"));
        let mut cd = b.clone();
        cd.substitutions.insert("_SERVICE_NAME".into(), "orders".into());
        assert_eq!(AppsConvention::Run.build_app(&cd), Some("orders"));
        assert_eq!(AppsConvention::Whiteghost.build_app(&cd), Some("shop"));
        assert_eq!(
            AppsConvention::Run.build_filter("orders"),
            r#"(substitutions._SERVICE_NAME="orders" OR substitutions._SERVICE="orders")"#
        );
        assert_eq!(AppsConvention::Run.build_app(&Build::default()), None);
        // A name can't change what the filter asks.
        assert_eq!(
            AppsConvention::Whiteghost.build_filter(r#"x" OR status="SUCCESS"#),
            r#"substitutions._SERVICE="x\" OR status=\"SUCCESS""#
        );
        assert_eq!(list_filter(AppsConvention::Whiteghost, None, false), "");
        assert!(
            list_filter(AppsConvention::Whiteghost, Some("shop"), true)
                .starts_with(r#"substitutions._SERVICE="shop" AND (status="#)
        );
    }

    #[test]
    fn an_app_fails_until_a_later_build_succeeds() {
        let fake = MockServer::start(vec![]);
        let newer_failure = FAILED_BUILD
            .replace("1a2b3c4d-1111", "9f9f9f9f-1111")
            .replace("2026-11-05T10:00:00.123456789Z", "2026-11-05T11:00:00Z");
        let fixed = FAILED_BUILD
            .replace("\"_SERVICE\":\"shop\"", "\"_SERVICE\":\"fixed\"")
            .replace("1a2b3c4d-1111", "bbbbbbbb-1111");
        let retrying = FAILED_BUILD
            .replace("\"_SERVICE\":\"shop\"", "\"_SERVICE\":\"retrying\"")
            .replace("1a2b3c4d-1111", "cccccccc-1111")
            .replace("2026-11-05T10:00:00.123456789Z", "2026-11-05T09:00:00Z");
        let deleted = FAILED_BUILD
            .replace("\"_SERVICE\":\"shop\"", "\"_SERVICE\":\"deleted\"")
            .replace("1a2b3c4d-1111", "dddddddd-1111");
        let direct = r#"{"id":"eeeeeeee","status":"FAILURE","createTime":"2026-11-05T12:00:00Z"}"#;
        // Newest first, as the API lists them; `shop`'s older failure must not count.
        fake.route(
            "pageSize=1000",
            Reply::json(
                200,
                &format!(r#"{{"builds":[{direct},{newer_failure},{FAILED_BUILD},{fixed},{retrying},{deleted}]}}"#),
            ),
        );
        fake.route(
            "substitutions._SERVICE%3D%22shop%22",
            Reply::json(200, r#"{"builds":[{"status":"CANCELLED"},{"status":"FAILURE"}]}"#),
        );
        fake.route(
            "substitutions._SERVICE%3D%22fixed%22",
            Reply::json(200, r#"{"builds":[{"status":"FAILURE"},{"status":"SUCCESS"}]}"#),
        );
        fake.route("substitutions._SERVICE%3D%22retrying%22", Reply::json(200, r#"{"builds":[{"status":"WORKING"}]}"#));
        fake.route(
            "/services",
            Reply::json(
                200,
                &format!(
                    r#"{{"services":[{},{},{}]}}"#,
                    service("shop", "2026-11-01T00:00:00Z"),
                    service("fixed", "2026-11-01T00:00:00Z"),
                    service("retrying", "2026-11-05T09:30:00Z")
                ),
            ),
        );
        let got = failing_deploys(&client(&fake), AppsConvention::Whiteghost).unwrap();
        assert_eq!(got.failing.iter().map(|f| f.app.as_str()).collect::<Vec<_>>(), ["shop", "retrying"]);
        assert_eq!(got.not_services, ["deleted"]);
        let shop = got.of("shop").unwrap();
        assert_eq!(shop.build.short_id(), "9f9f9f9f", "its newest failure");
        assert!(shop.older() && !shop.retrying);
        assert_eq!(shop.headline(), "last deploy failed");
        assert!(shop.serving_text().starts_with("still running shop-00012-abc from "));
        // A revision made after the failed build started: the build failed past its deploy step,
        // or someone deployed another way.
        let retrying = got.of("retrying").unwrap();
        assert!(!retrying.older() && retrying.retrying);
        assert_eq!(retrying.headline(), "last build failed");
        let text = retrying.serving_text();
        assert!(
            text.starts_with("retrying-00012-abc came after it, ") && text.ends_with(" · a newer build is running"),
            "{text}"
        );
        let json = shop.json();
        assert_eq!(
            (json["buildId"].as_str(), json["runningOlderRevision"].as_bool(), json["failingStep"].as_str()),
            (Some("9f9f9f9f-1111-2222-3333-444455556666"), Some(true), Some("build-or-fail"))
        );
        // What counts is what came after its newest failure.
        let requests = fake.requests();
        let shop_read = requests.iter().find(|r| r.path.contains("%22shop%22")).unwrap();
        assert!(shop_read.path.contains("create_time%3E%222026-11-05T11%3A00%3A00Z%22"), "{}", shop_read.path);
        assert!(shop_read.path.contains("fields=builds%28status%29"), "{}", shop_read.path);
        assert!(requests.iter().all(|r| r.method == "GET"));
        assert!(!requests.iter().any(|r| r.path.contains("deleted")), "an app that is gone needs no read");
    }

    #[test]
    fn a_read_that_fails_is_not_a_clean_bill_of_health() {
        let fake = MockServer::start(vec![]);
        fake.route("pageSize=1000", Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD}]}}"#)));
        fake.route(
            "/services",
            Reply::json(200, &format!(r#"{{"services":[{}]}}"#, service("shop", "2026-11-01T00:00:00Z"))),
        );
        fake.route(
            "%22shop%22",
            Reply::json(403, r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no"}}"#),
        );
        assert!(failing_deploys(&client(&fake), AppsConvention::Whiteghost).is_err());
        let fake = MockServer::start(vec![]);
        fake.route("pageSize=1000", Reply::json(200, r#"{"builds":[]}"#));
        fake.route(
            "/services",
            Reply::json(403, r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no"}}"#),
        );
        assert!(failing_deploys(&client(&fake), AppsConvention::Whiteghost).is_err());
    }

    #[test]
    fn one_apps_last_deploy() {
        let fake = MockServer::start(vec![]);
        fake.route("create_time", Reply::json(200, r#"{"builds":[{"status":"SUCCESS"}]}"#));
        fake.route("%22fixed%22", Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD}]}}"#)));
        fake.route("%22never%22", Reply::json(200, "{}"));
        let gcp = client(&fake);
        let s: Service = serde_json::from_str(&service("never", "2026-11-01T00:00:00Z")).unwrap();
        assert!(last_deploy(&gcp, AppsConvention::Whiteghost, &s).unwrap().is_none(), "no failed build");
        let s: Service = serde_json::from_str(&service("fixed", "2026-11-01T00:00:00Z")).unwrap();
        assert!(last_deploy(&gcp, AppsConvention::Whiteghost, &s).unwrap().is_none(), "a later success");
        let fake = MockServer::start(vec![]);
        fake.route("create_time", Reply::json(200, "{}"));
        fake.route("%22shop%22", Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD}]}}"#)));
        let s: Service = serde_json::from_str(&service("shop", "2026-11-01T00:00:00Z")).unwrap();
        let f = last_deploy(&client(&fake), AppsConvention::Whiteghost, &s).unwrap().unwrap();
        assert_eq!(f.app, "shop");
        // By name: no such service, no last deploy.
        let fake = MockServer::start(vec![Reply::json(
            404,
            r#"{"error":{"code":404,"status":"NOT_FOUND","message":"gone"}}"#,
        )]);
        let d = app_deploys(&client(&fake), AppsConvention::Whiteghost, "gone").unwrap();
        assert!(d.failing.is_empty());
        assert_eq!(d.not_services, ["gone"]);
    }

    #[test]
    fn rows_flag_the_build_behind_a_failed_deploy() {
        let b = failed();
        let later = Failing {
            app: "shop".into(),
            build: b.clone(),
            retrying: false,
            serving: "shop-00012-abc".into(),
            serving_since: parse_rfc3339("2026-11-01T00:00:00Z"),
        };
        let deploys = Deploys { failing: vec![later], ..Deploys::default() };
        let now = parse_rfc3339("2026-11-05T12:00:00Z").unwrap();
        let row = Row::new(&b, AppsConvention::Whiteghost, Some(&deploys));
        assert!(row.failing.is_some());
        let shown = cells(&row, now);
        assert_eq!(shown.len(), columns().len());
        assert_eq!(shown[0], ("✗ failure".to_string(), Tone::Bad));
        assert_eq!(shown[1].0, "shop");
        assert_eq!(shown[2].0, "1a2b3c4d");
        assert_eq!(shown[3].0, "0123456");
        assert_eq!(shown[5].0, "3m 00s");
        assert_eq!(shown[6].0, "shop-main-deploy");
        assert_eq!(shown[7].1, Tone::Bad);
        assert!(shown[7].0.starts_with("last deploy failed: still running shop-00012-abc from "), "{}", shown[7].0);
        // An older failure of the same app, or the same one with no deploy read: the failing step.
        let mut older = b.clone();
        older.id = "00000000-older".into();
        let row = Row::new(&older, AppsConvention::Whiteghost, Some(&deploys));
        assert!(row.failing.is_none());
        assert_eq!(cells(&row, now)[7], ("build-or-fail".to_string(), Tone::Muted));

        let json = Row::new(&b, AppsConvention::Whiteghost, Some(&deploys)).json(now);
        assert_eq!(json["app"], "shop");
        assert_eq!(json["durationSeconds"], 180.0);
        assert_eq!(json["lastDeployFailed"]["app"], "shop");
        assert!(!json.to_string().contains("hunter2"), "substitutions it does not read stay out");

        let working: Build = serde_json::from_str(r#"{"id":"w","status":"WORKING"}"#).unwrap();
        let shown = cells(&Row::new(&working, AppsConvention::Run, None), now);
        assert_eq!((shown[0].0.as_str(), shown[0].1, shown[1].0.as_str()), ("● working", Tone::Warn, "—"));
    }

    #[test]
    fn steps_say_how_they_went_and_where_they_fall() {
        let b = failed();
        let now = parse_rfc3339("2026-11-05T12:00:00Z").unwrap();
        assert_eq!(step_state(&b.steps[0], &b, now), ("✓", "1m 00s".to_string(), Tone::Plain));
        assert_eq!(step_state(&b.steps[1], &b, now), ("✗", "2m 00s · exit 1".to_string(), Tone::Bad));
        assert_eq!(step_state(&b.steps[2], &b, now), ("·", "not run".to_string(), Tone::Muted));
        assert_eq!(bar(&b.steps[0], &b, 30, now), Some((0, 10)));
        assert_eq!(bar(&b.steps[1], &b, 30, now), Some((10, 20)));
        assert_eq!(bar(&b.steps[2], &b, 30, now), None);
        // Short steps still show; nothing overflows.
        let mut quick = b.clone();
        quick.steps[0].timing.end_time = "2026-11-05T10:00:06Z".into();
        assert_eq!(bar(&quick.steps[0], &quick, 30, now), Some((0, 1)));
        for width in [1, 2, 7] {
            let (left, long) = bar(&b.steps[1], &b, width, now).unwrap();
            assert!(left + long <= width && long >= 1, "{left}+{long} of {width}");
        }
        let mut running = b.clone();
        running.status = "WORKING".into();
        running.finish_time.clear();
        running.steps[2].status = "QUEUED".into();
        assert_eq!(step_state(&running.steps[2], &running, now).1, "queued");
    }

    #[test]
    fn the_first_error_is_looked_for_in_the_failing_step() {
        let log = lines(&[
            (None, "FETCHSOURCE"),
            // Another step's noise, even failing words, comes first.
            (Some(0), "Error: this step went on"),
            (Some(1), "npm ci"),
            (Some(0), "ok"),
            (Some(1), "added 3 packages, 0 errors"),
            (Some(1), "npm ERR! code ELIFECYCLE"),
            (Some(1), "npm ERR! second"),
            (None, "ERROR: build step 1 failed"),
        ]);
        assert_eq!(first_error(&log.lines, Some(1)), Some(5));
        // No step known (a failure before the steps): the first that says one.
        assert_eq!(first_error(&log.lines, None), Some(1));
        // The step dies without saying the word: its last line.
        let quiet = lines(&[(Some(1), "working"), (Some(1), "killed"), (None, "ERROR: build step 1 failed")]);
        assert_eq!(first_error(&quiet.lines, Some(1)), Some(1));
        // The step left no line at all: anything that says an error.
        assert_eq!(first_error(&quiet.lines, Some(4)), Some(2));
        assert_eq!(first_error(&[], Some(1)), None);
    }

    #[test]
    fn a_word_in_a_name_or_a_warning_is_not_an_error() {
        for mentions in [
            "npm warn deprecated node-domexception@1.0.0: Use your platform's native DOMException instead",
            "WARNING: the error budget is low",
            "warn - failed to read a cache",
            "Creating Revision...failed",
            "Compiling src/error-handler.ts",
            "    at Object.fail_fast (/workspace/lib/failure_mode.js:3:1)",
            "FAILURES_ALLOWED=3",
            "added 3 packages, 0 errors",
            "",
        ] {
            assert!(!says_error(mentions), "{mentions}");
        }
        for says in [
            "npm error code 1",
            "npm ERR! code ELIFECYCLE",
            "Deployment failed",
            "ERROR: (gcloud.run.deploy) The user-provided container failed to start",
            "TypeError: x is not a function",
            " FAIL  src/shop.test.ts > checkout",
            "Test Files  1 failed | 19 passed (20)",
            "sh: tsc: not found",
            "fatal: could not read from remote repository.",
            "panic: runtime error: index out of range",
            "error[E0308]: mismatched types",
            "#12 ERROR: process \"/bin/sh -c npm ci\" did not complete successfully: exit code: 1",
        ] {
            assert!(says_error(says), "{says}");
        }
    }

    #[test]
    fn a_failure_keeps_the_lines_that_tell() {
        let b = failed();
        let mut items: Vec<(Option<usize>, String)> = vec![(Some(0), "Error: another step".to_string())];
        items.extend((0..100).map(|i| (Some(1), format!("line {i}"))));
        items[60].1 = "  Error: boom ya29.a0secret \u{1b}[2J".to_string();
        items.push((None, "ERROR: build step 1 failed".to_string()));
        let log = Log { lines: items.into_iter().map(|(step, text)| LogLine { step, text }).collect(), cut: true };
        let f = Failure::of(&b, &log);
        assert_eq!((f.step, f.first_error, f.said, f.lines, f.cut), (Some(1), Some(60), true, 102, true));
        let at: Vec<usize> = f.excerpt.iter().map(|(i, _)| *i).collect();
        assert_eq!(at.first(), Some(&55), "five lines before");
        assert!(at.contains(&80), "twenty after");
        assert!(!at.contains(&81), "and then a gap");
        assert_eq!(at.last(), Some(&100), "to the end of the step, not Cloud Build's own lines");
        assert!(at.contains(&91));
        let text = f.first_error_text().unwrap();
        assert!(text.starts_with("  Error: boom [redacted]"), "{text}");
        assert!(!text.contains('\u{1b}'), "{text}");
        let json = f.json(&b, 0.0);
        assert_eq!(json["step"]["id"], "build-or-fail");
        assert_eq!(json["step"]["exitCode"], 1);
        assert_eq!(json["firstErrorLine"], 61);
        assert_eq!(json["logLines"][0]["line"], 56);
        assert_eq!(json["type"], "USER_BUILD_STEP");

        // Before any step ran, or with nothing logged.
        let mut early = b.clone();
        early.steps.clear();
        let f = Failure::of(&early, &lines(&[(None, "FETCHSOURCE"), (None, "fatal: could not read from remote")]));
        assert_eq!((f.step, f.first_error), (None, Some(1)));
        assert_eq!(f.excerpt.len(), 2);
        // A step that dies without saying the word: its end, not called an error.
        let quiet = Failure::of(&b, &lines(&[(Some(1), "working"), (Some(1), "killed")]));
        assert_eq!((quiet.first_error, quiet.said, quiet.first_error_text()), (Some(1), false, None));
        assert_eq!(quiet.json(&b, 0.0)["firstErrorLine"], Value::Null);
        assert_eq!(quiet.excerpt.len(), 2);
        let empty = Failure::of(&b, &Log::default());
        assert!(empty.excerpt.is_empty() && empty.first_error.is_none());
        assert_eq!(empty.first_error_text(), None);
    }

    #[test]
    fn a_failed_build_is_read_with_its_log_and_how_its_app_stands() {
        let fake = MockServer::start(vec![]);
        fake.route("/services/shop", Reply::json(200, &service("shop", "2026-11-01T00:00:00Z")));
        fake.route("create_time", Reply::json(200, "{}"));
        fake.route("/builds/1a2b3c4d-1111-2222-3333-444455556666", Reply::json(200, FAILED_BUILD));
        fake.route("/builds?", Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD}]}}"#)));
        fake.route(
            "entries:list",
            Reply::json(
                200,
                r#"{"entries":[{"textPayload":"Step #1 - \"build-or-fail\": Error: boom","labels":{"build_step":"Step #1 - \"build-or-fail\""}}]}"#,
            ),
        );
        let d = Detail::load(&client(&fake), AppsConvention::Whiteghost, "1a2b3c4d-1111-2222-3333-444455556666", false)
            .unwrap();
        assert_eq!(d.app.as_deref(), Some("shop"));
        let failure = d.failure().unwrap();
        assert_eq!(failure.first_error_text(), Some("Error: boom"), "without the step's name in front");
        assert!(d.since().unwrap().starts_with("last deploy failed"), "{:?}", d.since());
        let now = parse_rfc3339("2026-11-05T12:00:00Z").unwrap();
        let json = d.json(now);
        assert_eq!(json["failure"]["step"]["number"], 1);
        assert_eq!(json["lastDeployFailed"]["app"], "shop");
        assert_eq!(json["steps"][2]["status"], "QUEUED");

        // What it means after a later success, or a later failure.
        let other = Failing {
            app: "shop".into(),
            build: Build { id: "9f9f9f9f-x".into(), ..Build::default() },
            retrying: false,
            serving: "r".into(),
            serving_since: None,
        };
        assert_eq!(since_text("a", &Deploys::default()), "a later build succeeded");
        let gone = Deploys { not_services: vec!["shop".into()], ..Deploys::default() };
        assert_eq!(since_text("a", &gone), "it has no Cloud Run service");
        let later = Deploys { failing: vec![other], ..Deploys::default() };
        assert!(
            since_text("a", &later).starts_with("a later build, 9f9f9f9f, failed too"),
            "{}",
            since_text("a", &later)
        );

        // A build that went well reads no log unless asked.
        let fake = MockServer::start(vec![Reply::json(
            200,
            r#"{"id":"0123456789012345678901234567890123456","status":"SUCCESS"}"#,
        )]);
        let d =
            Detail::load(&client(&fake), AppsConvention::Whiteghost, "0123456789012345678901234567890123456", false)
                .unwrap();
        assert!(d.log.is_none() && d.deploy.is_none() && d.failure().is_none());
        assert_eq!(fake.requests().len(), 1);
    }
}
