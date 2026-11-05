//! BigQuery API v2: standard-SQL queries through `jobs.query`, for the billing export (Costs).
//!
//! BigQuery bills the bytes a query scans, so every query can first be dry-run ([`dry_run`]):
//! free, and it says how many bytes the real one would read. A query still running when the
//! request ends is polled (`getQueryResults`) until it is done, and its rows are then read a
//! page at a time.
//!
//! Rows come as `{"f": [{"v": …}]}` against the answer's `schema`, numbers as strings; [`cells`]
//! turns each into a JSON object keyed by column name with real numbers, so it deserializes
//! straight into a typed struct.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{Gcp, GcpError, Opts, Result};
use crate::util::uri_encode;

/// What one request waits for a query before answering "not done yet" (it is then polled):
/// under the HTTP agent's 30 s timeout.
const WAIT_MS: u64 = 20_000;
/// A query still running after this long is given up (billing-export queries take seconds).
const MAX_WAIT: Duration = Duration::from_secs(300);
/// Rows per page.
const PAGE_ROWS: u32 = 10_000;
/// A query never reads more pages than this (a token that never ends).
const MAX_PAGES: usize = 200;
/// BigQuery bills at least this much per query, and rounds up to whole MiB.
const MIN_BILLED: u64 = 10 * MIB;
const MIB: u64 = 1 << 20;

/// A named query parameter (`@project` in the SQL). Values never go into the SQL text, so a
/// project id or a date from the config cannot change the query.
#[derive(Debug, Clone)]
pub struct Param {
    name: String,
    kind: &'static str,
    value: String,
}

impl Param {
    pub fn string(name: &str, value: &str) -> Param {
        Param { name: name.into(), kind: "STRING", value: value.into() }
    }

    /// `YYYY-MM-DD`.
    pub fn date(name: &str, value: &str) -> Param {
        Param { name: name.into(), kind: "DATE", value: value.into() }
    }
}

/// A query run in `project`, which pays for it (`x-goog-user-project` too, so the caller's
/// quota and bill are that project's).
pub struct Query<'a> {
    pub project: &'a str,
    pub sql: &'a str,
    pub params: &'a [Param],
}

/// The rows of a finished query and what it cost.
#[derive(Debug)]
pub struct Answer<T> {
    pub rows: Vec<T>,
    /// What BigQuery bills: the bytes processed rounded up to whole MiB, at least 10 MiB, and
    /// nothing for an answer from its 24-hour cache.
    pub bytes_billed: u64,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Response {
    schema: Schema,
    job_reference: Option<JobReference>,
    page_token: String,
    rows: Vec<Value>,
    /// int64 as a string, as every BigQuery count.
    total_bytes_processed: String,
    total_bytes_billed: String,
    job_complete: bool,
    cache_hit: bool,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default)]
pub struct Field {
    pub name: String,
    /// `STRING`, `INTEGER`, `FLOAT`, `NUMERIC`, `BOOLEAN`, `TIMESTAMP`, `DATE`, `RECORD`…
    #[serde(rename = "type")]
    pub kind: String,
    /// `NULLABLE`, `REQUIRED` or `REPEATED`.
    pub mode: String,
    /// A `RECORD`'s own fields.
    pub fields: Vec<Field>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct JobReference {
    project_id: String,
    job_id: String,
    location: String,
}

fn queries_url(project: &str) -> String {
    format!("https://bigquery.googleapis.com/bigquery/v2/projects/{}/queries", uri_encode(project))
}

fn body(q: &Query, dry_run: bool) -> Value {
    let params: Vec<Value> = q
        .params
        .iter()
        .map(|p| json!({ "name": p.name, "parameterType": { "type": p.kind }, "parameterValue": { "value": p.value } }))
        .collect();
    json!({
        "query": q.sql,
        "useLegacySql": false,
        "parameterMode": "NAMED",
        "queryParameters": params,
        "dryRun": dry_run,
        "timeoutMs": WAIT_MS,
        "maxResults": PAGE_ROWS,
        // The same id makes a repeated request (after a lost connection) return the first
        // one's job instead of running, and billing, the query twice.
        "requestId": request_id(),
    })
}

/// A request id unique to this process and call: at most 36 characters, as BigQuery asks.
fn request_id() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    format!("gcli-{:x}-{:x}-{n:x}", nanos as u64, std::process::id())
}

fn opts<'a>(q: &Query<'a>) -> Opts<'a> {
    Opts { quota_project: Some(q.project), idempotent: true }
}

fn count(text: &str) -> u64 {
    text.trim().parse().unwrap_or(0)
}

/// What BigQuery would bill for scanning `bytes`: whole MiB, at least 10 MiB, nothing for nothing.
pub fn billed(bytes: u64) -> u64 {
    if bytes == 0 { 0 } else { bytes.div_ceil(MIB).saturating_mul(MIB).max(MIN_BILLED) }
}

/// The bytes `q` would scan: a dry run, which BigQuery does not bill.
pub fn dry_run(gcp: &Gcp, q: &Query) -> Result<u64> {
    let r: Response = gcp.post(&queries_url(q.project), &body(q, true), opts(q))?;
    Ok(count(&r.total_bytes_processed))
}

/// Runs `q` and reads every row into `T` (fields by column name).
pub fn query<T: DeserializeOwned>(gcp: &Gcp, q: &Query) -> Result<Answer<T>> {
    let mut r: Response = gcp.post(&queries_url(q.project), &body(q, false), opts(q))?;
    let started = Instant::now();
    while !r.job_complete {
        let Some(job) = r.job_reference.clone() else {
            return Err(unexpected("BigQuery answered without the job to wait for"));
        };
        if started.elapsed() > MAX_WAIT {
            return Err(GcpError::Network(format!(
                "the BigQuery job {} still runs after {} s: try again later",
                job.job_id,
                MAX_WAIT.as_secs()
            )));
        }
        let billed_so_far = r.total_bytes_billed.clone();
        r = results(gcp, q, &job, None)?;
        if r.total_bytes_billed.is_empty() {
            r.total_bytes_billed = billed_so_far;
        }
    }
    let bytes_billed = match r.total_bytes_billed.trim().parse::<u64>() {
        Ok(b) => b,
        Err(_) if r.cache_hit => 0,
        Err(_) => billed(count(&r.total_bytes_processed)),
    };
    let schema = r.schema.clone();
    let mut rows = Vec::new();
    let mut page = r;
    for _ in 0..MAX_PAGES {
        for row in &page.rows {
            let object = cells(&schema.fields, row);
            rows.push(T::deserialize(object).map_err(|e| unexpected(&e.to_string()))?);
        }
        if page.page_token.is_empty() {
            break;
        }
        let Some(job) = page.job_reference.clone() else { break };
        let token = page.page_token.clone();
        page = results(gcp, q, &job, Some(&token))?;
        // getQueryResults leaves the job out of later pages' answers.
        page.job_reference.get_or_insert(job);
    }
    Ok(Answer { rows, bytes_billed })
}

/// `getQueryResults`: waits for the job (up to [`WAIT_MS`]) and reads a page of its rows.
fn results(gcp: &Gcp, q: &Query, job: &JobReference, token: Option<&str>) -> Result<Response> {
    let project = if job.project_id.is_empty() { q.project } else { job.project_id.as_str() };
    let url = format!("{}/{}", queries_url(project), uri_encode(&job.job_id));
    let (wait, rows) = (WAIT_MS.to_string(), PAGE_ROWS.to_string());
    let mut query: Vec<(&str, &str)> = vec![("timeoutMs", &wait), ("maxResults", &rows)];
    if !job.location.is_empty() {
        query.push(("location", &job.location));
    }
    if let Some(t) = token {
        query.push(("pageToken", t));
    }
    let mut r: Response = gcp.get_with(&url, &query, opts(q))?;
    r.job_reference.get_or_insert_with(|| job.clone());
    Ok(r)
}

fn unexpected(what: &str) -> GcpError {
    GcpError::Network(format!("unexpected answer from BigQuery: {what}"))
}

/// A row (`{"f": [{"v": …}, …]}`) as a JSON object of its columns, typed by `fields`: numbers
/// as numbers (BigQuery sends them as strings), records as objects, repeated fields as arrays,
/// SQL NULL as `null`.
pub fn cells(fields: &[Field], row: &Value) -> Value {
    let values = row.get("f").and_then(Value::as_array);
    let mut object = Map::new();
    for (i, field) in fields.iter().enumerate() {
        let v = values.and_then(|vs| vs.get(i)).and_then(|c| c.get("v"));
        object.insert(field.name.clone(), cell(field, v));
    }
    Value::Object(object)
}

fn cell(field: &Field, v: Option<&Value>) -> Value {
    let Some(v) = v.filter(|v| !v.is_null()) else { return Value::Null };
    if field.mode.eq_ignore_ascii_case("REPEATED") {
        let one = Field { mode: String::new(), ..field.clone() };
        return Value::Array(v.as_array().into_iter().flatten().map(|item| cell(&one, item.get("v"))).collect());
    }
    let text = v.as_str().unwrap_or_default();
    let number = |n: Option<f64>| n.filter(|n| n.is_finite()).and_then(serde_json::Number::from_f64).map(Value::Number);
    match field.kind.to_ascii_uppercase().as_str() {
        "RECORD" | "STRUCT" => cells(&field.fields, v),
        "INTEGER" | "INT64" => match text.parse::<i64>() {
            Ok(i) => json!(i),
            Err(_) => number(text.parse().ok()).unwrap_or(Value::Null),
        },
        "FLOAT" | "FLOAT64" | "NUMERIC" | "BIGNUMERIC" => number(text.parse().ok()).unwrap_or(Value::Null),
        "BOOLEAN" | "BOOL" => json!(text.eq_ignore_ascii_case("true")),
        // Seconds: `1.7303328E9`, or microseconds when the integer form was asked for.
        "TIMESTAMP" => match text.parse::<i64>() {
            Ok(micros) if micros.unsigned_abs() > 100_000_000_000 => number(Some(micros as f64 / 1e6)),
            _ => number(text.parse().ok()),
        }
        .unwrap_or(Value::Null),
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::auth;
    use crate::gcp::mock::{MockServer, Reply};
    use std::sync::Arc;

    fn client(server: &MockServer) -> Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", auth::tests::fixed("ya29.test"), Some(server.url()))
    }

    const SCHEMA: &str = r#"{"fields":[
        {"name":"day","type":"STRING","mode":"NULLABLE"},
        {"name":"app","type":"STRING","mode":"NULLABLE"},
        {"name":"cost","type":"FLOAT","mode":"NULLABLE"},
        {"name":"rows","type":"INTEGER","mode":"NULLABLE"}]}"#;

    #[derive(Debug, Deserialize, PartialEq)]
    struct Row {
        day: String,
        app: Option<String>,
        cost: f64,
        rows: i64,
    }

    fn query_of(sql: &str) -> Query<'_> {
        Query { project: "my-billing-project", sql, params: &[] }
    }

    #[test]
    fn dry_runs_say_the_bytes_and_send_parameters() {
        let server = MockServer::start(vec![Reply::json(
            200,
            r#"{"kind":"bigquery#queryResponse","totalBytesProcessed":"123456789","jobComplete":true,"cacheHit":false}"#,
        )]);
        let params = [Param::string("project", "my-platform-123456"), Param::date("from", "2026-10-01")];
        let q = Query {
            project: "my-billing-project",
            sql: "SELECT 1 WHERE @project != '' AND @from > '2000-01-01'",
            params: &params,
        };
        assert_eq!(dry_run(&client(&server), &q).unwrap(), 123_456_789);
        let r = &server.requests()[0];
        assert_eq!(
            (r.method.as_str(), r.path.as_str()),
            ("POST", "/bigquery/bigquery/v2/projects/my-billing-project/queries")
        );
        assert_eq!(r.header("x-goog-user-project"), Some("my-billing-project"));
        let body: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!((body["dryRun"].as_bool(), body["useLegacySql"].as_bool()), (Some(true), Some(false)));
        assert_eq!(body["parameterMode"], "NAMED");
        assert_eq!(
            body["queryParameters"][1],
            json!({ "name": "from", "parameterType": { "type": "DATE" }, "parameterValue": { "value": "2026-10-01" } })
        );
        assert!(body["requestId"].as_str().unwrap().len() <= 36);
    }

    #[test]
    fn answers_are_parsed_into_typed_rows() {
        let server = MockServer::start(vec![Reply::json(
            200,
            &format!(
                r#"{{"schema":{SCHEMA},"jobReference":{{"projectId":"my-billing-project","jobId":"job_1","location":"us-east1"}},
                "totalRows":"2","rows":[{{"f":[{{"v":"2026-11-03"}},{{"v":"orders"}},{{"v":"2.5"}},{{"v":"12"}}]}},
                {{"f":[{{"v":"2026-11-03"}},{{"v":null}},{{"v":"0.5"}},{{"v":"3"}}]}}],
                "totalBytesProcessed":"50000000","totalBytesBilled":"50331648","jobComplete":true,"cacheHit":false}}"#
            ),
        )]);
        let a: Answer<Row> = query(&client(&server), &query_of("SELECT …")).unwrap();
        assert_eq!(
            a.rows,
            [
                Row { day: "2026-11-03".into(), app: Some("orders".into()), cost: 2.5, rows: 12 },
                Row { day: "2026-11-03".into(), app: None, cost: 0.5, rows: 3 },
            ]
        );
        assert_eq!(a.bytes_billed, 50_331_648);
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn slow_queries_are_polled_and_pages_followed() {
        let row =
            |day: &str, cost: &str| format!(r#"{{"f":[{{"v":"{day}"}},{{"v":"a"}},{{"v":"{cost}"}},{{"v":"1"}}]}}"#);
        let server = MockServer::start(vec![
            // Not done within the request: polled.
            Reply::json(
                200,
                r#"{"jobReference":{"projectId":"my-billing-project","jobId":"job_2","location":"us-east1"},"jobComplete":false}"#,
            ),
            Reply::json(200, r#"{"jobComplete":false}"#),
            Reply::json(
                200,
                &format!(
                    r#"{{"schema":{SCHEMA},"jobComplete":true,"totalBytesProcessed":"5000","rows":[{}],"pageToken":"p/2"}}"#,
                    row("2026-11-01", "1.5")
                ),
            ),
            Reply::json(
                200,
                &format!(r#"{{"jobComplete":true,"rows":[{},{}]}}"#, row("2026-11-02", "2"), row("2026-11-03", "3")),
            ),
        ]);
        let a: Answer<Row> = query(&client(&server), &query_of("SELECT …")).unwrap();
        assert_eq!(a.rows.iter().map(|r| r.cost).collect::<Vec<_>>(), [1.5, 2.0, 3.0]);
        // No billed bytes in the answer: the processed bytes, rounded up to the 10 MiB minimum.
        assert_eq!(a.bytes_billed, 10 * MIB);
        let paths: Vec<String> = server.requests().iter().map(|r| r.path.clone()).collect();
        let results = "/bigquery/bigquery/v2/projects/my-billing-project/queries/job_2?timeoutMs=20000&maxResults=10000&location=us-east1";
        assert_eq!(paths[1], results);
        assert_eq!(paths[2], results);
        assert_eq!(paths[3], format!("{results}&pageToken=p%2F2"));
        assert!(server.requests().iter().all(|r| r.header("x-goog-user-project") == Some("my-billing-project")));
    }

    #[test]
    fn errors_come_through() {
        let server = MockServer::start(vec![Reply::json(
            404,
            r#"{"error":{"code":404,"message":"Not found: Table my-billing-project:billing.export was not found in location US","errors":[{"reason":"notFound"}],"status":"NOT_FOUND"}}"#,
        )]);
        let e = query::<Row>(&client(&server), &query_of("SELECT …")).unwrap_err();
        assert!(matches!(&e, GcpError::Api { status: 404, code, .. } if code == "NOT_FOUND"), "{e:?}");
        // A job that never reports done without saying which job it is.
        let server = MockServer::start(vec![Reply::json(200, r#"{"jobComplete":false}"#)]);
        assert!(query::<Row>(&client(&server), &query_of("SELECT …")).is_err());
    }

    #[test]
    fn cells_follow_the_schema() {
        let fields: Vec<Field> = serde_json::from_value(json!([
            { "name": "n", "type": "INT64" },
            { "name": "ok", "type": "BOOLEAN" },
            { "name": "at", "type": "TIMESTAMP" },
            { "name": "at_micros", "type": "TIMESTAMP" },
            { "name": "tags", "type": "STRING", "mode": "REPEATED" },
            { "name": "credit", "type": "RECORD", "fields": [{ "name": "name", "type": "STRING" }, { "name": "amount", "type": "FLOAT" }] },
            { "name": "nothing", "type": "FLOAT" },
            { "name": "bad", "type": "FLOAT" },
        ]))
        .unwrap();
        let row = json!({ "f": [
            { "v": "42" }, { "v": "true" }, { "v": "1.7303328E9" }, { "v": "1730332800000000" },
            { "v": [{ "v": "a" }, { "v": "b" }] },
            { "v": { "f": [{ "v": "PROMOTION" }, { "v": "-2.5" }] } },
            { "v": null }, { "v": "NaN" },
        ] });
        assert_eq!(
            cells(&fields, &row),
            json!({ "n": 42, "ok": true, "at": 1_730_332_800.0, "at_micros": 1_730_332_800.0, "tags": ["a", "b"],
                    "credit": { "name": "PROMOTION", "amount": -2.5 }, "nothing": null, "bad": null })
        );
        assert_eq!(billed(0), 0);
        assert_eq!(billed(1), 10 * MIB);
        assert_eq!(billed(100_000_001), 100_663_296);
    }
}
