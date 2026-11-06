//! Logs: what an app wrote, or any Cloud Logging filter. The domain under `gcli logs`, the Logs
//! module and the MCP tools: which entries to ask for, following a log without repeating an
//! entry, and grouping errors that differ only in their numbers and ids.
//!
//! Cloud Logging reads are free but limited to about 60 requests a minute per project: a
//! listing is one request for most apps, and a follower one every [`POLL_SECS`].

pub mod cmd;
pub mod mcp;

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::gcp::logging::{self, Entries, Entry, quote};
use crate::gcp::monitoring::rfc3339;
use crate::gcp::{Gcp, Result};
use crate::util::{ellipsize, sanitize, sanitize_json, utc_parts};

/// Cloud Logging's severities, lowest first.
pub const SEVERITIES: [&str; 9] =
    ["DEFAULT", "DEBUG", "INFO", "NOTICE", "WARNING", "ERROR", "CRITICAL", "ALERT", "EMERGENCY"];

/// `--errors`.
pub const ERRORS: &str = "ERROR";

/// One hour, when no window is asked for.
pub const DEFAULT_SINCE: f64 = 3600.0;

/// Seconds between two polls of a follower: 12 requests a minute, well inside the rate limit.
pub const POLL_SECS: u64 = 5;

/// How far back a follower keeps asking: an entry can reach Cloud Logging some seconds after its
/// timestamp, and it would be missed by asking only for what is newer than the last one seen.
const OVERLAP_SECS: f64 = 60.0;

/// Entries a poll asks for: 200 a second can be followed.
const POLL_PAGE: usize = 1000;

/// The log of the requests Cloud Run served, as a log name writes it.
const REQUEST_LOG: &str = "run.googleapis.com%2Frequests";

/// The longest window: nothing Cloud Logging keeps is older, and a date stays a date.
const MAX_SINCE: f64 = 3650.0 * 86_400.0;

/// A pattern of [`normalize`] is at most this long.
const MAX_PATTERN: usize = 300;

/// A severity name in any case, as Cloud Logging writes it.
pub fn parse_severity(text: &str) -> std::result::Result<&'static str, String> {
    let upper = text.trim().to_uppercase();
    SEVERITIES
        .iter()
        .find(|s| **s == upper)
        .copied()
        .ok_or_else(|| format!("'{}' is not a severity: one of {}", sanitize(text), SEVERITIES.join(", ")))
}

/// `15m`, `2h`, `3d` in seconds.
pub fn parse_since(text: &str) -> std::result::Result<f64, String> {
    let bad = || format!("'{}' is not a time window: minutes, hours or days, such as 15m, 2h or 3d", sanitize(text));
    let text = text.trim();
    let unit = match text.chars().last() {
        Some('m') => 60.0,
        Some('h') => 3600.0,
        Some('d') => 86_400.0,
        _ => return Err(bad()),
    };
    let digits = &text[..text.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let secs = digits.parse::<f64>().map_err(|_| bad())? * unit;
    if secs <= 0.0 || secs > MAX_SINCE {
        return Err(bad());
    }
    Ok(secs)
}

/// A window in its largest whole unit: `15m`, `2h`, `3d`.
pub fn fmt_window(secs: f64) -> String {
    let s = secs as u64;
    match s {
        _ if s >= 86_400 && s.is_multiple_of(86_400) => format!("{}d", s / 86_400),
        _ if s >= 3600 && s.is_multiple_of(3600) => format!("{}h", s / 3600),
        _ => format!("{}m", s / 60),
    }
}

/// Which entries to read.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// A Cloud Run service; with none, only `filter` says what to read.
    pub app: Option<String>,
    /// A Cloud Logging filter of the user's, as written.
    pub filter: Option<String>,
    /// The log of the requests served, instead of what the app wrote.
    pub requests: bool,
    /// This severity or worse.
    pub severity: Option<&'static str>,
    /// Text anywhere in an entry.
    pub search: Option<String>,
    /// Seconds back from now. `None` asks nothing of the time, for a filter with its own.
    pub since: Option<f64>,
}

impl Query {
    pub fn app(app: &str, since: f64) -> Query {
        Query { app: Some(app.to_string()), since: Some(since), ..Query::default() }
    }

    /// The filter's conditions, without the time. Every value is quoted, so a name can't change
    /// what the filter asks.
    pub fn conditions(&self, gcp: &Gcp) -> String {
        let mut parts = Vec::new();
        if let Some(app) = &self.app {
            parts.push("resource.type=\"cloud_run_revision\"".to_string());
            parts.push(format!("resource.labels.service_name={}", quote(app)));
            if !gcp.region.is_empty() {
                parts.push(format!("resource.labels.location={}", quote(&gcp.region)));
            }
        }
        let request_log = quote(&format!("projects/{}/logs/{REQUEST_LOG}", gcp.project));
        if self.requests {
            parts.push(format!("logName={request_log}"));
        } else if self.app.is_some() {
            parts.push(format!("logName!={request_log}"));
        }
        if let Some(severity) = self.severity {
            parts.push(format!("severity>={severity}"));
        }
        if let Some(search) = &self.search {
            parts.push(self.search_condition(gcp, search));
        }
        if let Some(filter) = &self.filter {
            // On its own line, so a `-- comment` at its end doesn't swallow what follows.
            parts.push(format!("({filter}\n)"));
        }
        parts.join(" AND ")
    }

    /// `text` in any field of an entry, in any case, as in the Logs Explorer's search box. Every
    /// entry's own labels and log name say the app, the region and the project, so a word of
    /// those would match them all: that one is looked for only in what the entry says.
    /// (`jsonPayload:"text"`, in all of a structured log, is not something Cloud Logging accepts.)
    fn search_condition(&self, gcp: &Gcp, text: &str) -> String {
        let q = quote(text);
        let app = self.app.as_deref().unwrap_or_default();
        let every_entry = format!(
            "{app} {} {} cloud_run_revision {REQUEST_LOG} run.googleapis.com%2Fstdout run.googleapis.com%2Fstderr run.googleapis.com%2Fvarlog%2Fsystem",
            gcp.region, gcp.project
        );
        if !every_entry.to_lowercase().contains(&text.to_lowercase()) {
            return q;
        }
        let fields =
            ["textPayload", "jsonPayload.message", "jsonPayload.msg", "jsonPayload.error", "httpRequest.requestUrl"];
        format!("({})", fields.map(|f| format!("{f}:{q}")).join(" OR "))
    }

    /// The whole filter, as of `now`.
    pub fn filter_at(&self, gcp: &Gcp, now: f64) -> String {
        with_time(&self.conditions(gcp), self.since.map(|s| rfc3339(now - s)))
    }

    /// What the query asks, for a person: `orders, last 1h, ERROR or worse`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        match (&self.app, &self.filter) {
            (Some(app), Some(_)) => parts.push(format!("{app} (with a filter)")),
            (Some(app), None) => parts.push(app.clone()),
            _ => parts.push("the filter".to_string()),
        }
        if self.requests {
            parts.push("requests log".into());
        }
        if let Some(since) = self.since {
            parts.push(format!("last {}", fmt_window(since)));
        }
        if let Some(severity) = self.severity {
            parts.push(format!("{severity} or worse"));
        }
        if let Some(search) = &self.search {
            parts.push(format!("matching \"{search}\""));
        }
        sanitize(&parts.join(", "))
    }

    /// What to say when a read found nothing.
    pub fn nothing_found(&self, env: &str, more: bool) -> String {
        let mut text = format!("No entries for {} in {}.", self.describe(), sanitize(env));
        if more {
            text.push_str(" Google stopped searching before the start of the window, so there may be some: ask for a shorter one.");
        } else if self.app.is_some() {
            text.push_str(&format!(
                " Check the app's name with `{} apps {}`, or widen the window.",
                crate::util::program(),
                sanitize(env)
            ));
        }
        text
    }
}

fn with_time(conditions: &str, from: Option<String>) -> String {
    match from {
        Some(from) if conditions.is_empty() => format!("timestamp>={}", quote(&from)),
        Some(from) => format!("{conditions} AND timestamp>={}", quote(&from)),
        None => conditions.to_string(),
    }
}

/// `2026-10-19 20:25:01 UTC`, as the rest of the tool writes times, with the seconds a log needs.
pub fn fmt_time(epoch: f64) -> String {
    let (y, mo, d, h, mi, s) = utc_parts(epoch as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02} UTC")
}

/// An entry's severity; Google leaves out the lowest.
pub fn severity(e: &Entry) -> &str {
    if e.severity.is_empty() { SEVERITIES[0] } else { &e.severity }
}

/// How bad a severity is, to compare two.
fn rank(severity: &str) -> usize {
    SEVERITIES.iter().position(|s| *s == severity).unwrap_or(0)
}

/// An entry's time as this tool shows it.
pub fn time_of(e: &Entry) -> String {
    e.at().map_or_else(|| sanitize(&e.timestamp), fmt_time)
}

/// An entry's message for a terminal, line by line: safe to print, tabs as spaces (a terminal
/// and the viewer expand them differently), and nothing at the end.
pub fn message_lines(e: &Entry) -> Vec<String> {
    let text = sanitize(&e.message()).replace('\t', "    ");
    let lines: Vec<String> = text.trim_end().lines().map(str::to_string).collect();
    if lines.is_empty() { vec![String::new()] } else { lines }
}

/// What the lines after a message's first start with: they read as part of that entry, and a log
/// line can't pass for an entry of its own.
pub const CONTINUED: &str = "    ";

/// `run.googleapis.com/stderr` from `projects/p/logs/run.googleapis.com%2Fstderr`.
pub fn log_id(e: &Entry) -> String {
    e.log_name.rsplit("/logs/").next().unwrap_or_default().replace("%2F", "/")
}

/// An entry whole, with nothing cut, for `--json`.
pub fn entry_json(e: &Entry) -> Value {
    let mut v = json!({
        "time": e.timestamp,
        "severity": severity(e),
        "message": e.message(),
        "insertId": e.insert_id,
        "log": log_id(e),
        "resource": { "type": e.resource.kind, "labels": e.resource.labels },
    });
    if !e.labels.is_empty() {
        v["labels"] = json!(e.labels);
    }
    if !e.trace.is_empty() {
        v["trace"] = json!(e.trace);
    }
    if let Some(r) = &e.http_request {
        v["httpRequest"] = json!({
            "method": r.request_method,
            "url": r.request_url,
            "status": r.status,
            "latency": r.latency,
        });
    }
    if let Some(payload) = e.json_payload.as_ref().or(e.proto_payload.as_ref()) {
        v["payload"] = payload.clone();
    }
    sanitize_json(&mut v);
    v
}

// ---- following ----------------------------------------------------------------------------------

/// `t` with its microseconds: a follower's lower bound must rise even in a very busy log.
fn precise(t: f64) -> String {
    let micros = ((t - t.floor()) * 1e6) as u32;
    format!("{}.{micros:06}Z", rfc3339(t).trim_end_matches('Z'))
}

/// Reads what is new in a log, oldest first, and never gives the same entry twice.
pub struct Follower {
    conditions: String,
    /// Entries older than this are not asked for again.
    floor: f64,
    /// The entries given since `floor`, by `insertId`, with their times.
    seen: HashMap<String, f64>,
}

fn key(e: &Entry) -> String {
    if e.insert_id.is_empty() { e.timestamp.clone() } else { e.insert_id.clone() }
}

impl Follower {
    /// Follows on from `tail`, the newest entries already shown. `more`: there were older ones
    /// not shown, which should stay so.
    pub fn after(conditions: String, tail: &[Entry], more: bool, since_start: Option<f64>, now: f64) -> Follower {
        let times = || tail.iter().filter_map(Entry::at);
        let mut floor = times().reduce(f64::max).unwrap_or(now) - OVERLAP_SECS;
        floor = floor.max(since_start.unwrap_or(f64::MIN));
        if more {
            // What is older than the oldest shown was not shown: it must not appear now.
            if let Some(oldest) = times().reduce(f64::min) {
                floor = floor.max(oldest);
            }
        }
        let seen = tail.iter().map(|e| (key(e), e.at().unwrap_or(f64::INFINITY))).collect();
        Follower { conditions, floor, seen }
    }

    /// What arrived since the last poll, in one request.
    pub fn poll(&mut self, gcp: &Gcp) -> Result<Vec<Entry>> {
        let filter = with_time(&self.conditions, Some(precise(self.floor)));
        let page = logging::first_page(gcp, &filter, false, POLL_PAGE)?;
        Ok(self.take(page))
    }

    fn take(&mut self, page: Entries) -> Vec<Entry> {
        // A full page may be all known entries: only moving up to its end gets past them.
        let full = page.list.len() >= POLL_PAGE;
        let newest = page.list.iter().filter_map(Entry::at).reduce(f64::max);
        let fresh: Vec<Entry> = page
            .list
            .into_iter()
            .filter(|e| self.seen.insert(key(e), e.at().unwrap_or(f64::INFINITY)).is_none())
            .collect();
        if let Some(newest) = newest {
            self.floor = self.floor.max(if full { newest } else { newest - OVERLAP_SECS });
        }
        // A second of margin for what `precise` and the float's precision round.
        let keep_from = self.floor - 1.0;
        self.seen.retain(|_, at| *at >= keep_from);
        fresh
    }
}

// ---- the error summary --------------------------------------------------------------------------

/// A message with what changes between occurrences of the same error replaced by `#`: numbers,
/// hex, UUIDs, long ids, email addresses. Only its first line counts: stack traces differ.
pub fn normalize(message: &str) -> String {
    // First: replacing numbers would change the shapes that tell a credential.
    let message = crate::gcp::error::redact_tokens(message);
    let line = message.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or_default();
    let mut out = String::with_capacity(line.len());
    for (i, word) in line.split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if word.split_once('@').is_some_and(|(name, domain)| !name.is_empty() && domain.contains('.')) {
            out.push('#');
            continue;
        }
        let mut run = String::new();
        for c in word.chars() {
            if c.is_ascii_alphanumeric() {
                run.push(c);
            } else {
                normalize_token(&run, &mut out);
                run.clear();
                out.push(c);
            }
        }
        normalize_token(&run, &mut out);
    }
    // A UUID or a date is one thing, however many parts it has.
    while out.contains("#-#") {
        out = out.replace("#-#", "#");
    }
    ellipsize(&out, MAX_PATTERN)
}

/// One run of letters and digits. All of it goes when it looks like an id; otherwise only its
/// numbers (`worker12` is `worker#`), so words stay.
fn normalize_token(token: &str, out: &mut String) {
    let numbers = token.split(|c: char| !c.is_ascii_digit()).filter(|n| !n.is_empty()).count();
    let bare = token.strip_prefix("0x").unwrap_or(token);
    let hex = !token.is_empty() && bare.bytes().all(|b| b.is_ascii_hexdigit());
    let long_id = (numbers >= 2 && token.len() >= 8) || (numbers >= 1 && token.len() >= 16);
    // After `#-`: the next group of a UUID, which may have no digit (`abcd`).
    if long_id || (hex && (numbers > 0 || token.len() >= 8 || out.ends_with("#-"))) {
        out.push('#');
        return;
    }
    let mut in_number = false;
    for c in token.chars() {
        let digit = c.is_ascii_digit();
        if !digit {
            out.push(c);
        } else if !in_number {
            out.push('#');
        }
        in_number = digit;
    }
}

/// Entries that say the same thing.
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    /// The message, normalized.
    pub pattern: String,
    pub count: usize,
    pub last_seen: Option<f64>,
    /// The worst severity among them.
    pub severity: String,
    /// The newest one's message, whole.
    pub example: String,
}

/// `entries` grouped by [`normalize`], the most frequent first (then the most recent).
pub fn group(entries: &[Entry]) -> Vec<Group> {
    let mut groups: HashMap<String, Group> = HashMap::new();
    for e in entries {
        let message = e.message();
        let pattern = normalize(&message);
        let g = groups.entry(pattern.clone()).or_insert_with(|| Group {
            pattern,
            count: 0,
            last_seen: None,
            severity: severity(e).to_string(),
            example: String::new(),
        });
        g.count += 1;
        if rank(severity(e)) > rank(&g.severity) {
            g.severity = severity(e).to_string();
        }
        if g.example.is_empty() || e.at() > g.last_seen {
            g.last_seen = e.at().or(g.last_seen);
            g.example = message;
        }
    }
    let mut list: Vec<Group> = groups.into_values().collect();
    list.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.last_seen.partial_cmp(&a.last_seen).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.pattern.cmp(&b.pattern))
    });
    list
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gcp::mock::{MockServer, Reply};
    use std::sync::Arc;

    #[test]
    fn a_credential_does_not_survive_the_grouping() {
        let got = normalize("login failed token=ya29.a0AfH6SMB123 for 42");
        assert!(!got.contains("ya29") && !got.contains("a0AfH"), "{got}");
        assert!(got.contains("[redacted]"), "{got}");
    }

    pub(crate) fn client(fake: &MockServer) -> Arc<Gcp> {
        Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()))
    }

    fn entry(id: &str, time: &str, severity: &str, text: &str) -> Entry {
        Entry {
            insert_id: id.into(),
            timestamp: time.into(),
            severity: severity.into(),
            text_payload: text.into(),
            ..Entry::default()
        }
    }

    #[test]
    fn severities_and_windows_are_read() {
        assert_eq!(parse_severity(" warning "), Ok("WARNING"));
        assert!(parse_severity("loud").unwrap_err().contains("one of DEFAULT, DEBUG"));
        assert_eq!(parse_since("15m"), Ok(900.0));
        assert_eq!(parse_since("2h"), Ok(7200.0));
        assert_eq!(parse_since(" 3d "), Ok(259_200.0));
        for bad in ["", "m", "5", "5x", "0m", "-1h", "1.5h", "5é", "99999999d", "\u{1b}[2Jm"] {
            let e = parse_since(bad).unwrap_err();
            assert!(e.contains("15m, 2h or 3d") && !e.contains('\u{1b}'), "{bad}: {e}");
        }
        assert_eq!(fmt_window(900.0), "15m");
        assert_eq!(fmt_window(7200.0), "2h");
        assert_eq!(fmt_window(259_200.0), "3d");
        assert_eq!(fmt_window(5400.0), "90m");
    }

    #[test]
    fn an_app_is_its_service_in_the_region_without_the_request_log() {
        let fake = MockServer::start(vec![]);
        let gcp = client(&fake);
        let query = Query::app("shop", 3600.0);
        let filter = query.filter_at(&gcp, 1_762_300_000.0);
        assert_eq!(
            filter,
            "resource.type=\"cloud_run_revision\" AND resource.labels.service_name=\"shop\" \
             AND resource.labels.location=\"us-east1\" \
             AND logName!=\"projects/my-platform-123456/logs/run.googleapis.com%2Frequests\" \
             AND timestamp>=\"2025-11-04T22:46:40Z\""
        );
        let requests = Query { requests: true, ..query.clone() }.conditions(&gcp);
        assert!(requests.contains("logName=\"projects/my-platform-123456/logs/run.googleapis.com%2Frequests\""));
        let errors = Query { severity: Some(ERRORS), ..query.clone() }.conditions(&gcp);
        assert!(errors.contains("severity>=ERROR"), "{errors}");
        // Every region: no location.
        let everywhere = Gcp::with("p", "", crate::gcp::auth::tests::fixed("t"), None);
        assert!(!query.conditions(&everywhere).contains("location"));
    }

    #[test]
    fn values_cannot_change_what_a_filter_asks() {
        let fake = MockServer::start(vec![]);
        let gcp = client(&fake);
        let sneaky = r#"x" OR severity>=DEBUG OR "y"#;
        let query =
            Query { app: Some(sneaky.into()), search: Some(sneaky.into()), since: Some(60.0), ..Query::default() };
        let filter = query.conditions(&gcp);
        let quoted = r#""x\" OR severity>=DEBUG OR \"y""#;
        assert!(filter.contains(&format!("service_name={quoted}")), "{filter}");
        assert!(filter.contains(&format!("textPayload:{quoted}")), "{filter}");
        assert_eq!(filter.matches(sneaky).count(), 0, "never unquoted: {filter}");
        let filter = Query { search: Some(sneaky.into()), ..Query::app("shop", 60.0) }.conditions(&gcp);
        assert!(filter.ends_with(&format!(" AND {quoted}")), "{filter}");
    }

    #[test]
    fn a_search_looks_in_every_field_unless_every_entry_would_match() {
        let fake = MockServer::start(vec![]);
        let gcp = client(&fake);
        let searched = |text: &str| Query { search: Some(text.into()), ..Query::app("shop", 60.0) }.conditions(&gcp);
        assert!(searched("order 42").ends_with(" AND \"order 42\""), "{}", searched("order 42"));
        for word in ["shop", "SHO", "us-east1", "my-platform", "cloud_run", "stdout", "stderr", "googleapis"] {
            let filter = searched(word);
            let q = quote(word);
            assert!(
                filter.ends_with(&format!(
                    "(textPayload:{q} OR jsonPayload.message:{q} OR jsonPayload.msg:{q} OR jsonPayload.error:{q} OR httpRequest.requestUrl:{q})"
                )),
                "{filter}"
            );
        }
    }

    #[test]
    fn a_users_filter_stays_whole() {
        let fake = MockServer::start(vec![]);
        let gcp = client(&fake);
        let query = Query {
            filter: Some("resource.type=\"gce_instance\" OR x -- why".into()),
            severity: Some(ERRORS),
            ..Query::default()
        };
        assert_eq!(query.filter_at(&gcp, 0.0), "severity>=ERROR AND (resource.type=\"gce_instance\" OR x -- why\n)");
        let timed = Query { since: Some(60.0), filter: Some("a".into()), ..Query::default() };
        assert_eq!(timed.filter_at(&gcp, 120.0), "(a\n) AND timestamp>=\"1970-01-01T00:01:00Z\"");
        assert_eq!(Query { since: Some(60.0), ..Query::default() }.filter_at(&gcp, 120.0).matches(" AND ").count(), 0);
    }

    #[test]
    fn nothing_found_says_why_it_may_be() {
        let query = Query { severity: Some(ERRORS), ..Query::app("shop", 3600.0) };
        let text = query.nothing_found("prod", false);
        assert!(text.starts_with("No entries for shop, last 1h, ERROR or worse in prod."), "{text}");
        assert!(text.contains("apps prod"), "{text}");
        assert!(query.nothing_found("prod", true).contains("Google stopped searching"));
    }

    #[test]
    fn messages_are_safe_and_entries_stay_one_entry() {
        let e = entry("a", "2026-11-05T10:00:01.5Z", "", "boom\u{1b}[2J\u{1b}]52;c;ZXZpbA==\u{7}\n\tat main\n\n");
        assert_eq!(message_lines(&e), ["boom", "    at main"]);
        assert_eq!(time_of(&e), "2026-11-05 10:00:01 UTC");
        assert_eq!(severity(&e), "DEFAULT");
        assert_eq!(message_lines(&entry("b", "", "", "")), [""]);
        assert_eq!(time_of(&entry("c", "not\u{1b}[2J a time", "", "")), "not a time");
        let json: Entry = serde_json::from_str(
            r#"{"logName":"projects/p/logs/run.googleapis.com%2Fstderr","jsonPayload":{"message":"hi","k\u001b[2J":"v"},
                "httpRequest":{"status":500},"trace":"t1","labels":{"a":"b"}}"#,
        )
        .unwrap();
        let v = entry_json(&json);
        assert_eq!(v["log"], "run.googleapis.com/stderr");
        assert_eq!(v["message"], "hi");
        assert_eq!(v["payload"], json!({ "message": "hi", "k": "v" }), "keys are cleaned too");
        assert_eq!((v["httpRequest"]["status"].as_i64(), v["trace"].as_str()), (Some(500), Some("t1")));
    }

    #[test]
    fn numbers_ids_and_hex_group_together() {
        for (message, pattern) in [
            ("order 123 failed after 45ms", "order # failed after #ms"),
            ("order 9 failed after 1200ms", "order # failed after #ms"),
            ("user 550e8400-e29b-41d4-a716-446655440000 not found", "user # not found"),
            ("user aaaaaaaa-abcd-abcd-abcd-aaaaaaaaaaaa not found", "user # not found"),
            ("bad pointer 0xdeadbeef at 0x1f", "bad pointer # at #"),
            ("commit deadbeef missing", "commit # missing"),
            ("trace 4bf92f3577b34da6a3ce929d0e0e4736 lost", "trace # lost"),
            ("token Ab3dE9gH2kLm expired", "token # expired"),
            ("GET /api/orders/123/items 500", "GET /api/orders/#/items #"),
            ("worker-12 died", "worker-# died"),
            ("worker12 died", "worker# died"),
            ("at 2026-11-05T10:00:00.123Z: refused 10.0.0.7:5432", "at #T#:#:#.#Z: refused #.#.#.#:#"),
            ("login failed for ana@example.com", "login failed for #"),
            ("TypeError: x is not a function\n    at a.js:1:2", "TypeError: x is not a function"),
            ("\n\n  spaced    out  \n", "spaced out"),
            ("decade facade beef", "decade facade beef"),
            ("año 2026", "año #"),
            ("", ""),
        ] {
            assert_eq!(normalize(message), pattern, "{message}");
        }
        assert!(normalize(&"x ".repeat(1000)).chars().count() <= MAX_PATTERN);
    }

    #[test]
    fn groups_are_counted_most_frequent_first() {
        let list = [
            entry("1", "2026-11-05T10:00:03Z", "ERROR", "db timeout after 30s"),
            entry("2", "2026-11-05T10:00:02Z", "CRITICAL", "db timeout after 45s"),
            entry("3", "2026-11-05T10:00:01Z", "ERROR", "db timeout after 12s"),
            entry("4", "2026-11-05T10:00:09Z", "ERROR", "cache miss 1"),
            entry("5", "2026-11-05T10:00:00Z", "ERROR", "cache miss 2"),
            entry("6", "2026-11-05T09:00:00Z", "ERROR", "lonely"),
            entry("7", "2026-11-05T10:30:00Z", "ERROR", "newer lonely"),
        ];
        let groups = group(&list);
        let shape: Vec<(&str, usize)> = groups.iter().map(|g| (g.pattern.as_str(), g.count)).collect();
        assert_eq!(
            shape,
            [("db timeout after #s", 3), ("cache miss #", 2), ("newer lonely", 1), ("lonely", 1)],
            "by count, then by last seen"
        );
        assert_eq!(groups[0].severity, "CRITICAL", "the worst of them");
        assert_eq!(groups[0].example, "db timeout after 30s", "the newest");
        assert_eq!(groups[1].example, "cache miss 1");
        assert_eq!(groups[1].last_seen, crate::util::parse_rfc3339("2026-11-05T10:00:09Z"));
    }

    fn page(entries: &[(&str, &str)]) -> Reply {
        let list: Vec<Value> = entries.iter().map(|(id, t)| json!({ "insertId": id, "timestamp": t })).collect();
        Reply::json(200, &json!({ "entries": list }).to_string())
    }

    fn ids(list: &[Entry]) -> Vec<&str> {
        list.iter().map(|e| e.insert_id.as_str()).collect()
    }

    fn at(time: &str) -> f64 {
        crate::util::parse_rfc3339(time).unwrap()
    }

    #[test]
    fn a_follower_never_repeats_and_catches_late_arrivals() {
        let fake = MockServer::start(vec![]);
        let gcp = client(&fake);
        let tail = [entry("a", "2026-11-05T10:10:00Z", "", "x"), entry("b", "2026-11-05T10:09:00Z", "", "x")];
        let now = at("2026-11-05T10:10:05Z");
        let mut f = Follower::after("c=1".into(), &tail, false, Some(now - 3600.0), now);
        // The tail again, a new one, and one that arrived late with an older time.
        fake.push(page(&[
            ("b", "2026-11-05T10:09:00Z"),
            ("late", "2026-11-05T10:09:30Z"),
            ("a", "2026-11-05T10:10:00Z"),
        ]));
        fake.push(page(&[("a", "2026-11-05T10:10:00Z"), ("c", "2026-11-05T10:10:07Z")]));
        fake.push(page(&[("a", "2026-11-05T10:10:00Z"), ("c", "2026-11-05T10:10:07Z")]));
        assert_eq!(ids(&f.poll(&gcp).unwrap()), ["late"]);
        assert_eq!(ids(&f.poll(&gcp).unwrap()), ["c"]);
        assert!(f.poll(&gcp).unwrap().is_empty());
        let requests = fake.requests();
        assert_eq!(requests.len(), 3, "one request a poll");
        let bodies: Vec<Value> = requests.iter().map(|r| serde_json::from_str(&r.body).unwrap()).collect();
        assert_eq!(bodies[0]["orderBy"], "timestamp asc");
        assert_eq!(bodies[0]["filter"], "c=1 AND timestamp>=\"2026-11-05T10:09:00.000000Z\"");
        assert_eq!(bodies[2]["filter"], "c=1 AND timestamp>=\"2026-11-05T10:09:07.000000Z\"", "the floor follows");
    }

    #[test]
    fn a_follower_does_not_show_what_the_tail_left_out() {
        let tail = [entry("a", "2026-11-05T10:10:00Z", "", "x"), entry("b", "2026-11-05T10:09:59Z", "", "x")];
        let now = at("2026-11-05T10:10:05Z");
        let f = Follower::after("c".into(), &tail, true, None, now);
        assert_eq!(f.floor, at("2026-11-05T10:09:59Z"), "more entries exist before the oldest shown");
        let f = Follower::after("c".into(), &tail, false, None, now);
        assert_eq!(f.floor, at("2026-11-05T10:09:00Z"));
        let f = Follower::after("c".into(), &[], false, Some(now - 30.0), now);
        assert_eq!(f.floor, now - 30.0, "an empty window: from its start");
        let f = Follower::after("c".into(), &[], false, Some(now - 3600.0), now);
        assert_eq!(f.floor, now - OVERLAP_SECS, "a long empty window is not searched every time");
    }

    #[test]
    fn a_follower_gets_past_a_full_page_and_forgets_what_it_cannot_see_again() {
        let mut f = Follower { conditions: "c".into(), floor: at("2026-11-05T10:00:00Z"), seen: HashMap::new() };
        let full: Vec<Entry> = (0..POLL_PAGE)
            .map(|i| entry(&format!("e{i}"), &format!("2026-11-05T10:{:02}:00Z", i % 60), "", "x"))
            .collect();
        let first = f.take(Entries { list: full.clone(), more: true });
        assert_eq!(first.len(), POLL_PAGE);
        assert_eq!(f.floor, at("2026-11-05T10:59:00Z"), "up to the end of the page, not 60 s before it");
        assert!(f.seen.len() < 50, "only what can come again: {}", f.seen.len());
        assert!(f.take(Entries { list: full, more: true }).len() < POLL_PAGE);
        assert_eq!(precise(at("2026-11-05T10:00:00.25Z")), "2026-11-05T10:00:00.250000Z");
    }

    #[test]
    fn an_entry_without_an_id_is_not_repeated_either() {
        let mut f = Follower { conditions: "c".into(), floor: 0.0, seen: HashMap::new() };
        let e = entry("", "2026-11-05T10:00:00Z", "", "x");
        assert_eq!(f.take(Entries { list: vec![e.clone()], more: false }).len(), 1);
        assert!(f.take(Entries { list: vec![e], more: false }).is_empty());
    }
}
