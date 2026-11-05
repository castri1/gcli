//! What an environment's project costs, from the detailed billing export in BigQuery (Google
//! Cloud has no Cost Explorer API): per UTC day, service, app (Cloud Run service) and SKU, the
//! billing account's months for budgets and credits, and the spikes among them.
//!
//! BigQuery bills the bytes each query scans, so a read is announced before it runs: its
//! queries are dry-run ([`prepare`], free) and priced, then run ([`run`]) and counted in the
//! [`ledger`]. What was read is kept per environment in `~/.gcli/costs/<env>/` ([`store`]) for
//! the Costs module, `gcli costs`, the MCP server (which never queries BigQuery) and the Apps
//! module (`apps.json`).
//!
//! The export is behind real time: most services by about 4 hours, Cloud Storage and Cloud
//! Scheduler by about 12. Today is always partial, and so is yesterday early in the UTC day:
//! comparisons and spikes stop at the last complete day ([`Snapshot::last_complete_day`]).

pub mod budget;
pub mod command;
pub mod day;
pub mod ledger;
pub mod mcp;
pub mod report;
pub mod spikes;
pub mod sql;

use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::{Billing, CostDefaults, Environment};
use crate::gcp::bigquery::{self, Param, Query};
use crate::gcp::budgets::{self, Budget};
use crate::gcp::{Gcp, GcpError};
use day::{Day, Range};

/// How far behind real time the slowest services reach the export: a day is complete once
/// this long has passed since it ended (and the export has rows past its end).
pub const SLOW_EXPORT_HOURS: f64 = 12.0;
/// Days of daily costs a refresh reads at least: a month of sparkline, and the 14 days before
/// each of its spikes.
const DAILY_DAYS: i64 = 30;

/// The project's cost on a UTC day, for a service, or one of its Cloud Run services.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct DayCost {
    pub day: Day,
    pub service_id: String,
    pub service: String,
    /// The Cloud Run service (the app); empty for other services, which no app can be told from.
    pub app: String,
    /// Gross.
    pub cost: f64,
    /// Negative: net is `cost + credits`.
    pub credits: f64,
}

/// A SKU of the project over a month: what it cost and how much of it was used.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SkuCost {
    /// `2026-11`
    pub month: String,
    pub service: String,
    pub sku: String,
    /// The export's usage unit (`seconds`, `byte-seconds`, `requests`…).
    pub unit: String,
    pub cost: f64,
    pub amount: f64,
}

impl SkuCost {
    /// What one unit cost, on average.
    pub fn rate(&self) -> Option<f64> {
        (self.amount > 0.0 && self.cost.is_finite()).then(|| self.cost / self.amount)
    }
}

/// A month of the billing account for a project and a service: gross (`credit_type` empty) or
/// one credit.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct MonthCost {
    pub month: String,
    pub project_number: String,
    pub service_id: String,
    /// Empty for the gross row; else the credit's type (`PROMOTION`, `DISCOUNT`…).
    pub credit_type: String,
    pub credit_name: String,
    pub amount: f64,
}

/// What a read covers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Kind {
    /// This month and the last, to today: what the module and `gcli costs` show.
    #[default]
    Current,
    /// A past month (its first day), and the one before it to compare (`--month`).
    Month(Day),
    /// The project's days only (`--check --date`, `--days`), when the current read lacks them.
    Days(Range),
}

/// Everything read for an environment at once.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Snapshot {
    /// The gcli environment: a cache file only serves that one.
    pub env: String,
    pub project: String,
    /// From the export, for budgets (which name projects by number).
    pub project_number: String,
    pub account: String,
    pub table: String,
    /// Unix seconds.
    pub read_at: f64,
    pub currency: String,
    pub kind: Kind,
    /// The project's days read (`daily`).
    pub days: Option<Range>,
    /// The months read (`skus`, `account_months`), from the first day of the first one.
    pub months: Option<Range>,
    /// The end of the newest usage hour in the project's rows: how far the export has come.
    pub data_through: f64,
    pub daily: Vec<DayCost>,
    pub skus: Vec<SkuCost>,
    /// The billing account's months, by project and service.
    pub account_months: Vec<MonthCost>,
    pub budgets: Vec<Budget>,
    /// What could not be read, and why (budgets without permission, say).
    pub notes: Vec<String>,
    pub queries: u64,
    pub bytes_billed: u64,
}

impl Snapshot {
    /// The last day the export has complete: past the slowest services' lag, with rows past
    /// its end, and within what was read.
    pub fn last_complete_day(&self) -> Option<Day> {
        let days = self.days?;
        let by_clock = Day::of(self.read_at - SLOW_EXPORT_HOURS * 3600.0).plus(-1);
        let by_data = if self.data_through > 0.0 { Day::of(self.data_through).plus(-1) } else { by_clock };
        let last = by_clock.min(by_data).min(days.to);
        (last >= days.from).then_some(last)
    }

    /// Whether the days it covers had all reached the export when it was read: a past month
    /// read after its late rows came in never changes.
    pub fn settled(&self) -> bool {
        let range = match self.kind {
            Kind::Current => return false,
            Kind::Month(m) => Range::new(m.previous_month(), m.month_end()),
            Kind::Days(r) => r,
        };
        self.read_at >= sql::partitions(range).1.start()
    }

    /// Read more than `hours` ago, and not settled.
    pub fn stale(&self, now: f64, hours: f64) -> bool {
        !self.settled() && now - self.read_at > hours * 3600.0
    }
}

// ---- errors ------------------------------------------------------------------------------------

/// A read that failed, said with what to change.
#[derive(Debug, Clone)]
pub struct CostError {
    pub message: String,
    /// The credentials expired: a sign-in fixes it.
    pub login: bool,
}

impl fmt::Display for CostError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CostError {}

fn failure(message: impl Into<String>) -> CostError {
    CostError { message: message.into(), login: false }
}

/// The environment's billing export settings, or what to set.
pub fn billing_of<'a>(env: &str, config: &'a Environment) -> Result<&'a Billing, CostError> {
    let setting = |key: &str| format!("environments.{env}.billing.{key}");
    let billing = config.billing.as_ref().filter(|b| !b.export_table.is_empty()).ok_or_else(|| {
        failure(format!(
            "'{env}' has no billing export configured: set {} (project.dataset.table of the detailed usage cost export) \
             and {} (the project that runs, and pays for, the BigQuery jobs) in ~/.gcli/config.yaml",
            setting("exportTable"),
            setting("queryProject")
        ))
    })?;
    if billing.query_project.is_empty() {
        return Err(failure(format!(
            "set {} in ~/.gcli/config.yaml: the project that runs, and pays for, the BigQuery jobs (one where you have roles/bigquery.jobUser)",
            setting("queryProject")
        )));
    }
    sql::table(&billing.export_table).map_err(|e| failure(format!("{}: {e}", setting("exportTable"))))?;
    Ok(billing)
}

/// A BigQuery error said with the setting to change.
pub fn explain(env: &str, billing: &Billing, e: &GcpError) -> CostError {
    let GcpError::Api { status, code, message } = e else {
        return CostError { message: e.short(), login: e.is_login_required() };
    };
    let (table, qp) = (&billing.export_table, &billing.query_project);
    let lower = message.to_lowercase();
    let text = if *status == 404 || code == "NOT_FOUND" {
        if lower.contains("project") && !lower.contains("table") && !lower.contains("dataset") {
            format!(
                "BigQuery: {message}. Check environments.{env}.billing.queryProject ('{qp}') in ~/.gcli/config.yaml"
            )
        } else {
            format!(
                "BigQuery: {message}. Check environments.{env}.billing.exportTable ('{table}') in ~/.gcli/config.yaml: \
                 project.dataset.table of the detailed usage cost export (gcp_billing_export_resource_v1_…)"
            )
        }
    } else if lower.contains("has not been used") || lower.contains("is disabled") {
        format!(
            "BigQuery: {message}. Enable the BigQuery API in '{qp}', or set environments.{env}.billing.queryProject to a project where it is enabled"
        )
    } else if *status == 403 || code == "PERMISSION_DENIED" {
        if lower.contains("jobs.create") {
            format!(
                "BigQuery: {message}. You cannot run queries in '{qp}': ask for roles/bigquery.jobUser there, or set \
                 environments.{env}.billing.queryProject to a project where you have it"
            )
        } else {
            format!(
                "BigQuery: {message}. Reading '{table}' needs roles/bigquery.dataViewer on its dataset (and roles/bigquery.jobUser \
                 on '{qp}'); check environments.{env}.billing.exportTable and billing.queryProject"
            )
        }
    } else if *status == 400 && lower.contains("unrecognized name") {
        format!(
            "BigQuery: {message}. environments.{env}.billing.exportTable must be the detailed usage cost export \
             (gcp_billing_export_resource_v1_…), which has resource names; the standard one does not"
        )
    } else {
        format!("BigQuery: {}", e.short())
    };
    failure(text)
}

// ---- reading -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Daily,
    Skus,
    Account,
}

/// A read whose queries were dry-run: what it will scan, before it runs.
#[derive(Debug, Clone)]
pub struct Prepared {
    env: String,
    project: String,
    account: String,
    table: String,
    query_project: String,
    pub kind: Kind,
    days: Option<Range>,
    months: Option<Range>,
    budgets: Vec<Budget>,
    notes: Vec<String>,
    queries: Vec<(Part, String, Vec<Param>)>,
    /// What BigQuery will bill for the bytes the dry runs say the queries scan.
    pub billed: u64,
}

impl Prepared {
    /// `≈ 1.2 GB, US$ 0.0075 (3 queries)`
    pub fn describe(&self, usd_per_tib: f64) -> String {
        format!(
            "≈ {}, {} ({} {})",
            report::bytes(self.billed),
            report::usd_small(usd(self.billed, usd_per_tib)),
            self.queries.len(),
            if self.queries.len() == 1 { "query" } else { "queries" }
        )
    }
}

/// What `bytes` cost at `usd_per_tib`.
pub fn usd(bytes: u64, usd_per_tib: f64) -> f64 {
    bytes as f64 / (1u64 << 40) as f64 * usd_per_tib
}

/// Plans a read of `env` (`kind`) on `now` and dry-runs its queries. Budgets are listed first
/// (free): how far back the account query goes depends on their periods.
pub fn prepare(gcp: &Gcp, env: &str, config: &Environment, kind: Kind, now: f64) -> Result<Prepared, CostError> {
    let billing = billing_of(env, config)?;
    let table = sql::table(&billing.export_table).map_err(failure)?;
    let today = Day::of(now);
    let month = today.month_start();
    let mut notes = Vec::new();
    let mut budgets = Vec::new();
    let (days, skus, account) = match kind {
        Kind::Current => {
            if billing.account.is_empty() {
                notes.push(format!("budgets: set environments.{env}.billing.account to see them"));
            } else {
                match list_budgets(gcp, &billing.account, &config.project, &billing.query_project) {
                    Ok(list) => budgets = list,
                    Err(e) => notes.push(format!("budgets not read: {e}")),
                }
            }
            let last_month = month.previous_month();
            let daily = Range::new(last_month.min(today.plus(-DAILY_DAYS)), today);
            let from = budget::earliest(&budgets, today).map_or(last_month, |d| d.min(last_month)).month_start();
            (daily, Some(Range::new(last_month, today)), Some(Range::new(from, today)))
        }
        Kind::Month(m) => {
            if m > month {
                return Err(failure(format!("{} has not started yet", m.month_name())));
            }
            let range = Range::new(m.previous_month(), m.month_end().min(today));
            (range, Some(range), Some(range))
        }
        Kind::Days(r) => (Range::new(r.from, r.to.min(today)), None, None),
    };
    let mut queries = vec![{
        let (q, p) = sql::daily(&table, &config.project, days);
        (Part::Daily, q, p)
    }];
    if let Some(r) = skus {
        let (q, p) = sql::skus(&table, &config.project, r);
        queries.push((Part::Skus, q, p));
    }
    if let Some(r) = account {
        let (q, p) = sql::account(&table, r);
        queries.push((Part::Account, q, p));
    }
    let mut billed = 0;
    for (_, q, params) in &queries {
        let scanned = bigquery::dry_run(gcp, &Query { project: &billing.query_project, sql: q, params })
            .map_err(|e| explain(env, billing, &e))?;
        billed += bigquery::billed(scanned);
    }
    Ok(Prepared {
        env: env.to_string(),
        project: config.project.clone(),
        account: billing.account.clone(),
        table: billing.export_table.clone(),
        query_project: billing.query_project.clone(),
        kind,
        days: Some(days),
        months: account.or(skus),
        budgets,
        notes,
        queries,
        billed,
    })
}

/// The account's budgets: the API needs a quota project with it enabled, which is more often
/// the environment's project than the one running BigQuery jobs. Both are tried.
fn list_budgets(gcp: &Gcp, account: &str, project: &str, query_project: &str) -> Result<Vec<Budget>, String> {
    match budgets::list(gcp, account, project) {
        Ok(list) => Ok(list),
        Err(first) if first.is_login_required() => Err(first.short()),
        Err(first) => budgets::list(gcp, account, query_project).map_err(|_| {
            format!(
                "{} (the Cloud Billing Budget API must be enabled in '{project}', and you need roles/billing.viewer on the billing account)",
                first.short()
            )
        }),
    }
}

#[derive(Deserialize)]
struct DailyRow {
    day: Day,
    project_number: Option<String>,
    service_id: Option<String>,
    service: Option<String>,
    app: Option<String>,
    cost: Option<f64>,
    credits: Option<f64>,
    latest: Option<f64>,
    currency: Option<String>,
}

#[derive(Deserialize)]
struct SkuRow {
    month: String,
    service: Option<String>,
    sku: Option<String>,
    unit: Option<String>,
    cost: Option<f64>,
    amount: Option<f64>,
}

#[derive(Deserialize)]
struct AccountRow {
    month: String,
    project_number: Option<String>,
    service_id: Option<String>,
    credit_type: Option<String>,
    credit_name: Option<String>,
    amount: Option<f64>,
}

/// Runs the queries of `p` and puts their rows together; counts what they were billed.
pub fn run(gcp: &Gcp, p: Prepared, now: f64) -> Result<Snapshot, CostError> {
    let billing =
        Billing { account: p.account.clone(), export_table: p.table.clone(), query_project: p.query_project.clone() };
    let mut s = Snapshot {
        env: p.env.clone(),
        project: p.project.clone(),
        account: p.account.clone(),
        table: p.table.clone(),
        read_at: now,
        kind: p.kind,
        days: p.days,
        months: p.months,
        budgets: p.budgets.clone(),
        notes: p.notes.clone(),
        ..Default::default()
    };
    let mut latest = 0.0f64;
    let result = (|| -> Result<(), GcpError> {
        for (part, q, params) in &p.queries {
            let query = Query { project: &p.query_project, sql: q, params };
            match part {
                Part::Daily => {
                    let a = bigquery::query::<DailyRow>(gcp, &query)?;
                    s.bytes_billed += a.bytes_billed;
                    for r in a.rows {
                        latest = latest.max(r.latest.unwrap_or(0.0));
                        if s.project_number.is_empty() {
                            s.project_number = r.project_number.unwrap_or_default();
                        }
                        if s.currency.is_empty() {
                            s.currency = r.currency.unwrap_or_default();
                        }
                        s.daily.push(DayCost {
                            day: r.day,
                            service_id: r.service_id.unwrap_or_default(),
                            service: r.service.unwrap_or_default(),
                            app: r.app.unwrap_or_default(),
                            cost: r.cost.unwrap_or(0.0),
                            credits: r.credits.unwrap_or(0.0),
                        });
                    }
                }
                Part::Skus => {
                    let a = bigquery::query::<SkuRow>(gcp, &query)?;
                    s.bytes_billed += a.bytes_billed;
                    s.skus = a
                        .rows
                        .into_iter()
                        .map(|r| SkuCost {
                            month: r.month,
                            service: r.service.unwrap_or_default(),
                            sku: r.sku.unwrap_or_default(),
                            unit: r.unit.unwrap_or_default(),
                            cost: r.cost.unwrap_or(0.0),
                            amount: r.amount.unwrap_or(0.0),
                        })
                        .collect();
                }
                Part::Account => {
                    let a = bigquery::query::<AccountRow>(gcp, &query)?;
                    s.bytes_billed += a.bytes_billed;
                    s.account_months = a
                        .rows
                        .into_iter()
                        .map(|r| MonthCost {
                            month: r.month,
                            project_number: r.project_number.unwrap_or_default(),
                            service_id: r.service_id.unwrap_or_default(),
                            credit_type: r.credit_type.unwrap_or_default(),
                            credit_name: r.credit_name.unwrap_or_default(),
                            amount: r.amount.unwrap_or(0.0),
                        })
                        .collect();
                }
            }
            s.queries += 1;
        }
        Ok(())
    })();
    // What ran is billed even when a later query failed.
    ledger::count(&Day::of(now).month_key(), s.queries, s.bytes_billed);
    result.map_err(|e| explain(&p.env, &billing, &e))?;
    // `latest` is the start of the newest hour.
    s.data_through = if latest > 0.0 { latest + 3600.0 } else { 0.0 };
    s.daily.sort_by(|a, b| (a.day, &a.service, &a.app).cmp(&(b.day, &b.service, &b.app)));
    Ok(s)
}

// ---- the cache ---------------------------------------------------------------------------------

/// `~/.gcli/costs` (a folder of their own for the tests, which never touch the user's).
pub fn costs_dir() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!("gcli-test-costs-{}", std::process::id())));
    }
    Some(crate::util::home_dir()?.join(".gcli").join("costs"))
}

/// `env` with every byte but ASCII letters, digits, `-` and `_` written `%XX`: reversible, so
/// two environments never share a folder (`prod.eu` → `prod%2Eeu`).
fn escape(env: &str) -> String {
    let mut out = String::new();
    for b in env.bytes() {
        if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `~/.gcli/costs/<env>/`
pub fn env_dir(env: &str) -> Option<PathBuf> {
    Some(costs_dir()?.join(escape(env)))
}

fn file_name(kind: Kind) -> String {
    match kind {
        Kind::Current => "current.json".into(),
        Kind::Month(m) => format!("month-{}.json", m.month_key()),
        Kind::Days(r) => format!("days-{}-{}.json", r.from, r.to),
    }
}

/// What was read for `env` (`kind`), from `dir` (its folder).
pub fn load_in(dir: &Path, env: &str, kind: Kind) -> Option<Snapshot> {
    let text = std::fs::read_to_string(dir.join(file_name(kind))).ok()?;
    let s: Snapshot = serde_json::from_str(&text).ok()?;
    // A damaged or edited file cannot claim another environment, or a reading from nowhere.
    (s.env == env && s.kind == kind && s.read_at.is_finite() && s.read_at > 0.0).then_some(s)
}

pub fn load(env: &str, kind: Kind) -> Option<Snapshot> {
    load_in(&env_dir(env)?, env, kind)
}

/// Keeps `s` in `dir`; after a current read, also `apps.json` for the Apps module.
pub fn store_in(dir: &Path, s: &Snapshot, rule: &spikes::Rule) -> std::io::Result<()> {
    write_private(&dir.join(file_name(s.kind)), serde_json::to_string(s).unwrap_or_default().as_bytes())?;
    if s.kind == Kind::Current {
        let apps = report::apps_file(s, rule);
        write_private(
            &dir.join("apps.json"),
            (serde_json::to_string_pretty(&apps).unwrap_or_default() + "\n").as_bytes(),
        )?;
    }
    Ok(())
}

pub fn store(s: &Snapshot, costs: &CostDefaults) -> std::io::Result<()> {
    let dir = env_dir(&s.env).ok_or_else(|| std::io::Error::other("cannot find the home directory"))?;
    store_in(&dir, s, &spikes::Rule::of(costs))
}

/// Writes a file only its owner can read (costs are not for every local user), atomically: a
/// reader never sees half of it.
pub fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = path.parent().ok_or_else(|| std::io::Error::other(format!("no folder for {}", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.{}.{n}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let written = options.open(&tmp).and_then(|mut f| f.write_all(content).and_then(|()| f.sync_all()));
    written.and_then(|()| std::fs::rename(&tmp, path)).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::auth;
    use crate::gcp::mock::{MockServer, Reply};

    pub(crate) fn day(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    pub(crate) fn env() -> Environment {
        Environment {
            project: "my-platform-123456".into(),
            region: "us-east1".into(),
            billing: Some(Billing {
                account: "012345-6789AB-CDEF01".into(),
                export_table: "my-billing-project.billing_export.gcp_billing_export_resource_v1_012345_6789AB_CDEF01"
                    .into(),
                query_project: "my-billing-project".into(),
            }),
            ..Default::default()
        }
    }

    /// 2026-11-05 20:55 UTC
    pub(crate) const NOW: f64 = 1_793_912_100.0;

    #[test]
    fn complete_days_follow_the_clock_and_the_data() {
        let mut s = Snapshot {
            read_at: NOW,
            days: Some(Range::new(day("2026-10-01"), day("2026-11-05"))),
            data_through: day("2026-11-05").start() + 16.0 * 3600.0,
            ..Default::default()
        };
        assert_eq!(s.last_complete_day(), Some(day("2026-11-04")));
        // Early in the UTC day, the slow services still miss yesterday.
        s.read_at = day("2026-11-05").start() + 2.0 * 3600.0;
        assert_eq!(s.last_complete_day(), Some(day("2026-11-03")));
        // An export that stopped: its last full day.
        s.read_at = NOW;
        s.data_through = day("2026-11-03").start() + 18.0 * 3600.0;
        assert_eq!(s.last_complete_day(), Some(day("2026-11-02")));
        // A window of the past ends at its own last day.
        s.days = Some(Range::new(day("2026-10-20"), day("2026-11-03")));
        s.data_through = day("2026-11-04").start();
        assert_eq!(s.last_complete_day(), Some(day("2026-11-03")));
    }

    #[test]
    fn settled_reads_never_go_stale() {
        let mut s = Snapshot { kind: Kind::Month(day("2026-10-01")), read_at: NOW, ..Default::default() };
        assert!(s.settled(), "October's late rows were in by November 3");
        assert!(!s.stale(NOW + 400.0 * 86_400.0, 6.0));
        s.kind = Kind::Current;
        assert!(!s.stale(NOW + 3600.0, 6.0) && s.stale(NOW + 7.0 * 3600.0, 6.0));
        s.kind = Kind::Days(Range::new(day("2026-10-25"), day("2026-11-04")));
        assert!(!s.settled());
    }

    #[test]
    fn missing_settings_say_what_to_set() {
        let mut e = env();
        e.billing = None;
        let msg = billing_of("platform", &e).unwrap_err().message;
        assert!(
            msg.contains("environments.platform.billing.exportTable") && msg.contains("billing.queryProject"),
            "{msg}"
        );
        let mut e = env();
        e.billing.as_mut().unwrap().query_project.clear();
        assert!(billing_of("platform", &e).unwrap_err().message.contains("environments.platform.billing.queryProject"));
        let mut e = env();
        e.billing.as_mut().unwrap().export_table = "x.y".into();
        assert!(billing_of("platform", &e).unwrap_err().message.contains("exportTable"));
        assert!(billing_of("platform", &env()).is_ok());
    }

    #[test]
    fn bigquery_errors_say_which_setting() {
        let b = env().billing.unwrap();
        let api =
            |status, code: &str, message: &str| GcpError::Api { status, code: code.into(), message: message.into() };
        let m = explain("platform", &b, &api(404, "NOT_FOUND", "Not found: Table my-billing-project:billing_export.t"))
            .message;
        assert!(m.contains("environments.platform.billing.exportTable"), "{m}");
        let m = explain(
            "platform",
            &b,
            &api(
                403,
                "PERMISSION_DENIED",
                "Access Denied: Project my-billing-project: User does not have bigquery.jobs.create permission",
            ),
        )
        .message;
        assert!(m.contains("billing.queryProject") && m.contains("roles/bigquery.jobUser"), "{m}");
        let m = explain(
            "platform",
            &b,
            &api(403, "PERMISSION_DENIED", "Access Denied: Table my-billing-project:billing_export.t"),
        )
        .message;
        assert!(m.contains("roles/bigquery.dataViewer"), "{m}");
        let m = explain("platform", &b, &api(400, "INVALID_ARGUMENT", "Unrecognized name: resource at [6:3]")).message;
        assert!(m.contains("detailed usage cost export"), "{m}");
        let e = explain("platform", &b, &GcpError::LoginRequired { detail: "x".into() });
        assert!(e.login);
    }

    const DAILY_SCHEMA: &str = r#"{"fields":[{"name":"day","type":"STRING"},{"name":"project_number","type":"STRING"},
        {"name":"service_id","type":"STRING"},{"name":"service","type":"STRING"},{"name":"app","type":"STRING"},
        {"name":"cost","type":"FLOAT"},{"name":"credits","type":"FLOAT"},{"name":"latest","type":"INTEGER"},{"name":"currency","type":"STRING"}]}"#;

    #[test]
    fn a_read_is_dry_run_then_run_and_counted() {
        let dry = |bytes: u64| Reply::json(200, &format!(r#"{{"totalBytesProcessed":"{bytes}","jobComplete":true}}"#));
        let row = |cells: &[&str]| {
            let f: Vec<String> = cells
                .iter()
                .map(|c| if *c == "null" { "{\"v\":null}".into() } else { format!("{{\"v\":\"{c}\"}}") })
                .collect();
            format!("{{\"f\":[{}]}}", f.join(","))
        };
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"budgets":[{"displayName":"gross","budgetFilter":{"projects":["projects/123456789012"]},"amount":{"specifiedAmount":{"units":"550"}}}]}"#,
            ),
            dry(100 << 20),
            dry(200 << 20),
            dry(300 << 20),
            Reply::json(
                200,
                &format!(
                    r#"{{"schema":{DAILY_SCHEMA},"jobComplete":true,"totalBytesBilled":"104857600","rows":[{},{}]}}"#,
                    row(&[
                        "2026-11-04",
                        "123456789012",
                        sql::CLOUD_RUN,
                        "Cloud Run",
                        "orders",
                        "1.5",
                        "-0.1",
                        "1793793600",
                        "USD"
                    ]),
                    row(&[
                        "2026-11-03",
                        "123456789012",
                        "9662-B51E-5089",
                        "Cloud SQL",
                        "null",
                        "2.25",
                        "0",
                        "1793707200",
                        "USD"
                    ])
                ),
            ),
            Reply::json(
                200,
                r#"{"schema":{"fields":[{"name":"month","type":"STRING"},{"name":"cost","type":"FLOAT"}]},"jobComplete":true,"totalBytesBilled":"209715200","rows":[{"f":[{"v":"2026-11"},{"v":"3.75"}]}]}"#,
            ),
            Reply::json(
                200,
                r#"{"schema":{"fields":[{"name":"month","type":"STRING"},{"name":"amount","type":"FLOAT"}]},"jobComplete":true,"totalBytesBilled":"314572800","rows":[]}"#,
            ),
        ]);
        let gcp = Gcp::with("my-platform-123456", "us-east1", auth::tests::fixed("t"), Some(server.url()));
        let p = prepare(&gcp, "platform", &env(), Kind::Current, NOW).unwrap();
        assert_eq!((p.queries.len(), p.billed), (3, 600 << 20));
        assert_eq!(p.describe(6.25), "≈ 629 MB, US$ 0.0036 (3 queries)");
        let s = run(&gcp, p, NOW).unwrap();
        assert_eq!((s.queries, s.bytes_billed), (3, 600 << 20));
        assert_eq!((s.project_number.as_str(), s.currency.as_str()), ("123456789012", "USD"));
        assert_eq!(s.daily.len(), 2);
        assert_eq!((s.daily[0].service.as_str(), s.daily[0].app.as_str()), ("Cloud SQL", ""));
        assert_eq!((s.daily[1].app.as_str(), s.daily[1].credits), ("orders", -0.1));
        // The newest hour started 2026-11-04 12:00 UTC.
        assert_eq!(s.data_through, 1_793_793_600.0 + 3600.0);
        assert_eq!(s.days, Some(Range::new(day("2026-10-01"), day("2026-11-05"))));
        assert_eq!(s.months, Some(Range::new(day("2026-10-01"), day("2026-11-05"))));
        assert_eq!(s.skus[0].cost, 3.75);
        assert_eq!(s.budgets[0].display_name, "gross");
        let reqs = server.requests();
        assert_eq!(reqs[0].path, "/billingbudgets/v1/billingAccounts/012345-6789AB-CDEF01/budgets?pageSize=100");
        assert!(reqs[1..4].iter().all(|r| r.body.contains("\"dryRun\":true")));
        assert!(reqs[4..].iter().all(|r| r.body.contains("\"dryRun\":false")));
    }

    #[test]
    fn cache_round_trip_and_apps_file() {
        let dir = std::env::temp_dir().join(format!("gcli-costs-cache-{}", std::process::id()));
        let s = report::tests::snapshot();
        let rule = spikes::Rule { ratio: 2.0, min_usd: 1.0, new_min_usd: 2.0 };
        store_in(&dir, &s, &rule).unwrap();
        assert_eq!(load_in(&dir, &s.env, Kind::Current), Some(s.clone()));
        assert_eq!(load_in(&dir, "other", Kind::Current), None, "another environment's file is not served");
        assert_eq!(load_in(&dir, &s.env, Kind::Month(day("2026-10-01"))), None);
        let apps: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("apps.json")).unwrap()).unwrap();
        // The schema the Apps module reads.
        assert_eq!(apps["readAt"], "2026-11-05T20:55:00Z");
        assert_eq!(apps["lastCompleteDay"], "2026-11-04");
        let orders = &apps["apps"]["orders"];
        assert_eq!(
            orders.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["monthToDate", "yesterday", "median14", "spike"]
        );
        assert!(orders["monthToDate"].is_f64() && orders["median14"].is_f64() && orders["spike"].is_boolean());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("current.json")).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // Past months and windows go in their own files, and no apps.json.
        std::fs::remove_file(dir.join("apps.json")).unwrap();
        let month = Snapshot { kind: Kind::Month(day("2026-10-01")), ..s.clone() };
        store_in(&dir, &month, &rule).unwrap();
        assert!(dir.join("month-2026-10.json").exists() && !dir.join("apps.json").exists());
        assert_eq!(escape("prod.eu/1"), "prod%2Eeu%2F1");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
