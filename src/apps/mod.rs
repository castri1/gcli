//! Apps: what gcli calls an app and what it knows about one. An app is a Cloud Run service,
//! plus what the environment's convention attaches to it (whiteghost's `<slug>-prod-*`
//! secrets); for each, its health over the last day from Cloud Monitoring, whether it holds
//! requests at its timeout, its revisions and secrets, and its cost when the Costs module has
//! read the bill. The terminal UI, the commands and the MCP server all read apps through here.

pub mod cmd;
pub mod costs;
pub mod health;
pub mod report;
pub mod secrets;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::config::AppsConvention;
use crate::gcp::run::{self, Revision, Service};
use crate::gcp::{Gcp, GcpError, Result};
use crate::tui::chart::{fmt_count, fmt_ms};
use crate::util::fmt_ago;
use costs::{AppCost, CostFile};
use health::{Charts, Health, Range};

impl AppsConvention {
    /// The prefix of an app's own secrets in Secret Manager. `None`: an app's secrets are only
    /// those its service reads. (Conventions name more, such as whiteghost's load balancer
    /// pieces `<slug>-neg` and `<slug>-backend`; they join here when a module reads them.)
    pub fn secret_prefix(self, app: &str) -> Option<String> {
        match self {
            AppsConvention::Run => None,
            AppsConvention::Whiteghost => Some(format!("{app}-prod-")),
        }
    }
}

/// Revisions an app's detail lists.
pub const REVISIONS: usize = 10;

/// How the list is sorted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Sort {
    #[default]
    Name,
    /// This month's cost, highest first (needs the Costs file).
    Cost,
    /// Requests over the last day, most first.
    Requests,
    /// 5xx responses, then 4xx, most first.
    Errors,
}

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::Name => "name",
            Sort::Cost => "cost",
            Sort::Requests => "requests",
            Sort::Errors => "errors",
        }
    }

    pub fn next(self) -> Sort {
        match self {
            Sort::Name => Sort::Cost,
            Sort::Cost => Sort::Requests,
            Sort::Requests => Sort::Errors,
            Sort::Errors => Sort::Name,
        }
    }
}

/// The health of an app without a request in the window (absent from Monitoring's answer).
pub static IDLE: Health =
    Health { requests: 0.0, classes: std::collections::BTreeMap::new(), billed_secs: 0.0, latency: None };

/// One app as the list shows it.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    pub service: &'a Service,
    /// `None` while the metrics are not read (or could not be).
    pub health: Option<&'a Health>,
    pub cost: Option<&'a AppCost>,
}

impl<'a> Row<'a> {
    pub fn new(
        service: &'a Service,
        health: Option<&'a HashMap<String, Health>>,
        costs: Option<&'a CostFile>,
    ) -> Row<'a> {
        Row {
            service,
            health: health.map(|h| h.get(service.id()).unwrap_or(&IDLE)),
            cost: costs.and_then(|c| c.app(service.id())),
        }
    }

    pub fn held(&self) -> bool {
        self.health.is_some_and(|h| h.held(self.service.timeout_secs()))
    }

    /// The row as `--json` and the MCP server give it.
    pub fn json(&self) -> Value {
        let s = self.service;
        let mut v = json!({
            "name": s.id(),
            "region": s.region(),
            "url": Some(&s.uri).filter(|u| !u.is_empty()),
            "latestReadyRevision": Some(s.revision()).filter(|r| !r.is_empty()),
            "revisionReadyAt": s.revision_ready_at().map(crate::gcp::monitoring::rfc3339),
            "latestRevisionFailed": s.latest_not_ready(),
            "traffic": s.traffic_split().unwrap_or_else(|| "100% latest".into()),
            "minInstances": s.min_instances(),
            "maxInstances": s.max_instances(),
            "cpu": if s.cpu_always_on() { "always-on" } else { "request" },
            "timeoutSeconds": s.timeout_secs(),
            "lastModifier": Some(&s.last_modifier).filter(|m| !m.is_empty()),
            "held": self.held(),
        });
        if let Some(h) = self.health {
            v["last24h"] = health_json(h, s.timeout_secs());
        }
        if let Some(c) = self.cost {
            v["cost"] = json!({
                "monthToDateUsd": round2(c.month_to_date),
                "yesterdayUsd": round2(c.yesterday),
                "median14Usd": round2(c.median14),
                "spike": c.spike,
            });
        }
        v
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// A stretch of health as JSON.
pub fn health_json(h: &Health, timeout_secs: f64) -> Value {
    let ms = |q: f64| h.percentile(q).map(|v| v.round());
    json!({
        "requests": h.requests,
        "byClass": h.classes,
        "percent4xx": h.percent("4xx").map(round2),
        "percent5xx": h.percent("5xx").map(round2),
        "p50Ms": ms(0.5),
        "p95Ms": ms(0.95),
        "p99Ms": ms(0.99),
        "billedInstanceHours": round2(h.billed_secs / 3600.0),
        "held": h.held(timeout_secs),
        "requestsNearTimeout": h.near_timeout(timeout_secs).round(),
    })
}

/// Sorts `items` by `by`, each seen as the [`Row`] `row` makes of it (ties, and rows without
/// the data, by name).
pub fn sort<'a, T>(items: &mut [T], by: Sort, row: impl Fn(&T) -> Row<'a>) {
    let cost = |r: &Row| r.cost.map_or(-1.0, |c| c.month_to_date);
    let requests = |r: &Row| r.health.map_or(-1.0, |h| h.requests);
    let errors = |r: &Row| r.health.map_or((-1.0, -1.0), |h| (h.count("5xx"), h.count("4xx")));
    items.sort_by(|a, b| {
        let (a, b) = (&row(a), &row(b));
        let order = match by {
            Sort::Name => std::cmp::Ordering::Equal,
            Sort::Cost => cost(b).total_cmp(&cost(a)),
            Sort::Requests => requests(b).total_cmp(&requests(a)),
            Sort::Errors => {
                let (ea, eb) = (errors(a), errors(b));
                eb.0.total_cmp(&ea.0).then(eb.1.total_cmp(&ea.1))
            }
        };
        order.then_with(|| a.service.id().cmp(b.service.id()))
    });
}

/// How a cell should look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Warn,
    Bad,
}

/// A column of the list. Narrow terminals drop the columns with the highest `drop` first.
pub struct Column {
    pub head: &'static str,
    pub right: bool,
    pub drop: u8,
}

/// The list's columns; the cost ones only with a Costs file.
pub fn columns(with_costs: bool) -> Vec<Column> {
    let col = |head, right, drop| Column { head, right, drop };
    let mut cols = vec![
        col("APP", false, 0),
        col("REV", false, 6),
        col("AGE", true, 4),
        col("MIN/MAX", true, 5),
        col("CPU", false, 7),
        col("REQ 24H", true, 0),
        col("4XX%", true, 8),
        col("5XX%", true, 1),
        col("BILLED H", true, 2),
        col("P99", true, 1),
    ];
    if with_costs {
        cols.push(col("$ MONTH", true, 2));
        cols.push(col("$ YDAY", true, 3));
    }
    cols.push(col("NOTES", false, 0));
    cols
}

/// A row's cells, in [`columns`]' order.
pub fn cells(row: &Row, with_costs: bool) -> Vec<(String, Tone)> {
    let s = row.service;
    let dash = || ("—".to_string(), Tone::Muted);
    let age = s.revision_ready_at().map(|t| fmt_ago(t).trim_end_matches(" ago").to_string()).unwrap_or_default();
    let scaling = match s.manual_instances() {
        Some(n) => (format!("={n}"), Tone::Warn),
        None => (
            format!("{}/{}", s.min_instances(), s.max_instances().map_or("-".into(), |m| m.to_string())),
            if s.min_instances() > 0 { Tone::Warn } else { Tone::Plain },
        ),
    };
    let cpu = if s.cpu_always_on() { ("always", Tone::Warn) } else { ("req", Tone::Muted) };
    let mut out = vec![
        (s.id().to_string(), Tone::Plain),
        (s.short(s.revision()).to_string(), Tone::Muted),
        (age, Tone::Muted),
        scaling,
        (cpu.0.to_string(), cpu.1),
    ];
    match row.health {
        Some(h) => {
            let percent = |class: &str, warn: f64, bad: f64| match h.percent(class) {
                None => dash(),
                Some(p) => {
                    let tone = if p >= bad {
                        Tone::Bad
                    } else if p >= warn {
                        Tone::Warn
                    } else {
                        Tone::Muted
                    };
                    (format!("{p:.1}"), tone)
                }
            };
            out.push((fmt_count(h.requests), if h.requests > 0.0 { Tone::Plain } else { Tone::Muted }));
            out.push(percent("4xx", 10.0, 50.0));
            out.push(percent("5xx", 0.05, 1.0));
            out.push((format!("{:.1}", h.billed_secs / 3600.0), Tone::Plain));
            out.push(match h.percentile(0.99) {
                Some(p) => (fmt_ms(p), if row.held() { Tone::Bad } else { Tone::Plain }),
                None => dash(),
            });
        }
        None => out.extend((0..5).map(|_| dash())),
    }
    if with_costs {
        match row.cost {
            Some(c) => {
                out.push((format!("{:.2}", c.month_to_date), Tone::Plain));
                out.push((format!("{:.2}", c.yesterday), if c.spike { Tone::Bad } else { Tone::Plain }));
            }
            None => out.extend([dash(), dash()]),
        }
    }
    let (notes, tone) = notes(row);
    out.push((notes, tone));
    out
}

/// What stands out about a row, most serious first.
fn notes(row: &Row) -> (String, Tone) {
    let s = row.service;
    let mut notes = Vec::new();
    let mut tone = Tone::Muted;
    if row.held() {
        notes.push("held".to_string());
        tone = Tone::Bad;
    } else if let Some(near) = row.health.map(|h| h.near_timeout(s.timeout_secs())).filter(|n| *n >= 1.0) {
        // Not at p99, but some requests reached the timeout: worth a look (crons, streams).
        notes.push(format!("{} near timeout", fmt_count(near.round())));
        tone = Tone::Warn;
    }
    if s.latest_not_ready() {
        notes.push(format!("{} not ready", s.short(run::last_segment(&s.latest_created_revision))));
        tone = Tone::Bad;
    }
    if row.cost.is_some_and(|c| c.spike) {
        notes.push("cost spike".into());
        tone = Tone::Bad;
    }
    if let Some(split) = s.traffic_split() {
        notes.push(split);
        if tone == Tone::Muted {
            tone = Tone::Warn;
        }
    }
    (notes.join(" · "), tone)
}

/// The list, read at once (the terminal UI reads the same pieces as they come).
pub struct Listing {
    pub services: Vec<Service>,
    pub health: std::result::Result<HashMap<String, Health>, GcpError>,
    pub costs: std::result::Result<Option<CostFile>, String>,
}

impl Listing {
    /// The services and their last day, read at the same time, and the costs file.
    pub fn load(gcp: &Gcp, env_name: &str, now: f64) -> Result<Listing> {
        let (services, health) = std::thread::scope(|s| {
            let health = s.spawn(|| health::last_day(gcp, now));
            let services = run::all_services(gcp);
            let health = health.join().unwrap_or_else(|_| Err(GcpError::Network("reading the metrics failed".into())));
            (services, health)
        });
        Ok(Listing { services: services?, health, costs: costs::read(env_name) })
    }

    pub fn rows(&self) -> Vec<Row<'_>> {
        let costs = self.costs.as_ref().ok().and_then(Option::as_ref);
        self.services.iter().map(|s| Row::new(s, self.health.as_ref().ok(), costs)).collect()
    }
}

/// An app with everything its detail shows.
pub struct Detail {
    pub service: Service,
    pub revisions: std::result::Result<Vec<Revision>, GcpError>,
    pub secrets: std::result::Result<Vec<secrets::Secret>, GcpError>,
    pub charts: std::result::Result<Charts, GcpError>,
}

/// When the running revision was made: from its own record when read, else from the service.
pub fn revision_created(service: &Service, revisions: Option<&[Revision]>) -> Option<f64> {
    revisions
        .and_then(|revs| revs.iter().find(|r| r.id() == service.revision()))
        .and_then(Revision::created)
        .or_else(|| service.revision_ready_at())
}

impl Detail {
    /// Finds the app, then reads its revisions, secrets and charts at the same time.
    pub fn load(gcp: &Gcp, convention: AppsConvention, app: &str, range: Range, now: f64) -> Result<Detail> {
        let service = run::find_service(gcp, app)?;
        let failed = || GcpError::Network("a read failed unexpectedly".into());
        let (revisions, secrets, charts) = std::thread::scope(|s| {
            let revisions = s.spawn(|| run::recent_revisions(gcp, &service.name, REVISIONS));
            let secrets = s.spawn(|| secrets::load(gcp, convention, &service));
            let charts = s.spawn(|| health::charts(gcp, service.id(), range, now));
            (
                revisions.join().unwrap_or_else(|_| Err(failed())),
                secrets.join().unwrap_or_else(|_| Err(failed())),
                charts.join().unwrap_or_else(|_| Err(failed())),
            )
        });
        Ok(Detail { service, revisions, secrets, charts })
    }

    pub fn revision_created(&self) -> Option<f64> {
        revision_created(&self.service, self.revisions.as_deref().ok())
    }

    pub fn secret_states<'a>(&'a self, project: &str) -> Vec<secrets::State<'a>> {
        match &self.secrets {
            Ok(list) => secrets::states(project, list, &self.service.secret_refs(), self.revision_created()),
            Err(_) => Vec::new(),
        }
    }

    /// The detail as `gcli app --json` and the MCP server give it. Values of secrets and of
    /// plain variables are never in it.
    pub fn json(&self, project: &str, cost: Option<&AppCost>) -> Value {
        let s = &self.service;
        let row = Row { service: s, health: None, cost };
        let mut v = row.json();
        let obj = v.as_object_mut().expect("an object");
        obj.remove("held");
        obj.insert("ingress".into(), json!(s.ingress));
        obj.insert(
            "scaling".into(),
            json!({
                "serviceMin": s.service_min(),
                "revisionMin": s.revision_min(),
                "manualInstances": s.manual_instances(),
            }),
        );
        let limits = s.template.containers.first().map(|c| c.resources.limits.clone()).unwrap_or_default();
        obj.insert("limits".into(), json!(limits));
        obj.insert("concurrency".into(), json!(s.template.max_instance_request_concurrency));
        obj.insert("serviceAccount".into(), json!(s.template.service_account));
        let problems: Vec<Value> = s
            .conditions
            .iter()
            .chain([&s.terminal_condition])
            .filter(|c| !c.kind.is_empty() && !c.succeeded())
            .map(|c| json!({ "type": c.kind, "state": c.state, "message": c.message }))
            .collect();
        obj.insert("conditionsNotMet".into(), json!(problems));
        match &self.charts {
            Ok(c) => {
                let summary = c.summary();
                let mut h = health_json(&summary, s.timeout_secs());
                h["window"] = json!(c.range.label());
                h["maxInstances"] = json!(c.instances().iter().map(|p| p.1).fold(0.0, f64::max));
                obj.insert("held".into(), json!(summary.held(s.timeout_secs())));
                obj.insert("health".into(), h);
            }
            Err(e) => {
                obj.insert("healthError".into(), json!(e.short()));
            }
        }
        match &self.revisions {
            Ok(revs) => {
                let list: Vec<Value> = revs
                    .iter()
                    .map(|r| {
                        json!({
                            "name": r.id(),
                            "created": r.create_time,
                            "creator": r.creator,
                            "image": r.image_short(),
                            "restartedByGcli": r.labels.contains_key(run::RESTART_LABEL),
                            "envNames": r.plain_env_names(),
                            "secrets": refs_json(&r.secret_refs()),
                        })
                    })
                    .collect();
                obj.insert("revisions".into(), json!(list));
            }
            Err(e) => {
                obj.insert("revisionsError".into(), json!(e.short()));
            }
        }
        match &self.secrets {
            Ok(_) => {
                let list: Vec<Value> = self
                    .secret_states(project)
                    .iter()
                    .map(|st| {
                        let versions: Value = match &st.secret.versions {
                            Ok(vs) => json!(
                                vs.iter()
                                    .map(|v| json!({
                                        "version": v.number(),
                                        "state": v.state,
                                        "created": v.create_time,
                                        "destroyed": Some(&v.destroy_time).filter(|d| !d.is_empty()),
                                    }))
                                    .collect::<Vec<_>>()
                            ),
                            Err(e) => json!({ "error": e.short() }),
                        };
                        json!({
                            "secret": st.secret.key,
                            "own": st.secret.own,
                            "created": Some(&st.secret.create_time).filter(|c| !c.is_empty()),
                            "boundAs": refs_json(&st.bindings),
                            "versions": versions,
                            "findings": st.findings.iter().map(secrets::Finding::text).collect::<Vec<_>>(),
                        })
                    })
                    .collect();
                obj.insert("secrets".into(), json!(list));
            }
            Err(e) => {
                obj.insert("secretsError".into(), json!(e.short()));
            }
        }
        v
    }
}

fn refs_json(refs: &[run::SecretRef]) -> Value {
    json!(
        refs.iter()
            .map(|r| json!({ "target": r.target, "secret": r.secret_id(), "version": r.version, "mounted": r.mounted }))
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::MockServer;

    fn service(json: &str) -> Service {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn whiteghost_names_an_apps_secrets() {
        assert_eq!(AppsConvention::Whiteghost.secret_prefix("shop").as_deref(), Some("shop-prod-"));
        assert_eq!(AppsConvention::Run.secret_prefix("shop"), None);
    }

    #[test]
    fn rows_sort_by_cost_requests_errors_and_name() {
        let services: Vec<Service> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| service(&format!(r#"{{"name":"projects/p/locations/r/services/{n}"}}"#)))
            .collect();
        let mut health = HashMap::new();
        let h = |requests: f64, e5: f64, e4: f64| Health {
            requests,
            classes: [("2xx".to_string(), requests - e5 - e4), ("5xx".into(), e5), ("4xx".into(), e4)].into(),
            ..Health::default()
        };
        health.insert("a".to_string(), h(10.0, 0.0, 5.0));
        health.insert("b".to_string(), h(500.0, 2.0, 0.0));
        health.insert("c".to_string(), h(50.0, 2.0, 9.0));
        let costs: CostFile = serde_json::from_str(
            r#"{"readAt":"2026-11-05T08:00:00Z","lastCompleteDay":"2026-11-04","apps":{"c":{"monthToDate":9},"a":{"monthToDate":30}}}"#,
        )
        .unwrap();
        let ids = |by: Sort| {
            let mut rows: Vec<Row> = services.iter().map(|s| Row::new(s, Some(&health), Some(&costs))).collect();
            sort(&mut rows, by, |r| *r);
            rows.iter().map(|r| r.service.id().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(ids(Sort::Name), ["a", "b", "c", "d"]);
        assert_eq!(ids(Sort::Cost), ["a", "c", "b", "d"]);
        assert_eq!(ids(Sort::Requests), ["b", "c", "a", "d"]);
        assert_eq!(ids(Sort::Errors), ["c", "b", "a", "d"], "5xx first, then 4xx");
        // `d` has no traffic: zero, not unknown.
        let d = Row::new(&services[3], Some(&health), Some(&costs));
        assert_eq!(d.health.map(|h| h.requests), Some(0.0));
        assert_eq!(Row::new(&services[3], None, None).health.map(|h| h.requests), None);
    }

    #[test]
    fn cells_flag_what_needs_attention() {
        let s = service(
            r#"{"name":"projects/p/locations/r/services/stream","template":{"timeout":"300s","scaling":{"minInstanceCount":1}},
                "latestReadyRevision":"projects/p/locations/r/services/stream/revisions/stream-00002-b",
                "latestCreatedRevision":"projects/p/locations/r/services/stream/revisions/stream-00003-c"}"#,
        );
        let mut held = Health { requests: 40.0, classes: [("2xx".to_string(), 40.0)].into(), ..Health::default() };
        held.latency = Some(crate::gcp::monitoring::Distribution {
            buckets: crate::gcp::monitoring::Buckets::Explicit(vec![1000.0, 280_000.0, 300_000.0]),
            counts: vec![10.0, 0.0, 30.0],
        });
        let cost = AppCost { month_to_date: 12.0, yesterday: 4.0, median14: 1.0, spike: true };
        let row = Row { service: &s, health: Some(&held), cost: Some(&cost) };
        assert!(row.held());
        let shown = cells(&row, true);
        assert_eq!(shown.len(), columns(true).len());
        assert_eq!(shown[0].0, "stream");
        assert_eq!(shown[1].0, "00002-b");
        assert_eq!(shown[3], ("1/-".to_string(), Tone::Warn));
        assert_eq!(shown[9].1, Tone::Bad, "p99 at the timeout");
        assert_eq!(shown[11], ("4.00".to_string(), Tone::Bad), "a cost spike");
        assert_eq!(shown[12], ("held · 00003-c not ready · cost spike".to_string(), Tone::Bad));
        // A busy app with a few requests at the timeout: not held at p99, but noted.
        let mut busy = Health { requests: 1003.0, classes: [("2xx".to_string(), 1003.0)].into(), ..Health::default() };
        busy.latency = Some(crate::gcp::monitoring::Distribution {
            buckets: crate::gcp::monitoring::Buckets::Explicit(vec![1000.0, 280_000.0, 300_000.0]),
            counts: vec![1000.0, 0.0, 3.0],
        });
        let s2 = service(r#"{"name":"projects/p/locations/r/services/busy"}"#);
        let busy_row = Row { service: &s2, health: Some(&busy), cost: None };
        assert!(!busy_row.held());
        assert_eq!(cells(&busy_row, false).last().unwrap(), &("3 near timeout".to_string(), Tone::Warn));
        assert_eq!(cells(&Row { service: &s, health: None, cost: None }, false).len(), columns(false).len());
        let json = row.json();
        assert_eq!((json["held"].as_bool(), json["last24h"]["requests"].as_f64()), (Some(true), Some(40.0)));
        assert_eq!(json["cost"]["spike"], true);
    }

    #[test]
    fn a_detail_reads_everything_at_once() {
        let server = MockServer::start(vec![]);
        server.route("/services/shop/revisions", crate::gcp::mock::Reply::json(200, r#"{"revisions":[]}"#));
        server.route("/services/shop", crate::gcp::mock::Reply::json(200, run::tests::SERVICE));
        server.route("/secrets?", crate::gcp::mock::Reply::json(200, r#"{"secrets":[]}"#));
        server.route("/versions", crate::gcp::mock::Reply::json(200, r#"{"versions":[]}"#));
        server.route("timeSeries", crate::gcp::mock::Reply::json(200, r#"{}"#));
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let detail = Detail::load(&gcp, AppsConvention::Whiteghost, "shop", Range::Day, 1_793_000_000.0).unwrap();
        assert_eq!(detail.service.id(), "shop");
        assert!(detail.revisions.is_ok() && detail.charts.is_ok());
        // The convention's prefix (none found) plus the three it reads.
        assert_eq!(detail.secrets.as_ref().unwrap().len(), 3);
        let json = detail.json("p", None);
        assert_eq!(json["scaling"]["serviceMin"], 2);
        assert_eq!(json["health"]["requests"], 0.0);
        let text = json.to_string();
        assert!(!text.contains("carriage return"), "plain values never leave: {text}");
        // 1 service + 1 revisions + 1 secret list + 3 versions + 4 metrics.
        assert_eq!(server.requests().len(), 10);
    }
}
