//! Database: a project's Cloud SQL instances, watched from the outside, through Google's admin and
//! monitoring APIs. gcli never connects to a database and reads none of its tables, data or
//! credentials. When many apps share one instance, the check that matters most is how close it is
//! to `max_connections`, since running out takes every app down at once.
//! The terminal UI, the commands and the MCP server all read an instance through here.
//!
//! A database is not an app: apps can share one database (a schema each), and Cloud Monitoring
//! counts connections per database only, so connections are shown by database's name and nothing
//! here claims to know which app holds them.

pub mod cmd;

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::apps::{Column, Tone};
use crate::findings::{self, Finding, Level};
use crate::gcp::error::redact;
use crate::gcp::monitoring::{self, Query, Series, rfc3339};
use crate::gcp::sql::{self, BackupRun, Instance};
use crate::gcp::{Gcp, GcpError, Result};
use crate::tui::chart::{Agg, columns as chart_columns, sparkline_to};
use crate::util::{ellipsize, fmt_utc, parse_rfc3339};

/// Connections, as a share of `max_connections`, now and at the peak of the last 24 hours.
pub const CONNECTIONS_WARN: f64 = 0.80;
pub const CONNECTIONS_BAD: f64 = 0.95;
/// Disk in use, as a share of the disk. With auto-resize on it is at most a warning.
pub const DISK_WARN: f64 = 0.85;
pub const DISK_BAD: f64 = 0.95;
/// CPU or memory, averaged over the last hour.
pub const LOAD_WARN: f64 = 0.90;
/// How old the last successful backup may be, in seconds.
pub const BACKUP_MAX_AGE: f64 = 36.0 * 3600.0;
/// How far transaction ids are towards wraparound.
pub const WRAPAROUND_WARN: f64 = 0.50;
pub const WRAPAROUND_BAD: f64 = 0.80;

const RESOURCE: &str = "cloudsql_database";
const NUM_BACKENDS: &str = "cloudsql.googleapis.com/database/postgresql/num_backends";
const BACKENDS_BY_STATE: &str = "cloudsql.googleapis.com/database/postgresql/num_backends_by_state";
const DEADLOCKS: &str = "cloudsql.googleapis.com/database/postgresql/deadlock_count";
const WRAPAROUND: &str = "cloudsql.googleapis.com/database/postgresql/transaction_id_utilization";
const CPU: &str = "cloudsql.googleapis.com/database/cpu/utilization";
const MEMORY: &str = "cloudsql.googleapis.com/database/memory/utilization";
const DISK: &str = "cloudsql.googleapis.com/database/disk/utilization";
const DISK_BYTES: &str = "cloudsql.googleapis.com/database/disk/bytes_used";

const HOUR: f64 = 3600.0;
const DAY: f64 = 86_400.0;
/// A reading older than this is not "now": Cloud Monitoring runs a few minutes behind, no more.
const FRESH: f64 = 900.0;
/// Growth is estimated from this much of the disk's past, and only with at least a day of it.
const GROWTH_WINDOW: f64 = 7.0 * DAY;
/// Days until full past this are noise, not a forecast.
const FULL_HORIZON_DAYS: f64 = 365.0;
/// Google reports the disk's size in "GB" that are really binary: bytes are shown on the same scale.
const GB: f64 = 1024.0 * 1024.0 * 1024.0;
/// Instances read at the same time.
const READS_AT_ONCE: usize = 4;

/// Connections to one database.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Database {
    pub name: String,
    /// The minute [`Metrics::states_at`], by state. `idle in transaction` holds locks while it
    /// does nothing, so it is kept apart from plain `idle`.
    pub active: f64,
    pub idle: f64,
    pub idle_in_transaction: f64,
    /// The most in any minute of the last 24 hours. Peaks of different databases fall at different
    /// times: they do not add up to the instance's peak.
    pub peak: f64,
    /// In the last 24 hours.
    pub deadlocks: f64,
}

impl Database {
    pub fn now(&self) -> f64 {
        self.active + self.idle + self.idle_in_transaction
    }
}

/// What Cloud Monitoring says of an instance. Shares are fractions (0.85 is 85%); series are
/// (Unix seconds, value), oldest first.
#[derive(Debug, Clone, Default)]
pub struct Metrics {
    /// The last 24 hours, which the charts and the peak cover.
    pub window: (f64, f64),
    /// Connections to the whole instance, one point per minute. Empty if it is not Postgres.
    pub connections: Vec<(f64, f64)>,
    /// Most connections first; an instance's every database in the last 24 hours.
    pub databases: Vec<Database>,
    /// The minute of the newest connections by state, when there is one.
    pub states_at: Option<f64>,
    pub cpu: Vec<(f64, f64)>,
    pub memory: Vec<(f64, f64)>,
    pub disk: Vec<(f64, f64)>,
    /// Bytes used over [`GROWTH_WINDOW`].
    pub disk_bytes: Vec<(f64, f64)>,
    pub wraparound: Option<f64>,
}

/// The newest value, if it is recent enough to call it now.
fn latest(points: &[(f64, f64)], now: f64) -> Option<f64> {
    points.last().filter(|p| now - p.0 <= FRESH).map(|p| p.1)
}

/// The average of the points after `since`.
fn mean_since(points: &[(f64, f64)], since: f64) -> Option<f64> {
    let values: Vec<f64> = points.iter().filter(|p| p.0 > since).map(|p| p.1).collect();
    (!values.is_empty()).then(|| values.iter().sum::<f64>() / values.len() as f64)
}

/// Units per day, by least squares: the ends of a disk's usage are noisy (temporary files, the
/// write-ahead log), and a difference between them would swing with them. `None` under a day.
fn growth_per_day(points: &[(f64, f64)]) -> Option<f64> {
    if points.last()?.0 - points.first()?.0 < DAY {
        return None;
    }
    let n = points.len() as f64;
    let (mean_t, mean_v) = (points.iter().map(|p| p.0).sum::<f64>() / n, points.iter().map(|p| p.1).sum::<f64>() / n);
    let across: f64 = points.iter().map(|p| (p.0 - mean_t) * (p.1 - mean_v)).sum();
    let spread: f64 = points.iter().map(|p| (p.0 - mean_t).powi(2)).sum();
    (spread > 0.0).then(|| across / spread * DAY)
}

impl Metrics {
    pub fn connections_now(&self, now: f64) -> Option<f64> {
        latest(&self.connections, now)
    }

    /// When the most connections were, and how many.
    pub fn peak(&self) -> Option<(f64, f64)> {
        self.connections.iter().copied().reduce(|best, p| if p.1 > best.1 { p } else { best })
    }

    pub fn cpu_last_hour(&self, now: f64) -> Option<f64> {
        mean_since(&self.cpu, now - HOUR)
    }

    pub fn memory_last_hour(&self, now: f64) -> Option<f64> {
        mean_since(&self.memory, now - HOUR)
    }

    pub fn disk_now(&self, now: f64) -> Option<f64> {
        latest(&self.disk, now)
    }

    pub fn disk_bytes_now(&self) -> Option<f64> {
        self.disk_bytes.last().map(|p| p.1)
    }

    /// At the last days' growth, when usage is growing. The disk's real size comes from the two
    /// metrics: it is a little under what the settings call it.
    pub fn days_until_full(&self, now: f64) -> Option<f64> {
        let (share, used) = (self.disk_now(now).filter(|s| *s > 0.0)?, self.disk_bytes_now()?);
        let growth = growth_per_day(&self.disk_bytes).filter(|g| *g > 0.0)?;
        Some((used / share - used).max(0.0) / growth)
    }

    /// The databases with connections now, then those without (when connections by state were
    /// read; otherwise all of them come first, by their peak).
    pub fn busy_and_quiet(&self) -> (&[Database], &[Database]) {
        match self.states_at {
            Some(_) => self.databases.split_at(self.databases.partition_point(|d| d.now() > 0.0)),
            None => (&self.databases, &[]),
        }
    }
}

/// What the backup runs say.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Backups {
    /// When the newest successful backup ended.
    pub last_success: Option<f64>,
    /// The latest attempt that finished, when it did not succeed.
    pub latest_failed: Option<FailedBackup>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FailedBackup {
    pub at: Option<f64>,
    pub status: String,
    pub message: String,
}

impl Backups {
    /// From runs newest first. One still running or queued has not failed yet, a skipped one was not
    /// tried, and a deleted one says nothing of the last attempt.
    fn of(runs: &[BackupRun]) -> Backups {
        let last_success = runs.iter().find(|r| r.succeeded()).and_then(BackupRun::ended);
        let latest_failed = runs
            .iter()
            .find(|r| matches!(r.status.as_str(), "SUCCESSFUL" | "FAILED" | "OVERDUE"))
            .filter(|r| !r.succeeded())
            .map(|r| FailedBackup { at: r.ended(), status: r.status.clone(), message: redact(&r.error.message) });
        Backups { last_success, latest_failed }
    }
}

/// Everything read of one instance. `None` is what the instance does not have (a stopped one
/// reports no metrics, a replica keeps no backups); `Some(Err)` what could not be read.
#[derive(Debug, Clone)]
pub struct Health {
    pub instance: Instance,
    /// Unix seconds: the "now" of all that was read.
    pub read_at: f64,
    pub metrics: Option<Result<Metrics>>,
    pub backups: Option<Result<Backups>>,
}

/// Google reports metrics of a Cloud SQL instance that is on.
fn reports_metrics(i: &Instance) -> bool {
    !i.is_external() && !i.stopped()
}

/// A replica has no backups of its own, and a stopped instance takes none.
fn has_backups(i: &Instance) -> bool {
    !i.is_replica() && !i.is_external() && !i.stopped()
}

fn joined<T>(r: std::thread::Result<Result<T>>) -> Result<T> {
    r.unwrap_or_else(|_| Err(GcpError::Network("a metrics read failed unexpectedly".into())))
}

/// `timeSeries.list` for one instance.
#[derive(Clone, Copy)]
struct Asker<'a> {
    gcp: &'a Gcp,
    database_id: &'a str,
    now: f64,
}

impl Asker<'_> {
    fn series(
        self,
        metric: &str,
        back: f64,
        period: u64,
        aligner: &str,
        reducer: &str,
        by: &[&str],
    ) -> Result<Vec<Series>> {
        monitoring::query(
            self.gcp,
            &Query {
                metric,
                resource: RESOURCE,
                resource_labels: vec![("database_id", self.database_id)],
                window: (self.now - back, self.now),
                period,
                aligner,
                reducer,
                group_by: by,
            },
        )
    }

    /// A metric of the instance as a whole.
    fn points(self, metric: &str, back: f64, period: u64, aligner: &str, reducer: &str) -> Result<Vec<(f64, f64)>> {
        let series = self.series(metric, back, period, aligner, reducer, &[])?;
        Ok(series.first().map(|s| s.points.iter().map(|p| (p.time, p.value)).collect()).unwrap_or_default())
    }
}

/// Reads an instance's metrics. Monitoring reads are free; these calls go at the same time.
fn read_metrics(gcp: &Gcp, i: &Instance, now: f64) -> Result<Metrics> {
    let id = format!("{}:{}", gcp.project, i.name);
    let ask = Asker { gcp, database_id: &id, now };
    let usage = |metric: &str| ask.points(metric, DAY, 300, "ALIGN_MEAN", "REDUCE_MAX");
    let usage = &usage;
    let (cpu, memory, disk, disk_bytes, postgres) = std::thread::scope(|s| {
        let cpu = s.spawn(move || usage(CPU));
        let memory = s.spawn(move || usage(MEMORY));
        let disk = s.spawn(move || usage(DISK));
        let bytes = s.spawn(move || ask.points(DISK_BYTES, GROWTH_WINDOW, 21_600, "ALIGN_MEAN", "REDUCE_MAX"));
        let postgres = i.is_postgres().then(|| s.spawn(move || read_postgres(ask)));
        (
            joined(cpu.join()),
            joined(memory.join()),
            joined(disk.join()),
            joined(bytes.join()),
            postgres.map(|p| joined(p.join())),
        )
    });
    let mut metrics = postgres.transpose()?.unwrap_or_default();
    metrics.window = (now - DAY, now);
    metrics.cpu = cpu?;
    metrics.memory = memory?;
    metrics.disk = disk?;
    metrics.disk_bytes = disk_bytes?;
    Ok(metrics)
}

/// What only Postgres reports.
fn read_postgres(ask: Asker) -> Result<Metrics> {
    const BY_DATABASE: &[&str] = &["metric.label.database"];
    const BY_STATE: &[&str] = &["metric.label.database", "metric.label.state"];
    let (connections, states, peaks, deadlocks, wraparound) = std::thread::scope(|s| {
        // The maximum of each database's one sample a minute, added up: the instance, minute by
        // minute. Anything coarser adds up maxima of different moments and overstates the peak.
        let connections = s.spawn(move || ask.points(NUM_BACKENDS, DAY, 60, "ALIGN_MAX", "REDUCE_SUM"));
        let states = s.spawn(move || ask.series(BACKENDS_BY_STATE, FRESH, 60, "ALIGN_MAX", "REDUCE_SUM", BY_STATE));
        let peaks = s.spawn(move || ask.series(NUM_BACKENDS, DAY, 86_400, "ALIGN_MAX", "REDUCE_SUM", BY_DATABASE));
        let deadlocks = s.spawn(move || ask.series(DEADLOCKS, DAY, 86_400, "ALIGN_SUM", "REDUCE_SUM", BY_DATABASE));
        let wraparound = s.spawn(move || ask.points(WRAPAROUND, FRESH, 300, "ALIGN_MEAN", "REDUCE_MAX"));
        (
            joined(connections.join()),
            joined(states.join()),
            joined(peaks.join()),
            joined(deadlocks.join()),
            joined(wraparound.join()),
        )
    });
    let (databases, states_at) = databases(&states?, &peaks?, &deadlocks?);
    Ok(Metrics {
        connections: connections?,
        databases,
        states_at,
        wraparound: latest(&wraparound?, ask.now),
        ..Metrics::default()
    })
}

/// Folds the three metrics into one row per database, most connections first.
///
/// A state with nobody in it sends no point at all, so a series' own last point can be minutes old
/// and no longer true: only what was reported at the newest minute of them all counts.
fn databases(states: &[Series], peaks: &[Series], deadlocks: &[Series]) -> (Vec<Database>, Option<f64>) {
    let mut by_name: BTreeMap<&str, Database> = BTreeMap::new();
    let newest = states.iter().filter_map(|s| s.points.last()).map(|p| p.time).reduce(f64::max);
    for s in states {
        let row = by_name.entry(s.label("database")).or_default();
        let Some(p) = s.points.last().filter(|p| Some(p.time) == newest) else { continue };
        // `idle in transaction`, and `idle in transaction (aborted)`, hold on to their locks.
        match s.label("state").replace('_', " ").as_str() {
            "idle" => row.idle += p.value,
            state if state.starts_with("idle in transaction") => row.idle_in_transaction += p.value,
            _ => row.active += p.value,
        }
    }
    for s in peaks {
        let row = by_name.entry(s.label("database")).or_default();
        row.peak = s.points.iter().map(|p| p.value).fold(row.peak, f64::max);
    }
    for s in deadlocks {
        by_name.entry(s.label("database")).or_default().deadlocks += s.points.iter().map(|p| p.value).sum::<f64>();
    }
    let mut rows: Vec<Database> = by_name
        .into_iter()
        .filter(|(name, _)| !name.is_empty())
        .map(|(name, row)| Database { name: name.to_string(), ..row })
        .collect();
    rows.sort_by(|a, b| b.now().total_cmp(&a.now()).then(b.peak.total_cmp(&a.peak)).then(a.name.cmp(&b.name)));
    (rows, newest)
}

/// Reads one instance, whatever can be read of it.
pub fn read(gcp: &Gcp, instance: Instance, now: f64) -> Health {
    let i = &instance;
    let (metrics, backups) = std::thread::scope(|s| {
        let metrics = reports_metrics(i).then(|| s.spawn(move || read_metrics(gcp, i, now)));
        let backups = (has_backups(i) && i.settings.backup_configuration.enabled)
            .then(|| s.spawn(move || sql::backup_runs(gcp, &i.name).map(|runs| Backups::of(&runs))));
        (metrics.map(|m| joined(m.join())), backups.map(|b| joined(b.join())))
    });
    Health { instance, read_at: now, metrics, backups }
}

/// Reads the project's instances, or only the one named. Fails only when the instances themselves
/// cannot be listed; what cannot be read of each is said in its [`Health`].
pub fn read_all(gcp: &Gcp, only: Option<&str>, now: f64) -> Result<Vec<Health>> {
    let instances = match only {
        Some(name) => vec![sql::get(gcp, name)?],
        None => sql::list(gcp)?,
    };
    let mut all = Vec::with_capacity(instances.len());
    for chunk in instances.chunks(READS_AT_ONCE) {
        let done: Vec<Health> = std::thread::scope(|s| {
            let reads: Vec<_> = chunk.iter().map(|i| s.spawn(move || read(gcp, i.clone(), now))).collect();
            let joined = reads.into_iter().zip(chunk).map(|(read, i)| {
                read.join().unwrap_or_else(|_| Health {
                    instance: i.clone(),
                    read_at: now,
                    metrics: Some(Err(GcpError::Network("reading the instance failed unexpectedly".into()))),
                    backups: None,
                })
            });
            joined.collect()
        });
        all.extend(done);
    }
    Ok(all)
}

/// The level of a share against its two thresholds.
fn level_of(share: f64, warn: f64, bad: f64) -> Option<Level> {
    if share >= bad {
        Some(Level::Bad)
    } else if share >= warn {
        Some(Level::Warn)
    } else {
        None
    }
}

fn tone_of(level: Option<Level>) -> Tone {
    match level {
        Some(Level::Bad) => Tone::Bad,
        Some(Level::Warn) => Tone::Warn,
        None => Tone::Plain,
    }
}

/// A number of connections or deadlocks, in full: the difference between two limits matters here.
fn count(v: f64) -> String {
    format!("{v:.0}")
}

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

/// `3 h ago`, `4 days ago`.
fn ago(then: f64, now: f64) -> String {
    let hours = ((now - then) / HOUR).max(0.0);
    if hours < 48.0 { format!("{hours:.0} h ago") } else { format!("{:.0} days ago", hours / 24.0) }
}

impl Health {
    fn metrics(&self) -> Option<&Metrics> {
        self.metrics.as_ref()?.as_ref().ok()
    }

    fn connections_level(&self, m: &Metrics, limit: f64) -> Option<Level> {
        let level = |v: f64| level_of(v / limit, CONNECTIONS_WARN, CONNECTIONS_BAD);
        m.connections_now(self.read_at).and_then(level).max(m.peak().and_then(|p| level(p.1)))
    }

    fn disk_level(&self, share: f64) -> Option<Level> {
        let level = level_of(share, DISK_WARN, DISK_BAD);
        if self.instance.settings.auto_resize() { level.map(|_| Level::Warn) } else { level }
    }

    /// Days until the disk fills, when that is a question: it is growing, with auto-resize off.
    pub fn days_until_full(&self) -> Option<f64> {
        if self.instance.settings.auto_resize() {
            return None;
        }
        self.metrics()?.days_until_full(self.read_at)
    }

    /// What needs attention. Rules on what could not be read say nothing here: see [`Health::unread`].
    pub fn findings(&self) -> Vec<Finding> {
        let i = &self.instance;
        let now = self.read_at;
        let name = i.name.as_str();
        let mut out = Vec::new();
        if !i.is_runnable() {
            out.push(Finding::bad(name, format!("state is {}, not RUNNABLE", i.state)));
        } else if i.stopped() {
            let finding = Finding::bad(name, "stopped: its activation policy is NEVER, so nothing can connect");
            out.push(finding.hint("Start it, or delete it if it is no longer needed."));
        }
        if let Some(m) = self.metrics() {
            out.extend(self.connection_finding(m));
            out.extend(self.disk_finding(m));
            for (what, share) in [("CPU", m.cpu_last_hour(now)), ("memory", m.memory_last_hour(now))] {
                if let Some(s) = share.filter(|s| *s >= LOAD_WARN) {
                    out.push(Finding::warn(name, format!("{what} averaged {} over the last hour", percent(s))));
                }
            }
            if let Some(share) = m.wraparound
                && let Some(level) = level_of(share, WRAPAROUND_WARN, WRAPAROUND_BAD)
            {
                let what = format!("transaction ids are {} of the way to wraparound", percent(share));
                let hint = "Vacuum is not keeping up: look for long-running transactions and autovacuum settings.";
                out.push(Finding { level, subject: name.to_string(), what, hint: Some(hint.to_string()) });
            }
            out.extend(deadlock_finding(name, m));
        }
        self.backup_findings(&mut out);
        out
    }

    fn connection_finding(&self, m: &Metrics) -> Option<Finding> {
        let limit = self.instance.max_connections()?;
        let level = self.connections_level(m, limit)?;
        let what = format!("connections: {}", connections_text(m, Some(limit), self.read_at));
        let mut finding = Finding { level, subject: self.instance.name.clone(), what, hint: None };
        if let Some(top) = m.databases.first().filter(|d| d.now() > 0.0) {
            finding = finding.hint(format!(
                "Most are in {}: {} now, {} idle. Running out takes every app on the instance down.",
                top.name,
                count(top.now()),
                count(top.idle + top.idle_in_transaction)
            ));
        }
        Some(finding)
    }

    fn disk_finding(&self, m: &Metrics) -> Option<Finding> {
        let share = m.disk_now(self.read_at)?;
        let level = self.disk_level(share)?;
        let mut what = format!("disk {} full", percent(share));
        if let Some(days) = self.days_until_full() {
            what.push_str(&format!(", {}", full_text(days)));
        }
        let hint = if self.instance.settings.auto_resize() {
            "Auto-resize is on: Cloud SQL grows the disk by itself, up to its limit if it has one."
        } else {
            "Auto-resize is off: grow the disk, or turn it on."
        };
        Some(Finding { level, subject: self.instance.name.clone(), what, hint: Some(hint.to_string()) })
    }

    fn backup_findings(&self, out: &mut Vec<Finding>) {
        let i = &self.instance;
        let (name, now) = (i.name.as_str(), self.read_at);
        if !has_backups(i) {
            return;
        }
        if !i.settings.backup_configuration.enabled {
            out.push(Finding::bad(name, "automated backups are disabled"));
            return;
        }
        let Some(Ok(backups)) = &self.backups else { return };
        match backups.last_success {
            Some(at) if now - at > BACKUP_MAX_AGE => {
                out.push(Finding::bad(name, format!("last successful backup was {} ({})", ago(at, now), fmt_utc(at))));
            }
            Some(_) => {}
            // A new instance has not had its first backup window yet.
            None if i.created().is_some_and(|c| now - c <= BACKUP_MAX_AGE) => {}
            None => out.push(Finding::bad(name, "no successful backup on record")),
        }
        if let Some(failed) = &backups.latest_failed {
            let when = failed.at.map(|t| format!(" ({})", fmt_utc(t))).unwrap_or_default();
            let mut what = format!("latest backup attempt {}{when}", failed.status.to_lowercase());
            if !failed.message.is_empty() {
                what.push_str(&format!(": {}", failed.message));
            }
            out.push(Finding::warn(name, what));
        }
    }

    /// What should have been read and was not, one line each. A check that lacks any of it cannot say
    /// all is well.
    pub fn unread(&self) -> Vec<String> {
        let (name, now) = (self.instance.name.as_str(), self.read_at);
        let mut out = Vec::new();
        match &self.metrics {
            Some(Err(e)) => out.push(format!("{name}: metrics: {}", e.short())),
            Some(Ok(m)) => {
                let mut missing = Vec::new();
                if self.instance.is_postgres() {
                    if m.connections_now(now).is_none() {
                        missing.push("connections");
                    }
                    if m.states_at.is_none() {
                        missing.push("connections by state");
                    }
                    if m.wraparound.is_none() {
                        missing.push("transaction ids");
                    }
                }
                if m.cpu_last_hour(now).is_none() {
                    missing.push("CPU");
                }
                if m.memory_last_hour(now).is_none() {
                    missing.push("memory");
                }
                if m.disk_now(now).is_none() {
                    missing.push("disk");
                }
                if !missing.is_empty() {
                    out.push(format!("{name}: no recent data from Cloud Monitoring: {}", missing.join(", ")));
                }
            }
            None => {}
        }
        if let Some(Err(e)) = &self.backups {
            out.push(format!("{name}: backups: {}", e.short()));
        }
        out
    }

    /// What was not judged, and why, when nothing failed.
    pub fn notes(&self) -> Vec<String> {
        let i = &self.instance;
        let mut out = Vec::new();
        if i.is_postgres() && reports_metrics(i) && i.max_connections().is_none() {
            out.push(format!(
                "{}: max_connections is not set, so the limit (which depends on the tier) is unknown and connections are not judged against it",
                i.name
            ));
        }
        if !i.is_postgres() && reports_metrics(i) {
            out.push(format!("{}: connections and transaction ids are read for Postgres only", i.name));
        }
        if i.is_replica() {
            out.push(format!("{}: a replica keeps no backups of its own", i.name));
        }
        out
    }

    /// The login error among what could not be read: the whole read needs a sign-in, then.
    pub fn login_error(&self) -> Option<&GcpError> {
        let metrics = self.metrics.as_ref().and_then(|m| m.as_ref().err());
        let backups = self.backups.as_ref().and_then(|b| b.as_ref().err());
        metrics.into_iter().chain(backups).find(|e| e.is_login_required())
    }

    pub fn json(&self) -> Value {
        let i = &self.instance;
        let s = &i.settings;
        let limit = i.max_connections();
        let backup = &s.backup_configuration;
        let ip = &s.ip_configuration;
        let round = |share: f64| (share * 1000.0).round() / 10.0;
        let mut v = json!({
            "name": i.name,
            "state": i.state,
            "stopped": i.stopped(),
            "replicaOf": (!i.master_instance_name.is_empty()).then_some(&i.master_instance_name),
            "databaseVersion": i.database_version,
            "region": i.region,
            "zone": i.gce_zone,
            "tier": s.tier,
            "availability": s.availability_type,
            "connectionName": i.connection_name,
            "maxConnections": limit.map(|l| l as u64),
            "disk": {
                "sizeGb": s.data_disk_size_gb.parse::<u64>().ok(),
                "autoResize": s.auto_resize(),
                "autoResizeLimitGb": s.storage_auto_resize_limit.parse::<u64>().ok().filter(|l| *l > 0),
            },
            "backupConfig": {
                "enabled": backup.enabled,
                "startTime": backup.start_time,
                "pointInTimeRecovery": backup.point_in_time_recovery_enabled,
                "retained": backup.backup_retention_settings.as_ref().and_then(|r| r.retained_backups),
            },
            "deletionProtection": s.deletion_protection_enabled,
            "network": {
                "publicIp": ip.ipv4_enabled,
                "privateNetwork": !ip.private_network.is_empty(),
                "authorizedNetworks": ip.authorized_networks.iter().map(|n| &n.value).collect::<Vec<_>>(),
                "sslMode": ip.ssl_mode,
                "requireSsl": ip.require_ssl,
            },
            "maintenanceWindow": s.maintenance_window.as_ref().map(|w| w.text()),
            "scheduledMaintenance": i.scheduled_maintenance.as_ref().map(|m| &m.start_time),
            "readAt": rfc3339(self.read_at),
        });
        if let Some(m) = self.metrics() {
            let now = self.read_at;
            let peak = m.peak();
            let connections = m.connections_now(now);
            v["connections"] = json!({
                "now": connections,
                "percent": connections.zip(limit).map(|(c, l)| round(c / l)),
                "peak24h": peak.map(|p| p.1),
                "peakPercent": peak.zip(limit).map(|(p, l)| round(p.1 / l)),
                "peakAt": peak.map(|p| rfc3339(p.0)),
            });
            v["cpuLastHourPercent"] = json!(m.cpu_last_hour(now).map(round));
            v["memoryLastHourPercent"] = json!(m.memory_last_hour(now).map(round));
            v["diskPercent"] = json!(m.disk_now(now).map(round));
            v["diskBytesUsed"] = json!(m.disk_bytes_now());
            v["daysUntilFull"] = json!(self.days_until_full().map(|d| d.round()));
            v["transactionIdPercent"] = json!(m.wraparound.map(round));
            v["databasesAt"] = json!(m.states_at.map(rfc3339));
            v["databases"] = m.databases.iter().map(|d| database_json(d, m.states_at.is_some())).collect();
        }
        if let Some(Ok(b)) = &self.backups {
            v["lastSuccessfulBackup"] = json!(b.last_success.map(rfc3339));
            if let Some(f) = &b.latest_failed {
                v["latestBackupFailed"] = json!({ "at": f.at.map(rfc3339), "status": f.status, "message": f.message });
            }
        }
        v
    }
}

fn database_json(d: &Database, known: bool) -> Value {
    let now = |v: f64| known.then_some(v);
    json!({
        "name": d.name,
        "connections": now(d.now()),
        "active": now(d.active),
        "idle": now(d.idle),
        "idleInTransaction": now(d.idle_in_transaction),
        "peak24h": d.peak,
        "deadlocks24h": d.deadlocks,
    })
}

/// `about 12 days until full`.
fn full_text(days: f64) -> String {
    if days > FULL_HORIZON_DAYS {
        "over a year until full at this growth".to_string()
    } else {
        format!("about {days:.0} days until full at the last 7 days' growth")
    }
}

/// `60 of 500 now (12%), peak 120 (24%) at …`
fn connections_text(m: &Metrics, limit: Option<f64>, read_at: f64) -> String {
    let share = |v: f64| limit.map(|l| format!(" ({})", percent(v / l))).unwrap_or_default();
    let mut text = match (m.connections_now(read_at), limit) {
        (Some(now), Some(l)) => format!("{} of {} now{}", count(now), count(l), share(now)),
        (Some(now), None) => format!("{} now, limit unknown", count(now)),
        (None, Some(l)) => format!("none read now (limit {})", count(l)),
        (None, None) => "none read now".to_string(),
    };
    if let Some((at, peak)) = m.peak() {
        text.push_str(&format!(", peak {}{} at {}", count(peak), share(peak), fmt_utc(at)));
    }
    text
}

fn deadlock_finding(name: &str, m: &Metrics) -> Option<Finding> {
    let mut hit: Vec<&Database> = m.databases.iter().filter(|d| d.deadlocks > 0.0).collect();
    if hit.is_empty() {
        return None;
    }
    hit.sort_by(|a, b| b.deadlocks.total_cmp(&a.deadlocks).then(a.name.cmp(&b.name)));
    let total: f64 = hit.iter().map(|d| d.deadlocks).sum();
    let names: Vec<String> = hit.iter().map(|d| format!("{} ({})", d.name, count(d.deadlocks))).collect();
    let word = if total == 1.0 { "deadlock" } else { "deadlocks" };
    Some(Finding::warn(name, format!("{} {word} in the last 24 h: {}", count(total), names.join(", "))))
}

/// Everything found across `all`, the worst first: what a `--check` reports.
pub fn findings(all: &[Health]) -> Vec<Finding> {
    let mut found: Vec<Finding> = all.iter().flat_map(Health::findings).collect();
    findings::sort(&mut found);
    found
}

/// The answer of `gcli db --json` and of the MCP server.
pub fn report(env: &str, project: &str, all: &[Health]) -> Value {
    json!({
        "env": env,
        "project": project,
        "instances": all.iter().map(Health::json).collect::<Vec<_>>(),
        "findings": findings(all),
        "unread": all.iter().flat_map(Health::unread).collect::<Vec<_>>(),
        "notes": all.iter().flat_map(Health::notes).collect::<Vec<_>>(),
    })
}

/// A line about what an instance is, for the summary.
pub struct Fact {
    pub key: &'static str,
    pub text: String,
    pub tone: Tone,
}

/// What the instance is: its settings, as the Admin API says.
pub fn facts(i: &Instance) -> Vec<Fact> {
    let s = &i.settings;
    let fact = |key, text: String, tone| Fact { key, text, tone };
    let mut about = vec![i.database_version.as_str(), s.tier.as_str()];
    let place = if i.gce_zone.is_empty() { i.region.as_str() } else { i.gce_zone.as_str() };
    about.push(place);
    let availability = s.availability_type.to_lowercase();
    about.push(&availability);
    about.retain(|t| !t.is_empty());
    let mut instance = about.join(" · ");
    if !i.master_instance_name.is_empty() {
        instance.push_str(&format!(" · replica of {}", i.master_instance_name));
    }
    let mut out = vec![fact("Instance", instance, Tone::Plain)];

    if i.is_postgres() {
        out.push(match i.max_connections() {
            Some(limit) => fact("Limit", format!("max_connections {}", count(limit)), Tone::Plain),
            None => fact("Limit", "unknown: max_connections is not set, it depends on the tier".into(), Tone::Warn),
        });
    }

    let mut disk = format!("{} GB", if s.data_disk_size_gb.is_empty() { "?" } else { &s.data_disk_size_gb });
    match (s.auto_resize(), s.storage_auto_resize_limit.parse::<u64>().unwrap_or(0)) {
        (false, _) => disk.push_str(" · auto-resize off"),
        (true, 0) => disk.push_str(" · auto-resize on"),
        (true, limit) => disk.push_str(&format!(" · auto-resize on, up to {limit} GB")),
    }
    out.push(fact("Disk", disk, Tone::Plain));

    let backup = &s.backup_configuration;
    if has_backups(i) {
        if backup.enabled {
            let mut parts = vec!["on".to_string()];
            if !backup.start_time.is_empty() {
                parts.push(format!("from {} UTC", backup.start_time));
            }
            if let Some(n) = backup.backup_retention_settings.as_ref().and_then(|r| r.retained_backups) {
                parts.push(format!("keeps {n}"));
            }
            parts.push(format!(
                "point-in-time recovery {}",
                if backup.point_in_time_recovery_enabled { "on" } else { "off" }
            ));
            out.push(fact("Backups", parts.join(" · "), Tone::Plain));
        } else {
            out.push(fact("Backups", "disabled".into(), Tone::Bad));
        }
    }

    let ip = &s.ip_configuration;
    let open = ip.authorized_networks.iter().any(|n| n.value == "0.0.0.0/0");
    let mut network = Vec::new();
    if ip.ipv4_enabled {
        let nets: Vec<&str> = ip.authorized_networks.iter().map(|n| n.value.as_str()).collect();
        network.push(match nets.len() {
            0 => "public IP, no authorized networks".to_string(),
            _ => format!("public IP from {}", nets.join(", ")),
        });
    }
    if !ip.private_network.is_empty() {
        network.push("private network".to_string());
    }
    if !ip.ssl_mode.is_empty() {
        network.push(format!("SSL {}", ip.ssl_mode.to_lowercase().replace('_', " ")));
    }
    if !network.is_empty() {
        out.push(fact("Network", network.join(" · "), if open && ip.ipv4_enabled { Tone::Warn } else { Tone::Plain }));
    }

    let mut maintenance = s.maintenance_window.as_ref().map_or("any time".to_string(), |w| w.text());
    if let Some(m) = &i.scheduled_maintenance {
        let when = parse_rfc3339(&m.start_time).map_or_else(|| m.start_time.clone(), fmt_utc);
        maintenance.push_str(&format!(" · next scheduled {when}"));
    }
    out.push(fact("Maintenance", maintenance, Tone::Plain));
    out.push(fact(
        "Protection",
        format!("deletion protection {}", if s.deletion_protection_enabled { "on" } else { "off" }),
        Tone::Plain,
    ));
    out
}

/// A line of an instance's health, with its last 24 hours when the metric has them.
pub struct Gauge {
    pub key: &'static str,
    pub text: String,
    pub tone: Tone,
    pub spark: Option<String>,
}

/// `points` in `width` columns on a fixed scale.
fn spark(points: &[(f64, f64)], window: (f64, f64), width: usize, agg: Agg, max: f64) -> String {
    sparkline_to(&chart_columns(points, window, width, agg), max)
}

/// How the instance is doing: its metrics and backups, as a person reads them.
pub fn gauges(h: &Health, width: usize) -> Vec<Gauge> {
    let i = &h.instance;
    let now = h.read_at;
    let mut out = Vec::new();
    if let Some(m) = h.metrics() {
        if i.is_postgres() {
            let limit = i.max_connections();
            let scale = limit.or(m.peak().map(|p| p.1)).unwrap_or(1.0);
            let tone = tone_of(limit.and_then(|l| h.connections_level(m, l)));
            out.push(Gauge {
                key: "Connections",
                text: connections_text(m, limit, now),
                tone,
                spark: Some(spark(&m.connections, m.window, width, Agg::Max, scale)),
            });
        }
        let load = |key, share: Option<f64>, points: &[(f64, f64)]| Gauge {
            key,
            text: share.map_or("no recent data".to_string(), |s| format!("{} over the last hour", percent(s))),
            tone: tone_of(share.filter(|s| *s >= LOAD_WARN).map(|_| Level::Warn)),
            spark: Some(spark(points, m.window, width, Agg::Mean, 1.0)),
        };
        out.push(load("CPU", m.cpu_last_hour(now), &m.cpu));
        out.push(load("Memory", m.memory_last_hour(now), &m.memory));
        let share = m.disk_now(now);
        let mut disk = share.map_or("no recent data".to_string(), percent);
        if let Some(used) = m.disk_bytes_now() {
            disk.push_str(&format!(" · {:.1} GB used", used / GB));
        }
        if let Some(days) = h.days_until_full() {
            disk.push_str(&format!(" · {}", full_text(days)));
        }
        out.push(Gauge {
            key: "Disk",
            text: disk,
            tone: tone_of(share.and_then(|s| h.disk_level(s))),
            spark: Some(spark(&m.disk, m.window, width, Agg::Max, 1.0)),
        });
        if i.is_postgres() {
            out.push(Gauge {
                key: "Txn ids",
                text: m
                    .wraparound
                    .map_or("no recent data".to_string(), |s| format!("{:.1}% of the way to wraparound", s * 100.0)),
                tone: tone_of(m.wraparound.and_then(|s| level_of(s, WRAPAROUND_WARN, WRAPAROUND_BAD))),
                spark: None,
            });
        }
    }
    if let Some(Ok(b)) = &h.backups {
        let (text, tone) = match (b.last_success, &b.latest_failed) {
            (Some(at), failed) => {
                let mut text = format!("{} · {}", ago(at, now), fmt_utc(at));
                let mut tone = if now - at > BACKUP_MAX_AGE { Tone::Bad } else { Tone::Plain };
                if failed.is_some() {
                    text.push_str(" · the latest attempt failed");
                    tone = if tone == Tone::Bad { Tone::Bad } else { Tone::Warn };
                }
                (text, tone)
            }
            (None, _) => ("none on record".to_string(), Tone::Warn),
        };
        out.push(Gauge { key: "Last backup", text, tone, spark: None });
    }
    out
}

/// The columns of connections per database.
pub fn columns() -> Vec<Column> {
    let col = |head, right, drop| Column { head, right, drop };
    vec![
        col("DATABASE", false, 0),
        col("NOW", true, 0),
        col("ACTIVE", true, 0),
        col("IDLE", true, 0),
        col("IDLE IN TX", true, 2),
        col("PEAK 24H", true, 1),
        col("DEADLOCKS", false, 3),
    ]
}

/// A database's cells, in [`columns`]' order. `known`: connections by state were read.
pub fn cells(d: &Database, known: bool) -> Vec<(String, Tone)> {
    let now = |v: f64| if known { count(v) } else { "—".to_string() };
    vec![
        (d.name.clone(), Tone::Plain),
        (now(d.now()), Tone::Plain),
        (now(d.active), Tone::Plain),
        (now(d.idle), Tone::Muted),
        (now(d.idle_in_transaction), if d.idle_in_transaction > 0.0 { Tone::Warn } else { Tone::Muted }),
        (count(d.peak), Tone::Plain),
        (count(d.deadlocks), if d.deadlocks > 0.0 { Tone::Warn } else { Tone::Muted }),
    ]
}

/// `+ 3 databases with no connections now; the busiest reached 5 in the last 24 h (shop)`.
pub fn quiet_line(quiet: &[Database]) -> Option<String> {
    let busiest = quiet.iter().max_by(|a, b| a.peak.total_cmp(&b.peak))?;
    let n = quiet.len();
    let mut text = format!("+ {n} database{} with no connections now", if n == 1 { "" } else { "s" });
    if busiest.peak > 0.0 {
        let who = ellipsize(&busiest.name, 40);
        text.push_str(&format!("; the busiest reached {} in the last 24 h ({who})", count(busiest.peak)));
    }
    Some(text)
}

/// What says connections by database and state may differ from the total by a little.
pub const DATABASES_EXPLAINED: &str = "By state, at the newest minute reported. Cloud Monitoring samples this apart from the total, so they can differ by a few.";

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};
    use serde_json::json;
    use std::sync::Arc;

    pub(crate) fn now() -> f64 {
        parse_rfc3339("2026-11-05T12:00:00Z").unwrap()
    }

    /// A fake Google Cloud with one instance; a healthy one by default.
    #[derive(Clone)]
    pub(crate) struct Scene {
        /// What the metrics' and backups' times are relative to.
        pub now: f64,
        pub instance: Value,
        /// Connections to the instance, `(seconds before now, value)`, newest first.
        pub connections: Vec<(f64, f64)>,
        /// `(database, state, value, seconds before now)`.
        pub states: Vec<(&'static str, &'static str, f64, f64)>,
        pub peaks: Vec<(&'static str, f64)>,
        pub deadlocks: Vec<(&'static str, f64)>,
        pub cpu: f64,
        pub memory: f64,
        pub disk: f64,
        /// `(seconds before now, bytes)`, newest first.
        pub disk_bytes: Vec<(f64, f64)>,
        pub wraparound: f64,
        pub backups: String,
    }

    pub(crate) fn instance_json() -> Value {
        json!({
            "name": "shared-db", "state": "RUNNABLE", "databaseVersion": "POSTGRES_15", "region": "us-east1",
            "gceZone": "us-east1-b", "instanceType": "CLOUD_SQL_INSTANCE",
            "connectionName": "my-platform-123456:us-east1:shared-db", "createTime": "2026-01-01T00:00:00Z",
            "settings": {
                "tier": "db-custom-2-7680", "availabilityType": "ZONAL", "activationPolicy": "ALWAYS",
                "dataDiskSizeGb": "100", "storageAutoResize": true, "storageAutoResizeLimit": "0",
                "deletionProtectionEnabled": true,
                "databaseFlags": [{ "name": "max_connections", "value": "500" }],
                "backupConfiguration": { "enabled": true, "startTime": "02:00", "pointInTimeRecoveryEnabled": true },
                "ipConfiguration": { "ipv4Enabled": true, "authorizedNetworks": [{ "value": "192.0.2.0/24" }] },
                "maintenanceWindow": { "day": 1, "hour": 3 }
            }
        })
    }

    impl Scene {
        /// As of a fixed moment, [`now`]: for a read given that moment.
        pub(crate) fn healthy() -> Scene {
            Scene::healthy_at(now())
        }

        /// As of this very moment: for whoever reads with the clock (the UI, the MCP server).
        pub(crate) fn live() -> Scene {
            Scene::healthy_at(crate::util::now_secs().floor())
        }

        fn healthy_at(now: f64) -> Scene {
            Scene {
                now,
                instance: instance_json(),
                connections: vec![(60.0, 60.0), (120.0, 62.0), (3600.0, 120.0), (7200.0, 30.0)],
                states: vec![
                    ("orders", "idle", 50.0, 120.0),
                    ("orders", "active", 3.0, 120.0),
                    ("orders", "idle in transaction", 1.0, 120.0),
                    ("shop", "idle", 2.0, 120.0),
                ],
                peaks: vec![("orders", 110.0), ("shop", 15.0), ("template1", 0.0), ("old_copy", 5.0)],
                deadlocks: vec![("orders", 0.0), ("shop", 0.0)],
                cpu: 0.22,
                memory: 0.41,
                disk: 0.40,
                disk_bytes: vec![(0.0, 40e9), (86_400.0, 40e9), (6.0 * 86_400.0, 40e9)],
                wraparound: 0.03,
                backups: json!({ "items": [{
                    "status": "SUCCESSFUL", "startTime": rfc3339(now - 5.0 * HOUR), "endTime": rfc3339(now - 4.9 * HOUR)
                }]})
                .to_string(),
            }
        }

        /// `(seconds before now, value)`, as Google sends a series: newest first.
        fn series(&self, labels: Value, points: &[(f64, f64)]) -> Value {
            let points: Vec<Value> = points
                .iter()
                .map(|(ago, v)| {
                    json!({ "interval": { "endTime": rfc3339(self.now - ago) }, "value": { "doubleValue": v } })
                })
                .collect();
            json!({ "metric": { "labels": labels }, "resource": { "labels": { "project_id": "p" } }, "points": points })
        }

        fn one(&self, points: &[(f64, f64)]) -> String {
            json!({ "timeSeries": [self.series(json!({}), points)] }).to_string()
        }

        /// A share, steady over the last 24 hours in 5-minute points.
        fn steady(&self, share: f64) -> String {
            let points: Vec<(f64, f64)> = (0..288).map(|i| (f64::from(i) * 300.0, share)).collect();
            self.one(&points)
        }

        pub(crate) fn serve(&self) -> MockServer {
            let fake = MockServer::start(vec![]);
            self.route_on(&fake);
            fake
        }

        /// The routes of this scene on `fake`. The more specific come first.
        pub(crate) fn route_on(&self, fake: &MockServer) {
            let by_db = |rows: &[(&str, f64)]| -> String {
                let list: Vec<Value> =
                    rows.iter().map(|(d, v)| self.series(json!({ "database": d }), &[(0.0, *v)])).collect();
                json!({ "timeSeries": list }).to_string()
            };
            fake.route("/backupRuns", Reply::json(200, &self.backups));
            let name = self.instance["name"].as_str().unwrap_or_default().to_string();
            fake.route(&format!("/instances/{name}"), Reply::json(200, &self.instance.to_string()));
            fake.route("/instances", Reply::json(200, &json!({ "items": [self.instance] }).to_string()));
            let states: Vec<Value> = self
                .states
                .iter()
                .map(|(d, s, v, ago)| {
                    self.series(json!({ "database": d, "state": s }), &[(*ago, *v), (ago + 60.0, 1.0)])
                })
                .collect();
            fake.route("num_backends_by_state", Reply::json(200, &json!({ "timeSeries": states }).to_string()));
            fake.route("deadlock_count", Reply::json(200, &by_db(&self.deadlocks)));
            fake.route(
                "alignmentPeriod=86400s&aggregation.perSeriesAligner=ALIGN_MAX",
                Reply::json(200, &by_db(&self.peaks)),
            );
            fake.route("num_backends%22", Reply::json(200, &self.one(&self.connections)));
            fake.route("cpu%2Futilization", Reply::json(200, &self.steady(self.cpu)));
            fake.route("memory%2Futilization", Reply::json(200, &self.steady(self.memory)));
            fake.route("disk%2Futilization", Reply::json(200, &self.steady(self.disk)));
            fake.route("disk%2Fbytes_used", Reply::json(200, &self.one(&self.disk_bytes)));
            fake.route("transaction_id_utilization", Reply::json(200, &self.one(&[(0.0, self.wraparound)])));
        }

        /// Everything read from the fake, as `read_all` gives it.
        pub(crate) fn read(&self) -> Health {
            let fake = self.serve();
            read_all(&client(&fake), None, self.now).unwrap().remove(0)
        }

        fn found(&self) -> Vec<(Level, String)> {
            let mut found = self.read().findings();
            findings::sort(&mut found);
            found.into_iter().map(|f| (f.level, f.what)).collect()
        }

        fn settings(mut self, f: impl FnOnce(&mut Value)) -> Scene {
            f(&mut self.instance["settings"]);
            self
        }

        fn at(mut self, connections: f64) -> Scene {
            self.connections = vec![(60.0, connections)];
            self
        }
    }

    pub(crate) fn client(fake: &MockServer) -> Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()))
    }

    fn only(found: Vec<(Level, String)>) -> (Level, String) {
        assert_eq!(found.len(), 1, "{found:?}");
        found.into_iter().next().unwrap()
    }

    #[test]
    fn a_healthy_instance_has_nothing_to_say() {
        let h = Scene::healthy().read();
        assert!(h.findings().is_empty(), "{:?}", h.findings());
        assert!(h.unread().is_empty() && h.notes().is_empty(), "{:?} {:?}", h.unread(), h.notes());
    }

    #[test]
    fn reads_only_and_asks_the_instances_metrics() {
        let scene = Scene::healthy();
        let fake = scene.serve();
        let all = read_all(&client(&fake), None, now()).unwrap();
        assert_eq!(all.len(), 1);
        let requests = fake.requests();
        assert!(requests.iter().all(|r| r.method == "GET"), "it only reads");
        let of = |needle: &str| requests.iter().filter(|r| r.path.contains(needle)).count();
        assert_eq!(of("timeSeries"), 9, "one call per question");
        assert!(requests.iter().filter(|r| r.path.contains("timeSeries")).all(|r| r.path.contains(
            "resource.type%20%3D%20%22cloudsql_database%22%20AND%20resource.label.database_id%20%3D%20%22my-platform-123456%3Ashared-db%22"
        )));
        let total = requests
            .iter()
            .find(|r| r.path.contains("num_backends%22") && r.path.contains("alignmentPeriod=60s"))
            .expect("connections minute by minute");
        assert!(
            total.path.contains("perSeriesAligner=ALIGN_MAX&aggregation.crossSeriesReducer=REDUCE_SUM"),
            "{}",
            total.path
        );
        assert!(!total.path.contains("groupByFields"), "the instance as a whole: {}", total.path);
        let states = requests.iter().find(|r| r.path.contains("num_backends_by_state")).unwrap();
        assert!(
            states.path.contains("groupByFields=metric.label.database&aggregation.groupByFields=metric.label.state")
        );
        assert_eq!(of("/instances?maxResults=100"), 1);
        assert_eq!(of("/backupRuns"), 1);
    }

    #[test]
    fn connections_per_database_most_first_with_idle_and_active() {
        let m_owner = Scene::healthy().read();
        let m = m_owner.metrics().unwrap();
        let names: Vec<&str> = m.databases.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["orders", "shop", "old_copy", "template1"], "most connections, then the peak");
        let orders = &m.databases[0];
        assert_eq!((orders.now(), orders.active, orders.idle, orders.idle_in_transaction), (54.0, 3.0, 50.0, 1.0));
        assert_eq!(orders.peak, 110.0);
        let (busy, quiet) = m.busy_and_quiet();
        assert_eq!((busy.len(), quiet.len()), (2, 2));
        assert_eq!(
            quiet_line(quiet).unwrap(),
            "+ 2 databases with no connections now; the busiest reached 5 in the last 24 h (old_copy)"
        );
        assert_eq!(quiet_line(&[]), None);
        assert_eq!(cells(orders, true)[4], ("1".to_string(), Tone::Warn));
        assert_eq!(cells(orders, true).len(), columns().len());
        assert_eq!(cells(orders, false)[1].0, "—");
    }

    #[test]
    fn a_state_no_longer_reported_does_not_count() {
        // Sparse series: `active` last said 5 at an older minute and no longer exists.
        let mut scene = Scene::healthy();
        scene.states =
            vec![("orders", "idle", 10.0, 120.0), ("orders", "active", 5.0, 300.0), ("gone", "idle", 2.0, 600.0)];
        let h = scene.read();
        let m = h.metrics().unwrap();
        let orders = m.databases.iter().find(|d| d.name == "orders").unwrap();
        assert_eq!((orders.now(), orders.active, orders.idle), (10.0, 0.0, 10.0));
        let gone = m.databases.iter().find(|d| d.name == "gone").unwrap();
        assert_eq!(gone.now(), 0.0, "listed, with nobody in it now");
        assert_eq!(m.states_at, Some(now() - 120.0));
    }

    #[test]
    fn the_instances_peak_is_the_busiest_minute_not_a_sum_of_peaks() {
        let h = Scene::healthy().read();
        let m = h.metrics().unwrap();
        assert_eq!(m.peak(), Some((now() - 3600.0, 120.0)));
        assert_eq!(m.connections_now(now()), Some(60.0));
        assert!(m.databases.iter().map(|d| d.peak).sum::<f64>() > 120.0);
    }

    #[test]
    fn connections_warn_at_80_percent_and_are_bad_at_95_now() {
        let scene = Scene::healthy();
        assert!(scene.clone().at(399.0).found().is_empty(), "under 80%");
        let (level, what) = only(scene.clone().at(400.0).found());
        assert_eq!(level, Level::Warn);
        assert!(what.starts_with("connections: 400 of 500 now (80%)"), "{what}");
        assert_eq!(only(scene.clone().at(474.0).found()).0, Level::Warn, "under 95%");
        let (level, what) = only(scene.at(475.0).found());
        assert_eq!(level, Level::Bad);
        assert!(what.contains("475 of 500 now (95%)"), "{what}");
    }

    #[test]
    fn connections_are_judged_at_the_peak_of_the_day_too() {
        let mut scene = Scene::healthy();
        scene.connections = vec![(60.0, 50.0), (5.0 * 3600.0, 400.0)];
        let (level, what) = only(scene.found());
        assert_eq!(level, Level::Warn);
        assert!(what.contains("50 of 500 now (10%), peak 400 (80%) at 2026-11-05 07:00 UTC"), "{what}");
        let mut scene = Scene::healthy();
        scene.connections = vec![(60.0, 50.0), (5.0 * 3600.0, 490.0)];
        let found = scene.read().findings();
        assert_eq!((found.len(), found[0].level), (1, Level::Bad), "{found:?}");
        assert!(found[0].hint.as_ref().unwrap().contains("Most are in orders: 54 now, 51 idle"), "{found:?}");
    }

    #[test]
    fn an_unknown_limit_is_said_and_no_percentage_computed() {
        let mut scene = Scene::healthy().at(10_000.0);
        scene.instance["settings"]["databaseFlags"] = json!([{ "name": "log_connections", "value": "on" }]);
        let h = scene.read();
        assert!(h.findings().is_empty(), "nothing to judge against: {:?}", h.findings());
        assert!(h.unread().is_empty());
        assert!(h.notes()[0].contains("max_connections is not set"), "{:?}", h.notes());
        let v = h.json();
        assert_eq!(v["maxConnections"], Value::Null);
        assert_eq!(v["connections"]["percent"], Value::Null);
        assert_eq!(v["connections"]["peakPercent"], Value::Null);
        assert_eq!(v["connections"]["now"], 10_000.0);
        let gauge = gauges(&h, 8).remove(0);
        assert!(!gauge.text.contains('%') && !gauge.text.contains(" of "), "{}", gauge.text);
        assert!(facts(&h.instance).iter().any(|f| f.key == "Limit" && f.text.starts_with("unknown")));
    }

    #[test]
    fn a_stale_reading_is_not_now() {
        let mut scene = Scene::healthy();
        scene.connections = vec![(FRESH + 60.0, 475.0)];
        let h = scene.read();
        assert_eq!(h.metrics().unwrap().connections_now(now()), None);
        let found = h.findings();
        assert_eq!(found[0].level, Level::Bad, "still the peak of the day");
        assert!(found[0].what.starts_with("connections: none read now (limit 500), peak 475 (95%)"), "{found:?}");
        assert!(h.unread()[0].contains("connections"), "{:?}", h.unread());
        let mut scene = Scene::healthy();
        scene.connections = vec![];
        scene.states = vec![];
        let h = scene.read();
        assert!(h.findings().is_empty());
        assert!(
            h.unread()[0].contains("no recent data from Cloud Monitoring: connections, connections by state"),
            "{:?}",
            h.unread()
        );
    }

    #[test]
    fn disk_warns_at_85_and_is_bad_at_95_unless_it_grows_by_itself() {
        let off = || Scene::healthy().settings(|s| s["storageAutoResize"] = json!(false));
        let with_disk = |scene: Scene, disk: f64| Scene { disk, ..scene };
        assert!(with_disk(off(), 0.8499).found().is_empty());
        assert_eq!(only(with_disk(off(), 0.85).found()).0, Level::Warn);
        assert_eq!(only(with_disk(off(), 0.9499).found()).0, Level::Warn);
        let (level, what) = only(with_disk(off(), 0.95).found());
        assert_eq!(level, Level::Bad);
        assert!(what.starts_with("disk 95% full"), "{what}");
        // Auto-resize on: at most a warning, whatever the share.
        let on = Scene::healthy();
        assert!(with_disk(on.clone(), 0.8499).found().is_empty());
        assert_eq!(only(with_disk(on.clone(), 0.85).found()).0, Level::Warn);
        let (level, what) = only(with_disk(on, 0.99).found());
        assert_eq!(level, Level::Warn);
        assert!(!what.contains("until full"), "{what}");
    }

    #[test]
    fn days_until_full_come_from_the_last_weeks_growth_with_auto_resize_off() {
        let week: Vec<(f64, f64)> = (0..28).map(|i| (f64::from(i) * 21_600.0, 90e9 - f64::from(i) * 0.25e9)).collect();
        // 1 GB a day, 90 GB used at 90%: the disk is 100 GB and 10 are left.
        let scene = Scene { disk: 0.90, disk_bytes: week.clone(), ..Scene::healthy() }
            .settings(|s| s["storageAutoResize"] = json!(false));
        let h = scene.read();
        let days = h.days_until_full().unwrap();
        assert!((days - 10.0).abs() < 1e-6, "{days}");
        assert_eq!(h.json()["daysUntilFull"], 10.0);
        let (level, what) = only(scene.found());
        assert_eq!(level, Level::Warn);
        assert!(what.contains("disk 90% full, about 10 days until full"), "{what}");
        // With auto-resize on, it is no question.
        let on = Scene { disk: 0.90, disk_bytes: week, ..Scene::healthy() };
        assert_eq!(on.read().days_until_full(), None);
        // Shrinking or flat: it never fills.
        let shrinking: Vec<(f64, f64)> =
            (0..28).map(|i| (f64::from(i) * 21_600.0, 90e9 + f64::from(i) * 0.25e9)).collect();
        let scene = Scene { disk: 0.90, disk_bytes: shrinking, ..Scene::healthy() }
            .settings(|s| s["storageAutoResize"] = json!(false));
        assert_eq!(scene.read().days_until_full(), None);
        // Less than a day of history: no estimate.
        let scene = Scene { disk: 0.90, disk_bytes: vec![(0.0, 90e9), (3600.0, 89e9)], ..Scene::healthy() }
            .settings(|s| s["storageAutoResize"] = json!(false));
        assert_eq!(scene.read().days_until_full(), None);
        assert_eq!(full_text(1000.0), "over a year until full at this growth");
    }

    #[test]
    fn a_noisy_disk_does_not_swing_the_estimate() {
        // The same 1 GB a day, with both ends far off the trend.
        let mut noisy: Vec<(f64, f64)> =
            (0..28).map(|i| (f64::from(i) * 21_600.0, 90e9 - f64::from(i) * 0.25e9)).collect();
        noisy[0].1 += 0.5e9;
        noisy[27].1 += 0.5e9;
        let oldest_first: Vec<(f64, f64)> = noisy.iter().rev().map(|(ago, v)| (-ago, *v)).collect();
        let growth = growth_per_day(&oldest_first).unwrap();
        assert!((growth - 1e9).abs() < 0.05e9, "{growth}");
    }

    #[test]
    fn cpu_and_memory_warn_at_90_percent_over_the_last_hour() {
        assert!(Scene { cpu: 0.8999, memory: 0.8999, ..Scene::healthy() }.found().is_empty());
        let (level, what) = only(Scene { cpu: 0.90, ..Scene::healthy() }.found());
        assert_eq!((level, what.as_str()), (Level::Warn, "CPU averaged 90% over the last hour"));
        let (level, what) = only(Scene { memory: 0.97, ..Scene::healthy() }.found());
        assert_eq!((level, what.as_str()), (Level::Warn, "memory averaged 97% over the last hour"));
        // Only the last hour counts: a busy morning and a quiet last hour say nothing.
        let cpu = vec![(now() - 7200.0, 1.0), (now() - 3600.0, 1.0), (now() - 1800.0, 0.5), (now(), 0.5)];
        let m = Metrics { cpu, ..Metrics::default() };
        assert_eq!(m.cpu_last_hour(now()), Some(0.5));
    }

    fn with_backups(backups: Value) -> Scene {
        Scene { backups: backups.to_string(), ..Scene::healthy() }
    }

    fn successful(at: f64) -> Value {
        json!({ "status": "SUCCESSFUL", "startTime": rfc3339(at - 60.0), "endTime": rfc3339(at) })
    }

    #[test]
    fn backups_disabled_are_bad() {
        let scene = Scene::healthy().settings(|s| s["backupConfiguration"]["enabled"] = json!(false));
        let (level, what) = only(scene.found());
        assert_eq!((level, what.as_str()), (Level::Bad, "automated backups are disabled"));
        let fake = scene.serve();
        read_all(&client(&fake), None, now()).unwrap();
        assert!(!fake.requests().iter().any(|r| r.path.contains("/backupRuns")), "nothing to read");
    }

    #[test]
    fn the_last_successful_backup_is_bad_past_36_hours() {
        let found = with_backups(json!({ "items": [successful(now() - 36.0 * 3600.0)] })).found();
        assert!(found.is_empty(), "at 36 hours it still counts: {found:?}");
        let (level, what) = only(with_backups(json!({ "items": [successful(now() - 36.0 * 3600.0 - 1.0)] })).found());
        assert_eq!(level, Level::Bad);
        assert!(what.starts_with("last successful backup was 36 h ago (2026-11-03 23:59 UTC)"), "{what}");
        let (level, what) = only(with_backups(json!({ "items": [] })).found());
        assert_eq!((level, what.as_str()), (Level::Bad, "no successful backup on record"));
        // A new instance has had no window yet.
        let mut scene = with_backups(json!({ "items": [] }));
        scene.instance["createTime"] = json!(rfc3339(now() - 3600.0));
        assert!(scene.found().is_empty());
    }

    #[test]
    fn the_latest_attempt_failing_is_a_warning() {
        let failed = json!({
            "status": "FAILED", "startTime": rfc3339(now() - 3600.0), "endTime": rfc3339(now() - 3000.0),
            "error": { "code": "ERR", "message": "out of luck token=ya29.a0secret123" }
        });
        let (level, what) =
            only(with_backups(json!({ "items": [failed.clone(), successful(now() - 86_400.0)] })).found());
        assert_eq!(level, Level::Warn);
        assert!(what.starts_with("latest backup attempt failed (2026-11-05 11:10 UTC): out of luck"), "{what}");
        assert!(!what.contains("a0secret"), "{what}");
        // A failure that a later success fixed, and one still running, are nothing.
        let found = with_backups(json!({ "items": [successful(now() - 1800.0), failed.clone()] })).found();
        assert!(found.is_empty(), "{found:?}");
        let running = json!({ "status": "RUNNING", "startTime": rfc3339(now() - 60.0) });
        let found = with_backups(json!({ "items": [running, successful(now() - 86_400.0)] })).found();
        assert!(found.is_empty(), "{found:?}");
        // Skipped windows are no attempt: what came before them counts.
        let skipped = json!({ "status": "SKIPPED", "startTime": rfc3339(now() - 60.0) });
        let found = with_backups(json!({ "items": [skipped, failed, successful(now() - 86_400.0)] })).found();
        assert_eq!(only(found).0, Level::Warn);
    }

    #[test]
    fn transaction_ids_warn_at_50_percent_and_are_bad_at_80() {
        assert!(Scene { wraparound: 0.4999, ..Scene::healthy() }.found().is_empty());
        assert_eq!(only(Scene { wraparound: 0.50, ..Scene::healthy() }.found()).0, Level::Warn);
        assert_eq!(only(Scene { wraparound: 0.7999, ..Scene::healthy() }.found()).0, Level::Warn);
        let (level, what) = only(Scene { wraparound: 0.80, ..Scene::healthy() }.found());
        assert_eq!((level, what.as_str()), (Level::Bad, "transaction ids are 80% of the way to wraparound"));
    }

    #[test]
    fn deadlocks_warn_and_name_the_databases() {
        let scene = Scene { deadlocks: vec![("shop", 1.0), ("orders", 2.0), ("quiet", 0.0)], ..Scene::healthy() };
        let (level, what) = only(scene.found());
        assert_eq!((level, what.as_str()), (Level::Warn, "3 deadlocks in the last 24 h: orders (2), shop (1)"));
        let scene = Scene { deadlocks: vec![("shop", 1.0)], ..Scene::healthy() };
        assert_eq!(only(scene.found()).1, "1 deadlock in the last 24 h: shop (1)");
    }

    #[test]
    fn an_instance_not_runnable_is_bad() {
        let mut scene = Scene::healthy();
        scene.instance["state"] = json!("MAINTENANCE");
        let (level, what) = only(scene.found());
        assert_eq!((level, what.as_str()), (Level::Bad, "state is MAINTENANCE, not RUNNABLE"));
    }

    #[test]
    fn a_stopped_instance_is_bad_and_expected_to_report_nothing() {
        let scene = Scene::healthy().settings(|s| s["activationPolicy"] = json!("NEVER"));
        let fake = scene.serve();
        let h = read_all(&client(&fake), None, now()).unwrap().remove(0);
        let found = h.findings();
        assert_eq!((found.len(), found[0].level), (1, Level::Bad), "{found:?}");
        assert!(found[0].what.starts_with("stopped"), "{found:?}");
        assert!(h.unread().is_empty(), "{:?}", h.unread());
        assert_eq!(fake.requests().len(), 1, "only the list: no metrics or backups to ask for");
    }

    #[test]
    fn a_replica_has_no_backups_to_miss() {
        let mut scene = Scene::healthy().settings(|s| s["backupConfiguration"]["enabled"] = json!(false));
        scene.instance["instanceType"] = json!("READ_REPLICA_INSTANCE");
        scene.instance["masterInstanceName"] = json!("main-db");
        let h = scene.read();
        assert!(h.findings().is_empty(), "{:?}", h.findings());
        assert!(h.notes()[0].contains("a replica keeps no backups"), "{:?}", h.notes());
        assert!(facts(&h.instance)[0].text.ends_with("replica of main-db"));
        assert!(!facts(&h.instance).iter().any(|f| f.key == "Backups"));
    }

    #[test]
    fn only_postgres_is_asked_for_what_only_postgres_reports() {
        let mut scene = Scene::healthy();
        scene.instance["databaseVersion"] = json!("MYSQL_8_0");
        let fake = scene.serve();
        let h = read_all(&client(&fake), None, now()).unwrap().remove(0);
        assert!(h.findings().is_empty() && h.unread().is_empty(), "{:?} {:?}", h.findings(), h.unread());
        assert!(h.notes()[0].contains("Postgres only"));
        assert!(!fake.requests().iter().any(|r| r.path.contains("postgresql")));
        assert_eq!(gauges(&h, 8).iter().map(|g| g.key).collect::<Vec<_>>(), ["CPU", "Memory", "Disk", "Last backup"]);
    }

    #[test]
    fn what_could_not_be_read_is_said_and_the_rest_still_judged() {
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no backups for you"}}"#;
        let fake = MockServer::start(vec![]);
        fake.route("/backupRuns", Reply::json(403, denied));
        Scene { cpu: 0.95, ..Scene::healthy() }.route_on(&fake);
        let h = read_all(&client(&fake), None, now()).unwrap().remove(0);
        assert_eq!(h.unread(), ["shared-db: backups: PERMISSION_DENIED: no backups for you"]);
        assert_eq!(h.findings()[0].what, "CPU averaged 95% over the last hour");

        let fake = MockServer::start(vec![]);
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no metrics"}}"#;
        fake.route("timeSeries", Reply::json(403, denied));
        Scene::healthy().route_on(&fake);
        let h = read_all(&client(&fake), None, now()).unwrap().remove(0);
        assert_eq!(h.unread(), ["shared-db: metrics: PERMISSION_DENIED: no metrics"]);
        assert!(h.findings().is_empty());
        assert_eq!(h.json()["connections"], Value::Null);
        assert_eq!(h.json()["lastSuccessfulBackup"], "2026-11-05T07:06:00Z");
        assert!(h.login_error().is_none());
        let login = Health { metrics: Some(Err(GcpError::LoginRequired { detail: "x".into() })), ..h };
        assert!(login.login_error().is_some());
    }

    #[test]
    fn several_instances_are_each_read_and_found() {
        let scene = Scene::healthy();
        let other = json!({ "name": "archive", "state": "FAILED", "databaseVersion": "POSTGRES_15",
                            "settings": { "activationPolicy": "ALWAYS", "backupConfiguration": { "enabled": false } } });
        let list = json!({ "items": [other, scene.instance] }).to_string();
        let fake = MockServer::start(vec![]);
        fake.route("/instances?", Reply::json(200, &list));
        scene.route_on(&fake);
        let all = read_all(&client(&fake), None, now()).unwrap();
        assert_eq!(all.iter().map(|h| h.instance.name.as_str()).collect::<Vec<_>>(), ["archive", "shared-db"]);
        let found = findings(&all);
        assert_eq!(found.len(), 2, "{found:?}");
        assert!(found.iter().all(|f| f.subject == "archive" && f.level == Level::Bad));
        assert_eq!(all[0].metrics().unwrap().cpu.len(), 288, "each asked by its own database_id");
        let asked = fake.requests();
        assert!(asked.iter().any(|r| r.path.contains("%3Aarchive%22")));
        assert!(asked.iter().any(|r| r.path.contains("%3Ashared-db%22")));
        let v = report("platform", "my-platform-123456", &all);
        assert_eq!(v["instances"].as_array().unwrap().len(), 2);
        assert_eq!(v["findings"][0]["subject"], "archive");
    }

    #[test]
    fn one_instance_by_name_is_read_with_its_own_call() {
        let fake = Scene::healthy().serve();
        let all = read_all(&client(&fake), Some("shared-db"), now()).unwrap();
        assert_eq!(all[0].instance.name, "shared-db");
        assert!(fake.requests().iter().any(|r| r.path.ends_with("/instances/shared-db")));
        assert!(!fake.requests().iter().any(|r| r.path.contains("/instances?maxResults")));
    }

    #[test]
    fn the_report_carries_the_numbers_and_what_it_found() {
        let scene = Scene { disk: 0.90, ..Scene::healthy() }.at(425.0);
        let h = scene.read();
        let v = report("platform", "my-platform-123456", std::slice::from_ref(&h));
        let i = &v["instances"][0];
        assert_eq!(i["maxConnections"], 500);
        assert_eq!(i["connections"]["now"], 425.0);
        assert_eq!(i["connections"]["percent"], 85.0);
        assert_eq!(i["connections"]["peakAt"], "2026-11-05T11:59:00Z");
        assert_eq!(i["diskPercent"], 90.0);
        assert_eq!(i["disk"]["sizeGb"], 100);
        assert_eq!(i["disk"]["autoResize"], true);
        assert_eq!(i["databases"][0]["name"], "orders");
        assert_eq!(i["databases"][0]["active"], 3.0);
        assert_eq!(i["databases"][0]["idle"], 50.0);
        assert_eq!(i["databases"][0]["idleInTransaction"], 1.0);
        assert_eq!(i["databases"][0]["peak24h"], 110.0);
        assert_eq!(i["maintenanceWindow"], "Mon 03:00 UTC");
        assert_eq!(v["findings"][0]["level"], "warn");
        assert_eq!(v["findings"].as_array().unwrap().len(), 2);
        assert_eq!(v["unread"], json!([]));
    }

    #[test]
    fn what_a_person_reads_of_an_instance() {
        let h = Scene { disk: 0.70, disk_bytes: vec![(0.0, 70.0 * GB)], ..Scene::healthy() }.read();
        let gauges = gauges(&h, 10);
        let of = |key: &str| gauges.iter().find(|g| g.key == key).unwrap();
        assert_eq!(of("Connections").text, "60 of 500 now (12%), peak 120 (24%) at 2026-11-05 11:00 UTC");
        assert_eq!(of("Connections").spark.as_ref().unwrap().chars().count(), 10);
        assert_eq!(of("CPU").text, "22% over the last hour");
        assert_eq!(of("Disk").text, "70% · 70.0 GB used");
        assert_eq!((of("Disk").tone, of("CPU").tone), (Tone::Plain, Tone::Plain));
        assert_eq!(of("Txn ids").text, "3.0% of the way to wraparound");
        assert_eq!(of("Last backup").text, "5 h ago · 2026-11-05 07:06 UTC");
        let facts = facts(&h.instance);
        let fact = |key: &str| facts.iter().find(|f| f.key == key).unwrap().text.as_str();
        assert_eq!(fact("Instance"), "POSTGRES_15 · db-custom-2-7680 · us-east1-b · zonal");
        assert_eq!(fact("Limit"), "max_connections 500");
        assert_eq!(fact("Disk"), "100 GB · auto-resize on");
        assert_eq!(fact("Backups"), "on · from 02:00 UTC · point-in-time recovery on");
        assert_eq!(fact("Network"), "public IP from 192.0.2.0/24");
        assert_eq!(fact("Maintenance"), "Mon 03:00 UTC");
        assert_eq!(fact("Protection"), "deletion protection on");

        // The rare: a limit in the thousands stays exact, maintenance on the calendar.
        let mut scene = Scene::healthy().at(1234.0);
        scene.instance["settings"]["databaseFlags"] = json!([{ "name": "max_connections", "value": "5000" }]);
        scene.instance["scheduledMaintenance"] = json!({ "startTime": "2026-11-08T03:00:00.000Z" });
        let h = scene.read();
        assert!(super::gauges(&h, 8)[0].text.starts_with("1234 of 5000 now (25%)"));
        let facts = super::facts(&h.instance);
        let maintenance = facts.iter().find(|f| f.key == "Maintenance").unwrap();
        assert_eq!(maintenance.text, "Mon 03:00 UTC · next scheduled 2026-11-08 03:00 UTC");
    }

    #[test]
    fn an_instance_open_to_the_world_stands_out_in_the_summary() {
        let scene = Scene::healthy().settings(|s| {
            s["ipConfiguration"]["authorizedNetworks"] = json!([{ "value": "0.0.0.0/0" }]);
            s["ipConfiguration"]["sslMode"] = json!("ALLOW_UNENCRYPTED_AND_ENCRYPTED");
        });
        let h = scene.read();
        let facts = facts(&h.instance);
        let network = facts.iter().find(|f| f.key == "Network").unwrap();
        assert_eq!(network.text, "public IP from 0.0.0.0/0 · SSL allow unencrypted and encrypted");
        assert_eq!(network.tone, Tone::Warn);
    }
}
