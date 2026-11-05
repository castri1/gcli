//! What the MCP tools `costs_summary` and `cost_spikes` answer. They read only what `gcli costs`
//! and the Costs module kept: BigQuery bills every query, so an AI agent never starts one on
//! its own. When there is nothing kept, or it is more than a day old, the answer asks the user
//! to run `gcli costs <env> --refresh`.

use serde_json::{Value, json};

use super::day::{Day, Range};
use super::spikes::{self, Rule};
use super::{Kind, Snapshot, command, report};
use crate::config::CostDefaults;
use crate::util::fmt_ago;

/// What was read is served for a day; after that the user is asked to read again.
const MAX_AGE_HOURS: f64 = 24.0;
/// Apps listed in a summary, most expensive first.
const TOP_APPS: usize = 25;

fn refresh_hint(env: &str) -> String {
    format!(
        "ask the user to run `gcli costs {env} --refresh` (BigQuery bills each read of the billing export, so this server never reads it itself)"
    )
}

/// The current read of `env`, if it is recent enough.
fn fresh(env: &str, now: f64) -> Result<Snapshot, String> {
    let s = super::load(env, Kind::Current)
        .ok_or_else(|| format!("No cost data has been read for '{env}' yet: {}.", refresh_hint(env)))?;
    if now - s.read_at > MAX_AGE_HOURS * 3600.0 {
        return Err(format!("The cost data of '{env}' was read {}: {}.", fmt_ago(s.read_at), refresh_hint(env)));
    }
    Ok(s)
}

/// This month so far, against the same complete days of last month, the forecast, budgets,
/// credits, services, apps and what they share, and the last complete day's spikes.
pub fn summary(env: &str, costs: &CostDefaults, now: f64) -> Result<Value, String> {
    let s = fresh(env, now)?;
    let rule = Rule::of(costs);
    let month = Day::of(s.read_at).month_start();
    let view = report::month_view(&s, month)
        .ok_or_else(|| format!("nothing of this month was read for '{env}': {}", refresh_hint(env)))?;
    let mut out = command::summary(&s, month, &view, false, None, &rule);
    let services: Vec<Value> = report::services(&s, &view)
        .into_iter()
        .map(|r| {
            json!({
                "service": r.service, "gross": r.total.gross, "net": r.total.net,
                "completeDays": r.now, "sameDaysLastMonth": r.before, "splitByApp": r.by_app,
            })
        })
        .collect();
    let (apps, shared) = report::apps(&s, &view, &rule);
    let total_apps = apps.len();
    let apps: Vec<Value> = apps
        .into_iter()
        .take(TOP_APPS)
        .map(|a| json!({ "app": a.app, "monthToDate": a.total, "lastCompleteDay": a.last_day, "median14": a.median14, "spike": a.spike }))
        .collect();
    out["services"] = Value::Array(services);
    out["apps"] = json!({ "listed": apps.len(), "of": total_apps, "list": apps });
    out["shared"] = serde_json::to_value(shared).unwrap_or_default();
    out["readAgo"] = json!(fmt_ago(s.read_at));
    Ok(out)
}

/// The spikes on `date` (a complete UTC day; default the last one), with the numbers behind
/// each. A day the current read does not cover is served from a window `gcli costs --check
/// --date` kept.
pub fn spikes(env: &str, date: Option<&str>, costs: &CostDefaults, now: f64) -> Result<Value, String> {
    let rule = Rule::of(costs);
    let (s, day) = match date {
        None => {
            let s = fresh(env, now)?;
            let day = s
                .last_complete_day()
                .ok_or_else(|| format!("no complete day was read for '{env}': {}", refresh_hint(env)))?;
            (s, day)
        }
        Some(text) => {
            let day = Day::parse(text).ok_or_else(|| format!("`date` is YYYY-MM-DD, not '{text}'"))?;
            let need = Range::new(spikes::history(day).from, day);
            let covers = |s: &Snapshot| {
                s.days.is_some_and(|d| d.covers(need)) && s.last_complete_day().is_some_and(|l| l >= day)
            };
            let s = super::load(env, Kind::Current)
                .filter(covers)
                .or_else(|| super::load(env, Kind::Days(need)).filter(covers))
                .ok_or_else(|| {
                    format!(
                        "{day} has not been read for '{env}' (it needs {} to {day}): ask the user to run `gcli costs {env} --check --date {day}`, which reads and keeps it",
                        need.from
                    )
                })?;
            (s, day)
        }
    };
    let found = report::spikes(&s, day, &rule)?;
    Ok(json!({
        "env": env,
        "project": s.project,
        "day": day,
        "utcDays": true,
        "readAt": report::rfc3339(s.read_at),
        "spikes": found,
        "rule": rule.describe(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::report::tests::snapshot;
    use crate::costs::tests::{NOW, day};

    #[test]
    fn served_from_the_cache_only_while_recent() {
        let costs = CostDefaults::default();
        let mut s = snapshot();
        s.env = "mcp-summary".into();
        let e = summary("mcp-summary", &costs, NOW).unwrap_err();
        assert!(e.contains("gcli costs mcp-summary --refresh"), "{e}");
        super::super::store(&s, &costs).unwrap();
        let v = summary("mcp-summary", &costs, NOW + 3600.0).unwrap();
        assert_eq!(v["lastCompleteDay"], "2026-11-04");
        assert_eq!(v["apps"]["list"][0]["app"], "orders");
        assert_eq!(v["spikes"]["found"][0]["name"], "orders");
        assert_eq!(v["month"]["complete"]["to"], "2026-11-04");
        assert!(v["budgets"].as_array().is_some_and(|b| !b.is_empty()));
        let e = summary("mcp-summary", &costs, NOW + 2.0 * 86_400.0).unwrap_err();
        assert!(e.contains("was read") && e.contains("--refresh"), "{e}");
    }

    #[test]
    fn spikes_of_a_day_read_or_kept() {
        let costs = CostDefaults::default();
        let mut s = snapshot();
        s.env = "mcp-spikes".into();
        super::super::store(&s, &costs).unwrap();
        let v = spikes("mcp-spikes", None, &costs, NOW).unwrap();
        assert_eq!((v["day"].as_str(), v["spikes"][0]["name"].as_str()), (Some("2026-11-04"), Some("orders")));
        let v = spikes("mcp-spikes", Some("2026-11-03"), &costs, NOW).unwrap();
        assert!(v["spikes"].as_array().unwrap().is_empty());
        // Not in the current read, and no window kept: how to get it.
        let e = spikes("mcp-spikes", Some("2026-10-05"), &costs, NOW).unwrap_err();
        assert!(e.contains("--check --date 2026-10-05"), "{e}");
        // A window `--check --date` kept.
        let need = Range::new(day("2026-09-21"), day("2026-10-05"));
        let window =
            Snapshot { kind: Kind::Days(need), days: Some(need), data_through: day("2026-10-06").start(), ..s };
        super::super::store(&window, &costs).unwrap();
        assert!(spikes("mcp-spikes", Some("2026-10-05"), &costs, NOW).is_ok());
        // Today is partial.
        assert!(spikes("mcp-spikes", Some("2026-11-05"), &costs, NOW).is_err());
    }
}
