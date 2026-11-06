//! What the MCP tools `search_logs` and `error_summary` answer. Log text is data, may carry text
//! written to mislead an AI, and may hold personal data: the tools frame it as data, keep only
//! what a search needs of each entry (no payloads, no labels, no traces), and the summary gives
//! patterns, with numbers, ids and email addresses gone, rather than the messages themselves.

use serde_json::{Value, json};

use super::{Query, cmd::GROUP_READ, group};
use crate::gcp::logging::{self, Entry};
use crate::gcp::monitoring::rfc3339;
use crate::gcp::{Gcp, Result};
use crate::util::ellipsize;

/// The most entries `search_logs` returns, whatever is asked.
pub const MAX_LIMIT: usize = 200;
/// Entries `search_logs` returns unless asked.
pub const DEFAULT_LIMIT: usize = 50;
/// A message is cut here; a search can read one by narrowing to it.
const MAX_MESSAGE: usize = 2000;
/// What all the entries of an answer take at most: inside what a tool may return, so an answer
/// is never cut in the middle of its JSON. The newest entries are the ones kept.
const MAX_LIST_CHARS: usize = 36_000;

/// Reminds the agent of what it is holding, in every answer that quotes logs.
const PERSONAL: &str = "Logs may hold personal data: repeat only what the user's question needs.";

fn window(query: &Query) -> Value {
    json!(query.since.map(|s| format!("last {}", super::fmt_window(s))))
}

fn entry(e: &Entry) -> Value {
    let message = e.message();
    let mut v = json!({
        "time": e.timestamp,
        "severity": super::severity(e),
        "message": ellipsize(&crate::gcp::error::redact_tokens(&message), MAX_MESSAGE),
        "log": super::log_id(e),
    });
    if let Some(revision) = e.resource.labels.get("revision_name") {
        v["revision"] = json!(revision);
    }
    v
}

/// The newest entries that match, at most `limit` (and [`MAX_LIMIT`]).
pub fn search(gcp: &Gcp, env: &str, query: &Query, limit: usize, now: f64) -> Result<Value> {
    let limit = limit.clamp(1, MAX_LIMIT);
    let read = logging::entries(gcp, &query.filter_at(gcp, now), true, limit)?;
    let mut size = 0;
    let list: Vec<Value> = read
        .list
        .iter()
        .map(entry)
        .take_while(|v| {
            size += v.to_string().len();
            size <= MAX_LIST_CHARS
        })
        .collect();
    let cut = list.len() < read.list.len();
    let mut out = json!({
        "personalData": PERSONAL,
        "env": env,
        "project": gcp.project,
        "app": query.app,
        "window": window(query),
        "order": "newest first",
        "entries": list.len(),
        "read": read.list.len(),
        "more": read.more,
        "list": list,
    });
    if cut {
        out["cut"] = json!(
            "Too much text for one answer: only the newest entries are here. Ask for a shorter `since`, a `severity` or a `search`."
        );
    }
    if read.list.is_empty() {
        out["note"] = json!(query.nothing_found(env, read.more));
    }
    Ok(out)
}

/// The distinct errors, with their counts, most frequent first.
pub fn error_summary(gcp: &Gcp, env: &str, query: &Query, now: f64) -> Result<Value> {
    let read = logging::entries(gcp, &query.filter_at(gcp, now), true, GROUP_READ)?;
    let mut size = 0;
    let groups: Vec<Value> = group(&read.list)
        .iter()
        .map(|g| {
            json!({
                "pattern": g.pattern,
                "count": g.count,
                "lastSeen": g.last_seen.map(rfc3339),
                "severity": g.severity,
            })
        })
        .take_while(|v| {
            size += v.to_string().len();
            size <= MAX_LIST_CHARS
        })
        .collect();
    let mut out = json!({
        "personalData": PERSONAL,
        "env": env,
        "project": gcp.project,
        "app": query.app,
        "window": window(query),
        "patternsMean": "Messages with numbers, ids, hex and email addresses replaced by #, so the same error counts once. `search_logs` shows the entries behind one.",
        "read": read.list.len(),
        "more": read.more,
        "kinds": groups.len(),
        "groups": groups,
    });
    if read.more {
        out["countedOver"] = json!(format!(
            "Only the newest {} entries of the window were read: the counts are a lower bound. A shorter `since` counts them all.",
            read.list.len()
        ));
    }
    if read.list.is_empty() {
        out["note"] = json!(query.nothing_found(env, read.more));
    }
    Ok(out)
}
