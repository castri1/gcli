//! `gcli logs`: list, search and follow what an app wrote (or any Cloud Logging filter), and
//! `--errors --group`, the distinct errors with their counts.
//!
//! Output goes through a writer rather than `println!`: a log is often piped into `head` or
//! `grep`, and a reader that leaves is a normal end, not a failure. What is said about the read
//! (counts, notes) goes to stderr, so stdout holds only the log.

use std::io::Write;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{CONTINUED, DEFAULT_SINCE, ERRORS, Follower, POLL_SECS, Query, group, parse_severity, parse_since};
use crate::cli::{current_env, gcp_for, paint};
use crate::config::Config;
use crate::gcp::Gcp;
use crate::gcp::logging::{self, Entry};
use crate::util::{ellipsize, fmt_ago, now_secs, program, sanitize, sanitize_json};

/// Entries listed unless `--limit` says otherwise.
const LIMIT: usize = 100;
/// Entries shown before following on from them.
const FOLLOW_TAIL: usize = 20;
/// Entries read for a summary: one request.
pub const GROUP_READ: usize = 1000;
/// Longest wait between polls after failures.
const MAX_BACKOFF_SECS: u64 = 60;
/// A line of text output is cut here (`--json` cuts nothing).
const MAX_LINE: usize = 2000;
/// A summary's patterns are cut here (`--json` cuts nothing).
const MAX_PATTERN_SHOWN: usize = 160;

#[derive(clap::Args)]
pub struct Args {
    /// The app (its Cloud Run service); optional with --filter
    app: Option<String>,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// This severity or worse: DEFAULT, DEBUG, INFO, NOTICE, WARNING, ERROR, CRITICAL, ALERT, EMERGENCY
    #[arg(long, value_name = "LEVEL", value_parser = parse_severity)]
    severity: Option<&'static str>,
    /// ERROR or worse
    #[arg(long, conflicts_with = "severity")]
    errors: bool,
    /// Only entries with this text in any field (any case)
    #[arg(long, value_name = "TEXT")]
    search: Option<String>,
    /// How far back: 15m, 2h, 3d (default: 1h)
    #[arg(long, value_name = "DURATION", value_parser = parse_since)]
    since: Option<f64>,
    /// Entries to show (default: 100, 20 before following; --group reads 1000)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u32).range(1..=10_000))]
    limit: Option<u32>,
    /// The log of the requests served, instead of what the app wrote
    #[arg(long)]
    requests: bool,
    /// A Cloud Logging filter, for anything that isn't an app (and-ed with the rest)
    #[arg(long, value_name = "EXPR")]
    filter: Option<String>,
    /// Keep showing new entries, oldest first, until Ctrl-C
    #[arg(long, short = 'f')]
    follow: bool,
    /// With --errors: the distinct errors with a count and when last seen, most frequent first
    #[arg(long, requires = "errors", conflicts_with = "follow")]
    group: bool,
    /// Machine-readable output, nothing cut; with --follow, one object per line
    #[arg(long)]
    json: bool,
}

/// `gcli logs APP [ENV]`, and with `--filter`, where the app is optional, `gcli logs [ENV]`.
fn target(config: &Config, args: &Args, has_filter: bool) -> Result<(Option<String>, String)> {
    let (app, env) = match (args.app.clone(), args.env.clone()) {
        (Some(one), None) if has_filter && config.environments.contains_key(&one) => (None, Some(one)),
        other => other,
    };
    if app.is_none() && !has_filter {
        let p = program();
        bail!("which app? {p} logs APP [ENV] (see {p} apps), or --filter EXPR for anything that isn't an app");
    }
    let env = match env {
        Some(name) => config.env(&name).map(|_| name)?,
        None => current_env(config)?,
    };
    Ok((app, env))
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let filter = args.filter.clone().filter(|f| !f.trim().is_empty());
    let (app, env) = target(config, &args, filter.is_some())?;
    // A filter that speaks of time keeps it, unless a window is asked for.
    let timed = filter.as_deref().is_some_and(|f| f.contains("timestamp"));
    let since = args.since.or(if timed { None } else { Some(DEFAULT_SINCE) });
    let query = Query {
        app,
        filter,
        requests: args.requests,
        severity: if args.errors { Some(ERRORS) } else { args.severity },
        search: args.search.clone().filter(|s| !s.trim().is_empty()),
        since,
    };
    let limit = args.limit.map(|n| n as usize);
    let gcp = gcp_for(config, &env)?;
    let mut out = std::io::stdout().lock();
    if args.follow {
        let mut wait = |pause: Duration| {
            std::thread::sleep(pause);
            true
        };
        eprintln!("{}", paint(&format!("Following {} every {POLL_SECS} s; Ctrl-C stops.", query.describe()), "90"));
        follow(&gcp, &env, &query, limit.unwrap_or(FOLLOW_TAIL), args.json, &mut out, &mut wait)?;
    } else if args.group {
        summary(&gcp, &env, &query, limit.unwrap_or(GROUP_READ), args.json, &mut out)?;
    } else {
        list(&gcp, &env, &query, limit.unwrap_or(LIMIT), args.json, &mut out)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// What the answers say about themselves in `--json`.
fn header(env: &str, gcp: &Gcp, query: &Query) -> Value {
    json!({
        "env": env,
        "project": gcp.project,
        "app": query.app,
        "window": query.since.map(super::fmt_window),
    })
}

fn write_json(out: &mut impl Write, mut v: Value) {
    sanitize_json(&mut v);
    // A reader that left is not a failure.
    let _ = writeln!(out, "{}", serde_json::to_string_pretty(&v).unwrap_or_default());
}

fn note(text: &str) {
    eprintln!("{}", paint(&sanitize(text), "90"));
}

fn severity_color(severity: &str) -> &'static str {
    match severity {
        "ERROR" | "CRITICAL" | "ALERT" | "EMERGENCY" => "31",
        "WARNING" => "33",
        "NOTICE" => "36",
        "DEFAULT" | "DEBUG" => "90",
        _ => "0",
    }
}

/// `2026-11-05 10:00:01 UTC  ERROR     message`, the lines after the message's first indented.
fn write_entry(out: &mut impl Write, e: &Entry) -> std::io::Result<()> {
    let severity = sanitize(super::severity(e));
    let lines = super::message_lines(e);
    writeln!(
        out,
        "{}  {}  {}",
        paint(&super::time_of(e), "90"),
        paint(&format!("{severity:<8}"), severity_color(&severity)),
        ellipsize(&lines[0], MAX_LINE)
    )?;
    for line in &lines[1..] {
        writeln!(out, "{CONTINUED}{}", ellipsize(line, MAX_LINE))?;
    }
    Ok(())
}

fn list(gcp: &Gcp, env: &str, query: &Query, limit: usize, json: bool, out: &mut impl Write) -> Result<()> {
    let read = logging::entries(gcp, &query.filter_at(gcp, now_secs()), true, limit)?;
    if json {
        let mut v = header(env, gcp, query);
        v["count"] = json!(read.list.len());
        v["more"] = json!(read.more);
        if read.list.is_empty() {
            v["note"] = json!(query.nothing_found(env, read.more));
        }
        v["entries"] = read.list.iter().map(super::entry_json).collect();
        write_json(out, v);
        return Ok(());
    }
    if read.list.is_empty() {
        let _ = writeln!(out, "{}", query.nothing_found(env, read.more));
        return Ok(());
    }
    for e in &read.list {
        if write_entry(out, e).is_err() {
            return Ok(());
        }
    }
    let mut footer = format!("{} entries · {} · {env} · newest first", read.list.len(), query.describe());
    if read.more {
        footer.push_str(" · more exist: --limit, --since");
    }
    note(&footer);
    Ok(())
}

/// An entry as following prints it: a line of text, or one object per line.
fn show(out: &mut impl Write, e: &Entry, json: bool) -> std::io::Result<()> {
    if json { writeln!(out, "{}", super::entry_json(e)) } else { write_entry(out, e) }
}

/// Shows the newest `tail` entries, then what arrives, until the reader leaves or `wait` says
/// stop (Ctrl-C ends the process). Only a failure that may pass is waited out, for longer each
/// time.
fn follow(
    gcp: &Gcp,
    env: &str,
    query: &Query,
    tail: usize,
    json: bool,
    out: &mut impl Write,
    wait: &mut impl FnMut(Duration) -> bool,
) -> Result<()> {
    let now = now_secs();
    let read = logging::entries(gcp, &query.filter_at(gcp, now), true, tail)?;
    let since_start = query.since.map(|s| now - s);
    let mut follower = Follower::after(query.conditions(gcp), &read.list, read.more, since_start, now);
    if read.list.is_empty() {
        note(&format!("No entries yet for {} in {env}.", query.describe()));
    }
    for e in read.list.iter().rev() {
        if show(out, e, json).is_err() {
            return Ok(());
        }
    }
    let _ = out.flush();
    let mut pause = POLL_SECS;
    loop {
        if !wait(Duration::from_secs(pause)) {
            return Ok(());
        }
        match follower.poll(gcp) {
            Ok(new) => {
                pause = POLL_SECS;
                for e in &new {
                    if show(out, e, json).is_err() {
                        return Ok(());
                    }
                }
                if out.flush().is_err() {
                    return Ok(());
                }
            }
            Err(e) if e.is_retryable() => {
                pause = (pause * 2).min(MAX_BACKOFF_SECS);
                note(&format!("! could not read ({}); trying again in {pause} s", e.short()));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn summary(gcp: &Gcp, env: &str, query: &Query, limit: usize, json: bool, out: &mut impl Write) -> Result<()> {
    let read = logging::entries(gcp, &query.filter_at(gcp, now_secs()), true, limit)?;
    let groups = group(&read.list);
    if json {
        let mut v = header(env, gcp, query);
        v["read"] = json!(read.list.len());
        v["more"] = json!(read.more);
        if read.list.is_empty() {
            v["note"] = json!(query.nothing_found(env, read.more));
        }
        v["groups"] = groups
            .iter()
            .map(|g| {
                json!({
                    "pattern": g.pattern,
                    "count": g.count,
                    "lastSeen": g.last_seen.map(crate::gcp::monitoring::rfc3339),
                    "severity": g.severity,
                    "example": g.example,
                })
            })
            .collect();
        write_json(out, v);
        return Ok(());
    }
    if groups.is_empty() {
        let _ = writeln!(out, "{}", query.nothing_found(env, read.more));
        return Ok(());
    }
    let width = groups.iter().map(|g| g.count.to_string().len()).max().unwrap_or(0).max("COUNT".len());
    let _ = writeln!(out, "{}", paint(&format!("{:>width$}  {:<9}  ERROR", "COUNT", "LAST SEEN"), "90;1"));
    for g in &groups {
        let last = g.last_seen.map_or("—".to_string(), fmt_ago);
        let line = writeln!(
            out,
            "{:>width$}  {}  {}",
            g.count,
            paint(&format!("{last:<9}"), "90"),
            sanitize(&ellipsize(&g.pattern, MAX_PATTERN_SHOWN))
        );
        if line.is_err() {
            return Ok(());
        }
    }
    let mut footer = format!("{} kinds in {} entries · {} · {env}", groups.len(), read.list.len(), query.describe());
    if read.more {
        footer.push_str(&format!(" · counted over the newest {}: older ones not read", read.list.len()));
    }
    note(&footer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::tests::client;
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::config::Environment;
    use crate::gcp::mock::{MockServer, Reply};
    use clap::Parser;

    fn parsed(line: &[&str]) -> std::result::Result<Args, String> {
        match Cli::try_parse_from([&["gcli", "logs"], line].concat()).map_err(|e| e.to_string())?.command {
            Some(Command::Logs(args)) => Ok(args),
            _ => Err("not logs".into()),
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        for name in ["dev", "prod"] {
            config.environments.insert(
                name.into(),
                Environment { project: format!("acme-{name}"), region: "us-east1".into(), ..Default::default() },
            );
        }
        config
    }

    fn resolved(line: &[&str]) -> std::result::Result<(Option<String>, String), String> {
        let args = parsed(line)?;
        let has_filter = args.filter.is_some();
        target(&config(), &args, has_filter).map_err(|e| e.to_string())
    }

    #[test]
    fn app_and_environment_are_told_apart() {
        let dev = config().default_env().unwrap_or_default().to_string();
        assert_eq!(resolved(&["shop", "prod"]).unwrap(), (Some("shop".into()), "prod".into()));
        assert_eq!(resolved(&["shop"]).unwrap(), (Some("shop".into()), dev.clone()));
        // Without --filter, the first word is the app, whatever its name.
        assert_eq!(resolved(&["prod"]).unwrap(), (Some("prod".into()), dev.clone()));
        // With it, a lone environment name is the environment.
        assert_eq!(resolved(&["--filter", "x", "prod"]).unwrap(), (None, "prod".into()));
        assert_eq!(resolved(&["--filter", "x", "shop"]).unwrap(), (Some("shop".into()), dev.clone()));
        assert_eq!(resolved(&["--filter", "x"]).unwrap(), (None, dev));
        assert!(resolved(&[]).unwrap_err().contains("which app?"));
        assert!(resolved(&["shop", "nowhere"]).unwrap_err().contains("nowhere"));
    }

    #[test]
    fn options_are_checked() {
        let args = parsed(&["shop", "--errors", "--group", "--since", "2h", "--limit", "5"]).unwrap();
        assert!(args.errors && args.group);
        assert_eq!((args.since, args.limit), (Some(7200.0), Some(5)));
        assert_eq!(parsed(&["shop", "--severity", "warning"]).unwrap().severity, Some("WARNING"));
        for bad in [
            &["shop", "--severity", "loud"][..],
            &["shop", "--since", "5x"],
            &["shop", "--limit", "0"],
            &["shop", "--limit", "10001"],
            &["shop", "--group"],
            &["shop", "--errors", "--severity", "INFO"],
            &["shop", "--errors", "--group", "--follow"],
        ] {
            assert!(parsed(bad).is_err(), "{bad:?}");
        }
    }

    const PAGE: &str = r#"{"entries":[
        {"insertId":"b","timestamp":"2026-11-05T10:00:02Z","severity":"ERROR","textPayload":"boom\u001b[2J\n\tat main"},
        {"insertId":"a","timestamp":"2026-11-05T10:00:01Z","jsonPayload":{"message":"hello"}}
    ]}"#;

    fn filter_sent(fake: &MockServer, i: usize) -> String {
        let body: Value = serde_json::from_str(&fake.requests()[i].body).unwrap();
        body["filter"].as_str().unwrap().to_string()
    }

    #[test]
    fn lists_newest_first_with_time_severity_and_a_safe_message() {
        let fake = MockServer::start(vec![Reply::json(200, PAGE)]);
        let mut out = Vec::new();
        let query = Query::app("shop", 3600.0);
        list(&client(&fake), "prod", &query, 100, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "2026-11-05 10:00:02 UTC  ERROR     boom\n        at main\n2026-11-05 10:00:01 UTC  DEFAULT   hello\n"
        );
        let body: Value = serde_json::from_str(&fake.requests()[0].body).unwrap();
        assert_eq!((body["orderBy"].as_str(), body["pageSize"].as_u64()), (Some("timestamp desc"), Some(100)));
        assert!(filter_sent(&fake, 0).contains("service_name=\"shop\""));
    }

    #[test]
    fn json_cuts_nothing_and_text_does() {
        let long = "x".repeat(MAX_LINE + 500);
        let page =
            json!({ "entries": [{ "insertId": "a", "timestamp": "2026-11-05T10:00:01Z", "textPayload": long }] });
        let fake = MockServer::start(vec![Reply::json(200, &page.to_string()), Reply::json(200, &page.to_string())]);
        let gcp = client(&fake);
        let query = Query::app("shop", 3600.0);
        let mut out = Vec::new();
        list(&gcp, "prod", &query, 1, true, &mut out).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["entries"][0]["message"].as_str().unwrap().len(), MAX_LINE + 500);
        assert_eq!(
            (v["env"].as_str(), v["app"].as_str(), v["window"].as_str()),
            (Some("prod"), Some("shop"), Some("1h"))
        );
        assert_eq!((v["count"].as_u64(), v["more"].as_bool()), (Some(1), Some(false)));
        let mut out = Vec::new();
        list(&gcp, "prod", &query, 1, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().trim_end().ends_with('…'));
    }

    #[test]
    fn an_unknown_app_is_a_clear_message() {
        let fake = MockServer::start(vec![Reply::json(200, "{}"), Reply::json(200, "{}")]);
        let gcp = client(&fake);
        let query = Query::app("nope", 3600.0);
        let mut out = Vec::new();
        list(&gcp, "prod", &query, 100, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("No entries for nope, last 1h in prod."), "{text}");
        let mut out = Vec::new();
        list(&gcp, "prod", &query, 100, true, &mut out).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!((v["count"].as_u64(), v["entries"].as_array().map(Vec::len)), (Some(0), Some(0)));
        assert!(v["note"].as_str().unwrap().contains("No entries for nope"));
    }

    #[test]
    fn google_errors_come_through_for_the_sign_in_hint() {
        let fake = MockServer::start(vec![Reply::json(
            403,
            r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"no access"}}"#,
        )]);
        let e = list(&client(&fake), "prod", &Query::app("shop", 3600.0), 1, false, &mut Vec::new()).unwrap_err();
        // main looks for this type to add the hint.
        assert!(e.downcast_ref::<crate::gcp::GcpError>().is_some(), "{e:#}");
        assert!(e.to_string().contains("PERMISSION_DENIED"));
    }

    /// A writer whose reader has left.
    struct Gone;
    impl Write for Gone {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn a_reader_that_leaves_is_not_a_failure() {
        let fake = MockServer::start(vec![Reply::json(200, PAGE), Reply::json(200, PAGE)]);
        let gcp = client(&fake);
        let query = Query::app("shop", 3600.0);
        list(&gcp, "prod", &query, 100, false, &mut Gone).unwrap();
        follow(&gcp, "prod", &query, 20, false, &mut Gone, &mut |_| panic!("no reader: no polling")).unwrap();
    }

    #[test]
    fn follows_oldest_first_without_repeats_at_a_gentle_pace() {
        let fake = MockServer::start(vec![
            Reply::json(200, PAGE),
            // Known entries again, and a new one.
            Reply::json(
                200,
                r#"{"entries":[{"insertId":"a","timestamp":"2026-11-05T10:00:01Z"},
                               {"insertId":"b","timestamp":"2026-11-05T10:00:02Z"},
                               {"insertId":"c","timestamp":"2026-11-05T10:00:07Z","textPayload":"new"}]}"#,
            ),
            // A failure that may pass, then the same again.
            Reply::json(400, r#"{"error":{"status":"RETRY_ME_NOT","message":"x"}}"#),
        ]);
        let gcp = client(&fake);
        let query = Query { requests: true, ..Query::app("shop", 3600.0) };
        let mut out = Vec::new();
        let mut pauses = Vec::new();
        let e = follow(&gcp, "prod", &query, 20, false, &mut out, &mut |p| {
            pauses.push(p.as_secs());
            true
        })
        .unwrap_err();
        assert!(e.to_string().contains("RETRY_ME_NOT"), "a failure that won't pass ends it: {e}");
        assert_eq!(pauses, [POLL_SECS, POLL_SECS]);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "{text}");
        assert!(lines[0].ends_with("hello") && lines[1].contains("boom") && lines[3].ends_with("new"), "{text}");
        let polled = filter_sent(&fake, 1);
        assert!(polled.contains("logName=\"projects/my-platform-123456/logs/run.googleapis.com%2Frequests\""));
        let body: Value = serde_json::from_str(&fake.requests()[1].body).unwrap();
        assert_eq!(body["orderBy"], "timestamp asc");
    }

    #[test]
    fn following_waits_out_a_failure_that_may_pass() {
        let throttled = r#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","message":"Quota"}}"#;
        // The client tries a throttled request four times itself.
        let mut replies = vec![Reply::json(200, "{}")];
        replies.extend((0..4).map(|_| Reply::json(429, throttled)));
        replies.extend((0..4).map(|_| Reply::json(429, throttled)));
        replies.push(Reply::json(200, r#"{"entries":[{"insertId":"c","timestamp":"2026-11-05T10:00:07Z"}]}"#));
        let fake = MockServer::start(replies);
        let mut pauses = Vec::new();
        let mut out = Vec::new();
        follow(&client(&fake), "prod", &Query::app("shop", 3600.0), 20, true, &mut out, &mut |p| {
            pauses.push(p.as_secs());
            pauses.len() < 4
        })
        .unwrap();
        assert_eq!(pauses, [POLL_SECS, 2 * POLL_SECS, 4 * POLL_SECS, POLL_SECS]);
        let text = String::from_utf8(out).unwrap();
        let v: Value = serde_json::from_str(text.trim()).expect("one object per line");
        assert_eq!(v["insertId"], "c");
    }

    #[test]
    fn the_summary_counts_distinct_errors() {
        let page = r#"{"entries":[
            {"insertId":"1","timestamp":"2026-11-05T10:00:03Z","severity":"ERROR","textPayload":"db timeout after 30s"},
            {"insertId":"2","timestamp":"2026-11-05T10:00:02Z","severity":"ERROR","textPayload":"db timeout after 45s"},
            {"insertId":"3","timestamp":"2026-11-05T10:00:01Z","severity":"ERROR","textPayload":"disk\u001b[2J full"}
        ],"nextPageToken":"more"}"#;
        let fake = MockServer::start(vec![Reply::json(200, page), Reply::json(200, page)]);
        let gcp = client(&fake);
        let query = Query { severity: Some(ERRORS), ..Query::app("shop", 3600.0) };
        let mut out = Vec::new();
        summary(&gcp, "prod", &query, 3, false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("COUNT  LAST SEEN"), "{text}");
        assert!(lines[1].starts_with("    2  ") && lines[1].ends_with("db timeout after #s"), "{text}");
        assert!(lines[2].starts_with("    1  ") && lines[2].ends_with("disk full"), "{text}");
        assert!(filter_sent(&fake, 0).contains("severity>=ERROR"));
        let mut out = Vec::new();
        summary(&gcp, "prod", &query, 3, true, &mut out).unwrap();
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!((v["read"].as_u64(), v["more"].as_bool()), (Some(3), Some(true)));
        let first = &v["groups"][0];
        assert_eq!((first["count"].as_u64(), first["pattern"].as_str()), (Some(2), Some("db timeout after #s")));
        assert_eq!(first["lastSeen"], "2026-11-05T10:00:03Z");
        assert_eq!(first["example"], "db timeout after 30s");
    }

    #[test]
    fn an_empty_summary_says_so() {
        let fake = MockServer::start(vec![Reply::json(200, "{}")]);
        let mut out = Vec::new();
        let query = Query { severity: Some(ERRORS), ..Query::app("shop", 3600.0) };
        summary(&client(&fake), "prod", &query, 10, false, &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().starts_with("No entries for shop, last 1h, ERROR or worse in prod."));
    }
}
