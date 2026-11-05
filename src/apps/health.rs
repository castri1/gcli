//! An app's health, from Cloud Run's metrics in Cloud Monitoring: requests by response class,
//! latency percentiles, billed instance time and instances. For the list, one call per metric
//! covers every service (Google sums and groups by service); an app's detail reads the same
//! metrics as time series.
//!
//! **Held requests**: when the p99 latency sits at the request timeout, requests are being
//! kept open until Cloud Run cuts them (server-sent events, long polls, crons slower than
//! their timeout). Each one keeps an instance billed for the whole timeout, so a few of them
//! cost more than thousands of quick requests.

use std::collections::{BTreeMap, HashMap};

use crate::gcp::monitoring::{self, Distribution, Query, Series};
use crate::gcp::{Gcp, GcpError, Result};

const REQUESTS: &str = "run.googleapis.com/request_count";
const LATENCIES: &str = "run.googleapis.com/request_latencies";
const BILLED: &str = "run.googleapis.com/container/billable_instance_time";
const INSTANCES: &str = "run.googleapis.com/container/instance_count";

pub const DAY: f64 = 86_400.0;

/// A request is "near the timeout" when it ran at least this share of it.
pub const HELD_SHARE: f64 = 0.9;
/// A busy app's p99 hides its long requests (thousands of fast ones around a few dozen held for
/// five minutes), so it also holds requests when those take at least this share of its billed
/// instance time…
pub const HELD_BILLED_SHARE: f64 = 0.2;
/// …and there are at least this many of them (a couple of slow requests are not a pattern).
pub const HELD_MIN_REQUESTS: f64 = 10.0;

/// What `held` means, in one line, for the detail and the reports.
pub const HELD_EXPLAINED: &str = "requests run until the request timeout (p99 at the timeout, or such requests take a fifth or more of the billed time): SSE streams, long polls or slow crons held open until Cloud Run cuts them, each keeping an instance billed the whole time.";

/// A stretch of an app's traffic, summed.
#[derive(Debug, Clone, Default)]
pub struct Health {
    pub requests: f64,
    /// Requests by response class (`2xx`, `4xx`, `5xx`…).
    pub classes: BTreeMap<String, f64>,
    /// Billed instance time, in seconds.
    pub billed_secs: f64,
    pub latency: Option<Distribution>,
}

impl Health {
    pub fn count(&self, class: &str) -> f64 {
        self.classes.get(class).copied().unwrap_or(0.0)
    }

    /// The share of requests in `class`, in percent; `None` without requests.
    pub fn percent(&self, class: &str) -> Option<f64> {
        (self.requests > 0.0).then(|| self.count(class) / self.requests * 100.0)
    }

    /// Latency percentile in milliseconds (`q` = 0.99 for p99).
    pub fn percentile(&self, q: f64) -> Option<f64> {
        self.latency.as_ref().and_then(|d| d.percentile(q))
    }

    pub fn held(&self, timeout_secs: f64) -> bool {
        if held(self.percentile(0.99), timeout_secs) {
            return true;
        }
        let near = self.near_timeout(timeout_secs);
        near >= HELD_MIN_REQUESTS && self.near_timeout_share(timeout_secs).is_some_and(|s| s >= HELD_BILLED_SHARE)
    }

    /// About what share of the billed instance time went to requests near the timeout (each
    /// counted as the whole timeout; concurrent ones share an instance, so it can overstate).
    pub fn near_timeout_share(&self, timeout_secs: f64) -> Option<f64> {
        (self.billed_secs > 0.0).then(|| self.near_timeout(timeout_secs) * timeout_secs / self.billed_secs)
    }

    /// About how many requests ran at least [`HELD_SHARE`] of the timeout.
    pub fn near_timeout(&self, timeout_secs: f64) -> f64 {
        self.latency.as_ref().map_or(0.0, |d| d.at_least(HELD_SHARE * timeout_secs * 1000.0))
    }
}

/// Whether a p99 latency (ms) sits at a timeout (seconds).
pub fn held(p99_ms: Option<f64>, timeout_secs: f64) -> bool {
    p99_ms.is_some_and(|p| p >= HELD_SHARE * timeout_secs * 1000.0)
}

/// A query on Cloud Run revisions, in `region` (empty: all) and of `service` (if any).
fn run_query<'a>(
    metric: &'a str,
    region: &'a str,
    service: Option<&'a str>,
    window: (f64, f64),
    period: u64,
    aligner: &'a str,
    group_by: &'a [&'a str],
) -> Query<'a> {
    let mut resource_labels = Vec::new();
    if !region.is_empty() {
        resource_labels.push(("location", region));
    }
    if let Some(s) = service {
        resource_labels.push(("service_name", s));
    }
    Query {
        metric,
        resource: "cloud_run_revision",
        resource_labels,
        window,
        period,
        aligner,
        reducer: "REDUCE_SUM",
        group_by,
    }
}

fn joined<T>(r: std::thread::Result<Result<T>>) -> Result<T> {
    r.unwrap_or_else(|_| Err(GcpError::Network("a metrics read failed unexpectedly".into())))
}

/// The last day of every service in the client's region, by service id: three calls, one per
/// metric, at the same time.
pub fn last_day(gcp: &Gcp, now: f64) -> Result<HashMap<String, Health>> {
    let window = (now - DAY, now);
    let period = DAY as u64;
    let region = gcp.region.as_str();
    const BY_SERVICE: &[&str] = &["resource.label.service_name"];
    const BY_CLASS: &[&str] = &["resource.label.service_name", "metric.label.response_code_class"];
    let (requests, billed, latency) = std::thread::scope(|s| {
        let query = |metric, aligner, group_by| {
            s.spawn(move || monitoring::query(gcp, &run_query(metric, region, None, window, period, aligner, group_by)))
        };
        let requests = query(REQUESTS, "ALIGN_SUM", BY_CLASS);
        let billed = query(BILLED, "ALIGN_SUM", BY_SERVICE);
        // ALIGN_DELTA keeps each point a distribution, which REDUCE_SUM adds bucket by bucket.
        let latency = query(LATENCIES, "ALIGN_DELTA", BY_SERVICE);
        (joined(requests.join()), joined(billed.join()), joined(latency.join()))
    });
    Ok(by_service(&requests?, &billed?, &latency?))
}

/// Folds the three metrics' series into one [`Health`] per service.
fn by_service(requests: &[Series], billed: &[Series], latency: &[Series]) -> HashMap<String, Health> {
    let mut out: HashMap<String, Health> = HashMap::new();
    for s in requests {
        let h = out.entry(s.label("service_name").to_string()).or_default();
        let n: f64 = s.points.iter().map(|p| p.value).sum();
        h.requests += n;
        *h.classes.entry(class_of(s)).or_default() += n;
    }
    for s in billed {
        out.entry(s.label("service_name").to_string()).or_default().billed_secs +=
            s.points.iter().map(|p| p.value).sum::<f64>();
    }
    for s in latency {
        let h = out.entry(s.label("service_name").to_string()).or_default();
        let sum = Distribution::sum(s.points.iter().filter_map(|p| p.distribution.as_ref()));
        h.latency = match (h.latency.take(), sum) {
            (Some(mut a), Some(b)) => {
                a.add(&b);
                Some(a)
            }
            (a, b) => a.or(b),
        };
    }
    out
}

/// `2xx`… (requests with no response at all have no class).
fn class_of(s: &Series) -> String {
    match s.label("response_code_class") {
        "" => "none".to_string(),
        c => c.to_string(),
    }
}

/// The window an app's charts cover.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Range {
    /// 24 hours in 1-hour points.
    Day,
    /// 6 hours in 5-minute points.
    Hours6,
}

impl Range {
    pub fn secs(self) -> f64 {
        match self {
            Range::Day => DAY,
            Range::Hours6 => 6.0 * 3600.0,
        }
    }

    pub fn period(self) -> u64 {
        match self {
            Range::Day => 3600,
            Range::Hours6 => 300,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Hours6 => "6h",
        }
    }

    pub fn next(self) -> Range {
        match self {
            Range::Day => Range::Hours6,
            Range::Hours6 => Range::Day,
        }
    }
}

/// One app's metrics as time series (time, value), oldest first.
#[derive(Debug, Clone)]
pub struct Charts {
    pub range: Range,
    /// Start and end, Unix seconds.
    pub window: (f64, f64),
    /// Requests by response class.
    pub classes: BTreeMap<String, Vec<(f64, f64)>>,
    pub latency: Vec<(f64, Distribution)>,
    /// Instances by state (`active`, `idle`): the most seen in each point.
    pub instances: BTreeMap<String, Vec<(f64, f64)>>,
    /// Billed instance time in each point, seconds.
    pub billed: Vec<(f64, f64)>,
}

/// Adds up series point by point.
fn add_points(into: &mut Vec<(f64, f64)>, points: impl IntoIterator<Item = (f64, f64)>) {
    for (t, v) in points {
        match into.iter_mut().find(|(at, _)| *at == t) {
            Some((_, sum)) => *sum += v,
            None => into.push((t, v)),
        }
    }
    into.sort_by(|a, b| a.0.total_cmp(&b.0));
}

impl Charts {
    /// Every request, point by point.
    pub fn requests(&self) -> Vec<(f64, f64)> {
        let mut all = Vec::new();
        for points in self.classes.values() {
            add_points(&mut all, points.iter().copied());
        }
        all
    }

    pub fn class(&self, class: &str) -> &[(f64, f64)] {
        self.classes.get(class).map_or(&[], Vec::as_slice)
    }

    /// Latency percentile `q` of each point, ms.
    pub fn percentile(&self, q: f64) -> Vec<(f64, f64)> {
        self.latency.iter().filter_map(|(t, d)| Some((*t, d.percentile(q)?))).collect()
    }

    /// Instances in every state, point by point.
    pub fn instances(&self) -> Vec<(f64, f64)> {
        let mut all = Vec::new();
        for points in self.instances.values() {
            add_points(&mut all, points.iter().copied());
        }
        all
    }

    /// Billed instance time as instances billed on average in each point.
    pub fn billed_instances(&self) -> Vec<(f64, f64)> {
        let period = self.range.period() as f64;
        self.billed.iter().map(|(t, s)| (*t, s / period)).collect()
    }

    /// The whole window, summed.
    pub fn summary(&self) -> Health {
        let classes: BTreeMap<String, f64> =
            self.classes.iter().map(|(c, points)| (c.clone(), points.iter().map(|p| p.1).sum())).collect();
        Health {
            requests: classes.values().sum(),
            classes,
            billed_secs: self.billed.iter().map(|p| p.1).sum(),
            latency: Distribution::sum(self.latency.iter().map(|(_, d)| d)),
        }
    }
}

/// `service`'s charts over `range` ending `now`: four calls, one per metric, at the same time.
pub fn charts(gcp: &Gcp, service: &str, range: Range, now: f64) -> Result<Charts> {
    let window = (now - range.secs(), now);
    let period = range.period();
    let region = gcp.region.as_str();
    let s = Some(service);
    let (requests, latency, instances, billed) = std::thread::scope(|scope| {
        let query = |metric, aligner, group_by| {
            scope
                .spawn(move || monitoring::query(gcp, &run_query(metric, region, s, window, period, aligner, group_by)))
        };
        let requests = query(REQUESTS, "ALIGN_SUM", &["metric.label.response_code_class"]);
        let latency = query(LATENCIES, "ALIGN_DELTA", &[]);
        // The most instances seen in each point, by state.
        let instances = query(INSTANCES, "ALIGN_MAX", &["metric.label.state"]);
        let billed = query(BILLED, "ALIGN_SUM", &[]);
        (joined(requests.join()), joined(latency.join()), joined(instances.join()), joined(billed.join()))
    });
    let points = |s: &Series| s.points.iter().map(|p| (p.time, p.value)).collect::<Vec<_>>();
    let mut classes = BTreeMap::new();
    for s in requests? {
        add_points(classes.entry(class_of(&s)).or_default(), points(&s));
    }
    let mut instance_states = BTreeMap::new();
    for s in instances? {
        add_points(instance_states.entry(s.label("state").to_string()).or_default(), points(&s));
    }
    let mut latency_points: Vec<(f64, Distribution)> = Vec::new();
    for s in latency? {
        for p in s.points {
            let Some(d) = p.distribution else { continue };
            match latency_points.iter_mut().find(|(t, _)| *t == p.time) {
                Some((_, sum)) => sum.add(&d),
                None => latency_points.push((p.time, d)),
            }
        }
    }
    latency_points.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut billed_points = Vec::new();
    for s in billed? {
        add_points(&mut billed_points, points(&s));
    }
    Ok(Charts { range, window, classes, latency: latency_points, instances: instance_states, billed: billed_points })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    /// A latency distribution in Cloud Run's buckets ([10·1.1^(i-1), 10·1.1^i) ms), with `n`
    /// requests in the bucket that holds `ms`, for each `(ms, n)`.
    pub(crate) fn latency(buckets: &[(f64, u64)]) -> String {
        let index = |ms: f64| ((ms / 10.0).ln() / 1.1f64.ln()).floor() as usize + 1;
        let size = buckets.iter().map(|(ms, _)| index(*ms) + 1).max().unwrap_or(1);
        let mut counts = vec![0u64; size];
        for (ms, n) in buckets {
            counts[index(*ms)] += n;
        }
        let counts: Vec<String> = counts.iter().map(|c| format!("\"{c}\"")).collect();
        format!(
            r#"{{"distributionValue":{{"count":"{}","bucketOptions":{{"exponentialBuckets":{{"numFiniteBuckets":168,"growthFactor":1.1,"scale":10}}}},"bucketCounts":[{}]}}}}"#,
            buckets.iter().map(|b| b.1).sum::<u64>(),
            counts.join(",")
        )
    }

    /// A series of `service` with metric `labels` (JSON members) and one point at `end`.
    pub(crate) fn series(service: &str, labels: &str, end: &str, value: &str) -> String {
        format!(
            r#"{{"metric":{{"labels":{{{labels}}}}},"resource":{{"labels":{{"service_name":"{service}"}}}},"points":[{{"interval":{{"endTime":"{end}"}},"value":{value}}}]}}"#
        )
    }

    pub(crate) fn time_series(series: &[String]) -> Reply {
        Reply::json(200, &format!(r#"{{"timeSeries":[{}]}}"#, series.join(",")))
    }

    /// A fake Monitoring answering [`last_day`]: `shop` busy with some 5xx, `stream` holding
    /// half its requests at a 300 s timeout.
    pub(crate) fn route_last_day(server: &MockServer) {
        let end = "2026-11-05T12:00:00Z";
        server.route(
            "request_count",
            time_series(&[
                series("shop", r#""response_code_class":"2xx""#, end, r#"{"int64Value":"900"}"#),
                series("shop", r#""response_code_class":"5xx""#, end, r#"{"int64Value":"100"}"#),
                series("stream", r#""response_code_class":"2xx""#, end, r#"{"int64Value":"40"}"#),
            ]),
        );
        server.route(
            "billable_instance_time",
            time_series(&[
                series("shop", "", end, r#"{"doubleValue":7200}"#),
                series("stream", "", end, r#"{"doubleValue":86400}"#),
            ]),
        );
        server.route(
            "request_latencies",
            time_series(&[
                series("shop", "", end, &latency(&[(120.0, 990), (2_000.0, 10)])),
                series("stream", "", end, &latency(&[(150.0, 20), (299_000.0, 20)])),
            ]),
        );
    }

    #[test]
    fn the_last_day_of_every_service_in_three_calls() {
        let server = MockServer::start(vec![]);
        route_last_day(&server);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let now = crate::util::parse_rfc3339("2026-11-05T12:00:00Z").unwrap();
        let health = last_day(&gcp, now).unwrap();

        let requests = server.requests();
        assert_eq!(requests.len(), 3, "one call per metric for every service");
        for (metric, aligner, groups) in [
            (
                "request_count",
                "ALIGN_SUM",
                "resource.label.service_name&aggregation.groupByFields=metric.label.response_code_class",
            ),
            ("billable_instance_time", "ALIGN_SUM", "resource.label.service_name"),
            ("request_latencies", "ALIGN_DELTA", "resource.label.service_name"),
        ] {
            let r = requests.iter().find(|r| r.path.contains(metric)).unwrap();
            assert!(r.path.contains(&format!("perSeriesAligner={aligner}&")), "{}", r.path);
            assert!(r.path.contains("crossSeriesReducer=REDUCE_SUM"), "{}", r.path);
            assert!(r.path.ends_with(&format!("groupByFields={groups}")), "{}", r.path);
            assert!(r.path.contains("location%20%3D%20%22us-east1%22") && r.path.contains("alignmentPeriod=86400s"));
        }

        let shop = &health["shop"];
        assert_eq!((shop.requests, shop.count("5xx"), shop.percent("5xx")), (1000.0, 100.0, Some(10.0)));
        assert_eq!((shop.percent("4xx"), shop.billed_secs), (Some(0.0), 7200.0));
        let p99 = shop.percentile(0.99).unwrap();
        assert!((110.0..135.0).contains(&p99), "990 of 1000 at ~120 ms: {p99}");
        assert!(!shop.held(300.0));

        let stream = &health["stream"];
        assert!(stream.held(300.0), "p99 at 299 s of a 300 s timeout");
        assert!(!stream.held(3600.0), "the same latency is fine with an hour's timeout");
        assert!((19.0..=20.0).contains(&stream.near_timeout(300.0)), "{}", stream.near_timeout(300.0));
        assert_eq!(Health::default().percent("5xx"), None);
    }

    #[test]
    fn a_busy_app_holds_requests_when_they_take_its_billed_time() {
        // Buckets in ms: <1 s, 1 s–270 s, 270–300 s (near a 300 s timeout), 300 s and over.
        let app = |near: f64, billed_hours: f64| Health {
            requests: 10_000.0 + near,
            classes: BTreeMap::new(),
            billed_secs: billed_hours * 3600.0,
            latency: Some(Distribution {
                buckets: crate::gcp::monitoring::Buckets::Explicit(vec![1000.0, 270_000.0, 300_000.0]),
                counts: vec![10_000.0, 0.0, near, 0.0],
            }),
        };
        let cron = app(27.0, 2.5);
        assert!(cron.percentile(0.99).unwrap() < 1000.0, "the fast requests hide them from the p99");
        assert!(cron.held(300.0), "27 requests of ~5 min are ~90% of 2.5 billed hours");
        assert!(!app(4.0, 1.4).held(300.0), "a fifth of the billed time, but only 4 requests");
        assert!(!app(27.0, 100.0).held(300.0), "27 of them in 100 billed hours are not the cost");
        assert_eq!(app(27.0, 0.0).near_timeout_share(300.0), None);
    }

    #[test]
    fn held_is_the_p99_at_the_timeout() {
        assert!(held(Some(270_000.0), 300.0));
        assert!(!held(Some(269_000.0), 300.0));
        assert!(held(Some(3_500_000.0), 3600.0));
        assert!(!held(None, 300.0));
    }

    #[test]
    fn charts_of_one_service() {
        let (t1, t2) = ("2026-11-05T11:00:00Z", "2026-11-05T12:00:00Z");
        let server = MockServer::start(vec![]);
        server.route(
            "request_count",
            time_series(&[
                series("shop", r#""response_code_class":"2xx""#, t1, r#"{"int64Value":"10"}"#),
                series("shop", r#""response_code_class":"2xx""#, t2, r#"{"int64Value":"20"}"#),
                series("shop", r#""response_code_class":"5xx""#, t2, r#"{"int64Value":"5"}"#),
            ]),
        );
        server.route(
            "request_latencies",
            time_series(&[
                series("shop", "", t1, &latency(&[(100.0, 10)])),
                series("shop", "", t2, &latency(&[(900.0, 25)])),
            ]),
        );
        server.route(
            "instance_count",
            time_series(&[
                series("shop", r#""state":"active""#, t2, r#"{"int64Value":"2"}"#),
                series("shop", r#""state":"idle""#, t2, r#"{"int64Value":"1"}"#),
            ]),
        );
        server.route("billable_instance_time", time_series(&[series("shop", "", t2, r#"{"doubleValue":5400}"#)]));
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let now = crate::util::parse_rfc3339(t2).unwrap();
        let charts = charts(&gcp, "shop", Range::Day, now).unwrap();
        assert_eq!(server.requests().len(), 4);
        assert!(server.requests().iter().all(|r| r.path.contains("service_name%20%3D%20%22shop%22")));
        assert!(server.requests().iter().all(|r| r.path.contains("alignmentPeriod=3600s")));
        assert_eq!(charts.requests(), [(now - 3600.0, 10.0), (now, 25.0)]);
        assert_eq!(charts.class("5xx"), [(now, 5.0)]);
        assert_eq!(charts.instances(), [(now, 3.0)]);
        assert_eq!(charts.billed_instances(), [(now, 1.5)]);
        let p50: Vec<f64> = charts.percentile(0.5).iter().map(|p| p.1.round()).collect();
        assert!(p50[0] < 110.0 && p50[1] > 800.0, "{p50:?}");
        let summary = charts.summary();
        assert_eq!((summary.requests, summary.billed_secs), (35.0, 5400.0));
        assert_eq!(summary.latency.as_ref().map(Distribution::count), Some(35.0));
        assert_eq!(charts.window, (now - DAY, now));
    }
}
