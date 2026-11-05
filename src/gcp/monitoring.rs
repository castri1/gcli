//! Cloud Monitoring API v3: time series, aligned and reduced by Google (one call can cover
//! every service of a project, grouped by a label), and percentiles read from distributions.
//! Reading metrics is free.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use super::{Gcp, Opts, Result};
use crate::util::{parse_rfc3339, uri_encode, utc_parts};

/// One `timeSeries.list` call.
pub struct Query<'a> {
    /// `run.googleapis.com/request_count`
    pub metric: &'a str,
    /// The monitored resource type (`cloud_run_revision`).
    pub resource: &'a str,
    /// Extra `resource.label.<key> = "<value>"` conditions (a region, one service).
    pub resource_labels: Vec<(&'a str, &'a str)>,
    /// Start and end, Unix seconds.
    pub window: (f64, f64),
    /// Seconds each point covers.
    pub period: u64,
    /// `ALIGN_SUM`, `ALIGN_DELTA`, `ALIGN_MAX`…
    pub aligner: &'a str,
    /// `REDUCE_SUM`… across the series that share the `group_by` labels.
    pub reducer: &'a str,
    /// `resource.label.service_name`, `metric.label.response_code_class`…
    pub group_by: &'a [&'a str],
}

/// A series after reduction: the labels it was grouped by (metric and resource labels
/// together, by their bare key: `service_name`, `response_code_class`) and its points, oldest
/// first.
#[derive(Debug, Clone, Default)]
pub struct Series {
    pub labels: BTreeMap<String, String>,
    pub points: Vec<Point>,
}

impl Series {
    pub fn label(&self, key: &str) -> &str {
        self.labels.get(key).map_or("", String::as_str)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Point {
    /// End of the interval it covers, Unix seconds.
    pub time: f64,
    /// The number, or a distribution's count.
    pub value: f64,
    pub distribution: Option<Distribution>,
}

/// Where a distribution's bucket boundaries are (Google's `BucketOptions`).
#[derive(Debug, Clone, PartialEq)]
pub enum Buckets {
    Linear { n: usize, width: f64, offset: f64 },
    Exponential { n: usize, growth: f64, scale: f64 },
    Explicit(Vec<f64>),
}

/// A histogram: how many values fell in each bucket. Bucket 0 is the underflow (below the
/// first boundary), the last the overflow.
#[derive(Debug, Clone, PartialEq)]
pub struct Distribution {
    pub buckets: Buckets,
    pub counts: Vec<f64>,
}

impl Distribution {
    pub fn count(&self) -> f64 {
        self.counts.iter().sum()
    }

    /// Lower and upper bound of bucket `i` (the overflow's upper bound is infinite; values
    /// are latencies, so the underflow starts at 0).
    fn bounds(&self, i: usize) -> (f64, f64) {
        let (lower, upper) = match &self.buckets {
            Buckets::Linear { n, width, offset } => {
                let at = |k: usize| offset + width * k as f64;
                match i {
                    0 => (f64::NEG_INFINITY, *offset),
                    i if i > *n => (at(*n), f64::INFINITY),
                    i => (at(i - 1), at(i)),
                }
            }
            Buckets::Exponential { n, growth, scale } => {
                let at = |k: usize| scale * growth.powi(k as i32);
                match i {
                    0 => (f64::NEG_INFINITY, *scale),
                    i if i > *n => (at(*n), f64::INFINITY),
                    i => (at(i - 1), at(i)),
                }
            }
            Buckets::Explicit(b) => match i {
                0 => (f64::NEG_INFINITY, b.first().copied().unwrap_or(0.0)),
                i if i >= b.len() => (b.last().copied().unwrap_or(0.0), f64::INFINITY),
                i => (b[i - 1], b[i]),
            },
        };
        (lower.max(0.0).min(upper), upper)
    }

    /// The value below which a share `q` (0.99 for p99) of the values fall, interpolated
    /// linearly inside its bucket, as Cloud Monitoring does. In the overflow bucket, its lower
    /// bound (all that is known). `None` without values.
    pub fn percentile(&self, q: f64) -> Option<f64> {
        let total = self.count();
        if total <= 0.0 {
            return None;
        }
        let rank = q.clamp(0.0, 1.0) * total;
        let mut below = 0.0;
        for (i, &c) in self.counts.iter().enumerate() {
            if c > 0.0 && below + c >= rank {
                let (lower, upper) = self.bounds(i);
                if upper.is_infinite() {
                    return Some(lower);
                }
                return Some(lower + (upper - lower) * ((rank - below) / c).clamp(0.0, 1.0));
            }
            below += c;
        }
        // Rounding left the rank past the end: the last bucket with values.
        let last = self.counts.iter().rposition(|c| *c > 0.0)?;
        let (lower, upper) = self.bounds(last);
        Some(if upper.is_infinite() { lower } else { upper })
    }

    /// About how many values were at least `x`, a bucket counted in proportion to how much of
    /// it lies above `x`.
    pub fn at_least(&self, x: f64) -> f64 {
        self.counts
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let (lower, upper) = self.bounds(i);
                if lower >= x {
                    c
                } else if upper <= x || upper.is_infinite() {
                    // Below the line, or an overflow that starts below it: nothing known above.
                    0.0
                } else {
                    c * (upper - x) / (upper - lower)
                }
            })
            .sum()
    }

    /// Adds `other`'s counts (same buckets; another layout is left out, which Google does not
    /// produce within one metric).
    pub fn add(&mut self, other: &Distribution) {
        if other.buckets != self.buckets {
            return;
        }
        if self.counts.len() < other.counts.len() {
            self.counts.resize(other.counts.len(), 0.0);
        }
        for (a, b) in self.counts.iter_mut().zip(&other.counts) {
            *a += b;
        }
    }

    /// The sum of `items` (`None` when empty).
    pub fn sum<'a>(items: impl IntoIterator<Item = &'a Distribution>) -> Option<Distribution> {
        let mut items = items.into_iter();
        let mut total = items.next()?.clone();
        items.for_each(|d| total.add(d));
        Some(total)
    }
}

/// `2026-11-05T21:01:27Z`
pub fn rfc3339(t: f64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(t as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// A filter value in double quotes.
fn quoted(v: &str) -> String {
    format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""))
}

impl Query<'_> {
    fn filter(&self) -> String {
        let mut parts = vec![
            format!("metric.type = {}", quoted(self.metric)),
            format!("resource.type = {}", quoted(self.resource)),
        ];
        for (k, v) in &self.resource_labels {
            parts.push(format!("resource.label.{k} = {}", quoted(v)));
        }
        parts.join(" AND ")
    }
}

/// Runs `q` over the client's project: every series it returns, across pages.
pub fn query(gcp: &Gcp, q: &Query) -> Result<Vec<Series>> {
    let url = format!("https://monitoring.googleapis.com/v3/projects/{}/timeSeries", uri_encode(&gcp.project));
    let (filter, start, end, period) = (q.filter(), rfc3339(q.window.0), rfc3339(q.window.1), format!("{}s", q.period));
    let mut params: Vec<(&str, &str)> = vec![
        ("filter", filter.as_str()),
        ("interval.startTime", start.as_str()),
        ("interval.endTime", end.as_str()),
        ("aggregation.alignmentPeriod", period.as_str()),
        ("aggregation.perSeriesAligner", q.aligner),
        ("aggregation.crossSeriesReducer", q.reducer),
    ];
    params.extend(q.group_by.iter().map(|g| ("aggregation.groupByFields", *g)));
    let raw: Vec<RawSeries> = gcp.list(&url, &params, "timeSeries", Opts::default())?;
    Ok(raw.into_iter().map(RawSeries::into_series).collect())
}

// ---- wire format ----------------------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct Labels {
    labels: BTreeMap<String, String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawSeries {
    metric: Labels,
    resource: Labels,
    points: Vec<RawPoint>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawPoint {
    interval: RawInterval,
    value: RawValue,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawInterval {
    end_time: String,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawValue {
    /// int64 values come as strings.
    int64_value: Option<Value>,
    double_value: Option<f64>,
    distribution_value: Option<RawDistribution>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawDistribution {
    bucket_options: RawBucketOptions,
    bucket_counts: Vec<Value>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawBucketOptions {
    linear_buckets: Option<RawLinear>,
    exponential_buckets: Option<RawExponential>,
    explicit_buckets: Option<RawExplicit>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawLinear {
    num_finite_buckets: usize,
    width: f64,
    offset: f64,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct RawExponential {
    num_finite_buckets: usize,
    growth_factor: f64,
    scale: f64,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RawExplicit {
    bounds: Vec<f64>,
}

/// An int64 sent as a string (or, by some proxies, a number).
fn int64(v: &Value) -> f64 {
    match v {
        Value::String(s) => s.parse().unwrap_or(0.0),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        _ => 0.0,
    }
}

impl RawSeries {
    fn into_series(self) -> Series {
        let mut labels = self.resource.labels;
        labels.extend(self.metric.labels);
        let mut points: Vec<Point> = self
            .points
            .into_iter()
            .filter_map(|p| {
                let time = parse_rfc3339(&p.interval.end_time)?;
                let distribution = p.value.distribution_value.map(RawDistribution::into_distribution);
                let value = match (&distribution, &p.value.int64_value, p.value.double_value) {
                    (Some(d), _, _) => d.count(),
                    (_, Some(v), _) => int64(v),
                    (_, _, Some(v)) => v,
                    _ => 0.0,
                };
                Some(Point { time, value, distribution })
            })
            .collect();
        points.sort_by(|a, b| a.time.total_cmp(&b.time));
        Series { labels, points }
    }
}

impl RawDistribution {
    fn into_distribution(self) -> Distribution {
        let o = self.bucket_options;
        let buckets = if let Some(e) = o.exponential_buckets {
            Buckets::Exponential { n: e.num_finite_buckets, growth: e.growth_factor, scale: e.scale }
        } else if let Some(l) = o.linear_buckets {
            Buckets::Linear { n: l.num_finite_buckets, width: l.width, offset: l.offset }
        } else {
            Buckets::Explicit(o.explicit_buckets.map(|e| e.bounds).unwrap_or_default())
        };
        Distribution { buckets, counts: self.bucket_counts.iter().map(int64).collect() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};

    fn exp(counts: &[f64]) -> Distribution {
        Distribution { buckets: Buckets::Exponential { n: 168, growth: 1.1, scale: 10.0 }, counts: counts.to_vec() }
    }

    #[test]
    fn percentiles_interpolate_inside_buckets() {
        // Linear buckets of 10 from 0: [0,10) [10,20) [20,30) …
        let d = Distribution {
            buckets: Buckets::Linear { n: 5, width: 10.0, offset: 0.0 },
            counts: vec![0.0, 50.0, 30.0, 20.0],
        };
        assert_eq!(d.count(), 100.0);
        assert_eq!(d.percentile(0.5), Some(10.0), "the 50th value ends the first bucket");
        assert_eq!(d.percentile(0.65), Some(15.0));
        assert_eq!(d.percentile(0.99), Some(29.5));
        assert_eq!(d.percentile(1.0), Some(30.0));
        // Exponential, as Cloud Run reports latencies: bucket i is [10·1.1^(i-1), 10·1.1^i) ms.
        let mut counts = vec![0.0; 6];
        counts[5] = 10.0; // [14.641, 16.105)
        let p = exp(&counts).percentile(0.5).unwrap();
        assert!((p - (14.641 + 0.5 * (16.1051 - 14.641))).abs() < 1e-3, "{p}");
        // The underflow starts at 0; the overflow is its lower bound.
        assert_eq!(exp(&[4.0]).percentile(0.5), Some(5.0));
        let overflow = Distribution { buckets: Buckets::Explicit(vec![100.0, 300_000.0]), counts: vec![0.0, 0.0, 3.0] };
        assert_eq!(overflow.percentile(0.99), Some(300_000.0));
        assert_eq!(exp(&[]).percentile(0.5), None);
    }

    #[test]
    fn counts_values_above_a_line() {
        let d = Distribution { buckets: Buckets::Explicit(vec![10.0, 20.0, 30.0]), counts: vec![5.0, 10.0, 10.0, 2.0] };
        // Half of [10,20), all of [20,30), and the overflow.
        assert_eq!(d.at_least(15.0), 5.0 + 10.0 + 2.0);
        assert_eq!(d.at_least(30.0), 2.0);
        assert_eq!(d.at_least(0.0), 27.0);
    }

    #[test]
    fn distributions_add_up() {
        let mut a = exp(&[1.0, 2.0]);
        a.add(&exp(&[0.0, 1.0, 5.0]));
        assert_eq!(a.counts, [1.0, 3.0, 5.0]);
        let other = Distribution { buckets: Buckets::Explicit(vec![1.0]), counts: vec![9.0] };
        a.add(&other);
        assert_eq!(a.count(), 9.0, "another layout is left out");
        assert_eq!(Distribution::sum([&exp(&[1.0]), &exp(&[2.0])]).unwrap().counts, [3.0]);
        assert_eq!(Distribution::sum(std::iter::empty()), None);
    }

    #[test]
    fn queries_grouped_series() {
        let server = MockServer::start(vec![
            Reply::json(
                200,
                r#"{"timeSeries":[
                {"metric":{"type":"run.googleapis.com/request_count","labels":{"response_code_class":"2xx"}},
                 "resource":{"type":"cloud_run_revision","labels":{"service_name":"shop","project_id":"p"}},
                 "metricKind":"DELTA","valueType":"INT64",
                 "points":[{"interval":{"startTime":"2026-11-05T10:00:00Z","endTime":"2026-11-05T11:00:00Z"},"value":{"int64Value":"7"}},
                           {"interval":{"startTime":"2026-11-05T09:00:00Z","endTime":"2026-11-05T10:00:00Z"},"value":{"int64Value":"3"}}]}],
                "nextPageToken":"p2"}"#,
            ),
            Reply::json(
                200,
                r#"{"timeSeries":[
                {"metric":{"type":"run.googleapis.com/request_latencies"},"resource":{"type":"cloud_run_revision","labels":{"service_name":"api"}},
                 "points":[{"interval":{"endTime":"2026-11-05T11:00:00Z"},"value":{"distributionValue":{"count":"3","mean":12.5,
                    "bucketOptions":{"exponentialBuckets":{"numFiniteBuckets":168,"growthFactor":1.1,"scale":10}},"bucketCounts":["1","0","2"]}}}]},
                {"metric":{"type":"run.googleapis.com/container/billable_instance_time"},"resource":{"labels":{"service_name":"api"}},
                 "points":[{"interval":{"endTime":"2026-11-05T11:00:00Z"},"value":{"doubleValue":3600.5}}]}]}"#,
            ),
        ]);
        let gcp = Gcp::with("p", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(server.url()));
        let q = Query {
            metric: "run.googleapis.com/request_count",
            resource: "cloud_run_revision",
            resource_labels: vec![("location", "us-east1")],
            window: (1_793_000_000.0, 1_793_086_400.0),
            period: 3600,
            aligner: "ALIGN_SUM",
            reducer: "REDUCE_SUM",
            group_by: &["resource.label.service_name", "metric.label.response_code_class"],
        };
        let series = query(&gcp, &q).unwrap();
        assert_eq!(series.len(), 3);
        assert_eq!((series[0].label("service_name"), series[0].label("response_code_class")), ("shop", "2xx"));
        let values: Vec<f64> = series[0].points.iter().map(|p| p.value).collect();
        assert_eq!(values, [3.0, 7.0], "oldest first");
        let d = series[1].points[0].distribution.as_ref().unwrap();
        assert_eq!((series[1].points[0].value, d.counts.as_slice()), (3.0, [1.0, 0.0, 2.0].as_slice()));
        assert_eq!(series[2].points[0].value, 3600.5);

        let path = &server.requests()[0].path;
        let expected = [
            "/monitoring/v3/projects/p/timeSeries?filter=",
            &uri_encode(r#"metric.type = "run.googleapis.com/request_count" AND resource.type = "cloud_run_revision" AND resource.label.location = "us-east1""#),
            "&interval.startTime=2026-10-26T07%3A33%3A20Z&interval.endTime=2026-10-27T07%3A33%3A20Z",
            "&aggregation.alignmentPeriod=3600s&aggregation.perSeriesAligner=ALIGN_SUM&aggregation.crossSeriesReducer=REDUCE_SUM",
            "&aggregation.groupByFields=resource.label.service_name&aggregation.groupByFields=metric.label.response_code_class",
        ]
        .concat();
        assert_eq!(path, &expected);
        assert!(server.requests()[1].path.ends_with("&pageToken=p2"));
    }

    #[test]
    fn filter_values_are_quoted() {
        let q = Query {
            metric: "m",
            resource: "r",
            resource_labels: vec![("service_name", "a\"b")],
            window: (0.0, 1.0),
            period: 60,
            aligner: "ALIGN_SUM",
            reducer: "REDUCE_SUM",
            group_by: &[],
        };
        assert_eq!(q.filter(), r#"metric.type = "m" AND resource.type = "r" AND resource.label.service_name = "a\"b""#);
        assert_eq!(rfc3339(1_440_938_160.9), "2015-08-30T12:36:00Z");
    }
}
