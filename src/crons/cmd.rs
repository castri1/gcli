//! The Crons commands: `gcli crons` (the list, `--check` as a check on every job) and
//! `gcli cron <job>` (its details and its recent attempts, `--report` for an AI agent).

use std::process::ExitCode;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::history::WINDOW_TEXT;
use super::{Detail, Listing, Row, attempt_result, cells, columns, report, schedule_text};
use crate::apps::cmd::{print_table_wide, tone, warn};
use crate::apps::{Column, Tone};
use crate::builds::cmd::{named_env, target};
use crate::cli::{gcp_for, paint, print_json};
use crate::config::{AppsConvention, Config};
use crate::findings::{self, COULD_NOT_TELL, Finding, Level};
use crate::gcp::error::redact_tokens;
use crate::gcp::{Gcp, GcpError};
use crate::util::{fmt_duration, fmt_utc, now_secs, program};

/// A job's name is its handle: `gcli cron` needs it whole.
const WIDEST: usize = 72;
/// Attempts `gcli cron` prints at most; `--json` has them all.
const ATTEMPTS_SHOWN: usize = 30;

#[derive(clap::Args)]
pub struct Args {
    /// An app (its Cloud Run service). A single name that is an environment means the environment
    #[arg(value_name = "APP")]
    app: Option<String>,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Only the jobs failing: last attempt failed, overdue or orphan
    #[arg(long)]
    failed: bool,
    /// Exit 1 when a job is failing, 2 when it could not read (for cron and CI)
    #[arg(long, conflicts_with = "failed")]
    check: bool,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct OneArgs {
    /// The job's name, as `crons` shows it
    job: String,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// A Markdown report for an AI agent
    #[arg(long, conflicts_with = "json")]
    report: bool,
    #[arg(long)]
    json: bool,
}

fn region_text(gcp: &Gcp) -> &str {
    if gcp.region.is_empty() { "every region" } else { &gcp.region }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Says what could not be read: the list goes on without it.
fn warn_unread(listing: &Listing) {
    if let Some(e) = &listing.services_error {
        warn(&format!("cannot tell which app each job belongs to, or which are orphans: {}", e.short()));
    }
    if let Some(e) = &listing.history_error {
        warn(&format!("cannot tell how long jobs have been failing, or the HTTP status: {}", e.short()));
    }
}

/// `gcli crons [APP] [ENV] [--failed] [--json]`, or `gcli crons [ENV] --check`.
pub fn list(config: &Config, args: Args) -> Result<ExitCode> {
    if args.check {
        // Any failure to read means "could not tell", never the 1 that means a job is failing.
        return check(config, args.app, args.env, args.json).or_else(|e| {
            eprintln!("error: {}", crate::util::sanitize(&format!("{e:#}")));
            if e.downcast_ref::<GcpError>().is_some_and(GcpError::is_login_required) {
                eprintln!("hint: run `{} login` (or press L in the UI)", program());
            }
            Ok(ExitCode::from(COULD_NOT_TELL))
        });
    }
    let (app, env_name) = target(config, args.app, args.env)?;
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    let now = now_secs();
    let listing = Listing::load(&gcp, env.apps, now)?;
    warn_unread(&listing);
    let of_app = |r: &&Row| app.as_deref().is_none_or(|a| r.app() == Some(a));
    let rows: Vec<&Row> =
        listing.rows.iter().filter(of_app).filter(|r| !args.failed || r.level(now) == Some(Level::Bad)).collect();
    if args.json {
        let mut out = json!({
            "env": env_name,
            "project": env.project,
            "region": region_text(&gcp),
            "app": app,
            "window": WINDOW_TEXT,
            "shown": rows.len(),
            "of": listing.rows.len(),
            "failing": listing.failing(now),
            "jobs": rows.iter().map(|r| r.json(now)).collect::<Vec<_>>(),
        });
        listing.add_errors(&mut out);
        print_json(&out);
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("{}", nothing_found(app.as_deref(), args.failed, env.apps));
    } else {
        print_table_wide(&columns(), rows.iter().map(|r| cells(r, now)).collect(), WIDEST);
    }
    let failing = rows.iter().filter(|r| r.level(now) == Some(Level::Bad)).count();
    let mut footer = vec![format!("{} job{}", rows.len(), plural(rows.len()))];
    if let Some(a) = &app {
        footer.push(format!("of {a}"));
    }
    if args.failed {
        footer.push("failing only".into());
    } else {
        footer.push(format!("{failing} failing"));
    }
    footer.push(format!("{env_name} · {} · {}", env.project, region_text(&gcp)));
    if listing.history_read && !listing.in_question().is_empty() {
        footer.push(format!("attempts read: {WINDOW_TEXT} (+ is at least)"));
    }
    println!("\n{}", paint(&footer.join(" · "), "90"));
    if failing > 0 {
        println!(
            "{}",
            paint(
                &format!(
                    "Why, and since when: {} crons {env_name} --check, {} cron JOB {env_name}",
                    program(),
                    program()
                ),
                "90"
            )
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn nothing_found(app: Option<&str>, failed: bool, convention: AppsConvention) -> String {
    let what = if failed { "No jobs failing" } else { "No jobs" };
    let how = match convention {
        AppsConvention::Run => "its URL is the service's own",
        AppsConvention::Whiteghost => "its URL is the service's own, or its name starts with the app's",
    };
    match app {
        Some(a) => format!("{what} of {a}. A job belongs to an app when {how}."),
        None => format!("{what}."),
    }
}

fn check(config: &Config, first: Option<String>, env: Option<String>, json: bool) -> Result<ExitCode> {
    let env_name = match (first, env) {
        (Some(_), Some(_)) => bail!("--check reads every job: {} crons [ENV] --check", program()),
        (first, _) => named_env(config, first)?,
    };
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    let now = now_secs();
    let listing = Listing::load(&gcp, env.apps, now)?;
    let found = listing.check(now)?;
    let code = findings::exit_code(&found);
    if json {
        let mut out = listing.failures_json(now);
        out["env"] = json!(env_name);
        out["project"] = json!(env.project);
        out["region"] = json!(region_text(&gcp));
        print_json(&out);
        return Ok(code);
    }
    warn_unread(&listing);
    println!(
        "{}  {}",
        paint(&env_name, "1"),
        paint(
            &format!(
                "{} · {} · {} job{}",
                env.project,
                region_text(&gcp),
                listing.rows.len(),
                plural(listing.rows.len())
            ),
            "90"
        )
    );
    print_findings(&found);
    let failing = listing.failing(now);
    if failing == 0 {
        println!("{} No job is failing.", paint("✓", "32"));
    } else {
        println!(
            "\n{}",
            paint(
                &format!(
                    "Runs of failures are counted over the {WINDOW_TEXT}. One job's attempts and last success: {p} cron JOB {env_name} · What an app wrote: {p} logs APP {env_name}",
                    p = program()
                ),
                "90"
            )
        );
    }
    Ok(code)
}

fn print_findings(found: &[Finding]) {
    for (level, title, color) in [(Level::Bad, "FAILING", "31;1"), (Level::Warn, "WORTH A LOOK", "33;1")] {
        let of_level: Vec<&Finding> = found.iter().filter(|f| f.level == level).collect();
        if of_level.is_empty() {
            continue;
        }
        println!("\n{}", paint(&format!("{title} · {}", of_level.len()), color));
        for f in of_level {
            let t = if level == Level::Bad { Tone::Bad } else { Tone::Warn };
            println!("  {}", tone(&f.subject, t));
            println!("      {}", f.what);
            if let Some(hint) = &f.hint {
                println!("      {}", paint(hint, "90"));
            }
        }
    }
}

/// `gcli cron <job> [env] [--report] [--json]`.
pub fn one(config: &Config, args: OneArgs) -> Result<ExitCode> {
    let env_name = named_env(config, args.env)?;
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    let now = now_secs();
    let detail = Detail::load(&gcp, env.apps, &args.job, now)?;
    if args.json {
        let mut v: Value = detail.json(now);
        v["env"] = json!(env_name);
        print_json(&v);
    } else if args.report {
        print!("{}", report::markdown(&env_name, &env.project, &detail, now));
    } else {
        print_detail(&detail, &env_name, now);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_detail(d: &Detail, env_name: &str, now: f64) {
    let row = &d.row;
    let job = &row.job;
    let kv = |k: &str, v: String| println!("  {}  {v}", paint(&format!("{k:<9}"), "90"));
    let (result, t) = super::result(job);
    println!(
        "{}  {}  {}",
        paint(job.id(), "1"),
        tone(&result, t),
        paint(&format!("{env_name} · {}", job.location()), "90")
    );
    if !job.description.is_empty() {
        kv("About", redact_tokens(&job.description));
    }
    match (row.app(), row.live_app()) {
        (Some(app), Some(_)) => kv("App", app.to_string()),
        (Some(app), None) => kv("App", paint(&format!("{app} (no Cloud Run service by that name)"), "31")),
        _ => kv("App", paint("none known", "90")),
    }
    if let Some(url) = job.url() {
        kv(
            "Target",
            format!(
                "{} {}  {}",
                job.target_kind(),
                redact_tokens(&url),
                paint(&format!("token: {}", job.auth_kind().unwrap_or("none")), "90")
            ),
        );
    } else if !job.target_kind().is_empty() {
        kv("Target", job.target_kind());
    }
    kv("Schedule", schedule_text(job));
    let when = |t: f64| format!("{} · {}", fmt_utc(t), super::relative(t, now));
    kv("Last run", job.last_attempt().map_or("never".to_string(), when));
    kv("Next run", job.next_run().map_or("—".to_string(), when));
    let timeout = row.timeout.map(|t| format!(" · the service's request timeout {t:.0}s")).unwrap_or_default();
    kv("Deadline", format!("{}{timeout}", job.attempt_deadline));
    if let Some(retry) = job.retry_config.as_ref().filter(|r| r.retry_count > 0) {
        kv("Retries", retry.retry_count.to_string());
    }
    if let Some(t) = job.updated() {
        kv("Changed", when(t));
    }
    if !job.status.message.is_empty() {
        kv("Status", redact_tokens(&job.status.message));
    }
    if let Some(code) = job.status.code.filter(|c| *c < 0) {
        kv("Status", paint(&format!("code {code}: {}", super::NEGATIVE_CODE_EXPLAINED), "90"));
    }
    if let Some(last) = d.last_success {
        kv("Last OK", last.text(now));
    }
    if let Some(e) = &d.services_error {
        println!("{}", paint(&format!("! cannot tell its app: {}", e.short()), "33"));
    }
    let found = row.findings(now);
    if !found.is_empty() {
        print_findings(&found);
    }
    println!("\n{}", paint(&format!("ATTEMPTS · {WINDOW_TEXT}"), "36;1"));
    match &d.attempts {
        Err(e) => println!("  {}", paint(&format!("! {}", e.short()), "33")),
        Ok(read) if read.list.is_empty() => println!("  {}", paint("None in the logs.", "90")),
        Ok(read) => {
            print_attempts(read, now);
            if let Some(app) = row.live_app().filter(|_| !found.is_empty()) {
                println!("\n{}", paint(&format!("What the app wrote: {} logs {app} {env_name}", program()), "90"));
            }
        }
    }
}

fn print_attempts(read: &super::history::Attempts, now: f64) {
    let col = |head, right| Column { head, right, drop: 0 };
    let cols = [
        col("WHEN", false),
        col("AGO", true),
        col("RESULT", false),
        col("HTTP", true),
        col("TOOK", true),
        col("DETAIL", false),
    ];
    let table = read
        .list
        .iter()
        .take(ATTEMPTS_SHOWN)
        .map(|a| {
            let result_tone = if a.running {
                Tone::Warn
            } else if a.failed {
                Tone::Bad
            } else {
                Tone::Plain
            };
            vec![
                (fmt_utc(a.at), Tone::Muted),
                (super::relative(a.at, now), Tone::Muted),
                (attempt_result(a), result_tone),
                (a.http.map_or(String::new(), |h| h.to_string()), result_tone),
                (a.took.map_or(String::new(), fmt_duration), Tone::Muted),
                (if a.failed { redact_tokens(&a.detail) } else { String::new() }, Tone::Muted),
            ]
        })
        .collect();
    print_table_wide(&cols, table, WIDEST);
    if read.list.len() > ATTEMPTS_SHOWN {
        println!("{}", paint(&format!("… and {} more (--json has them)", read.list.len() - ATTEMPTS_SHOWN), "90"));
    }
    if read.cut {
        println!("{}", paint("… older ones in the window were not read", "90"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_found_says_how_a_job_is_recognized() {
        assert_eq!(nothing_found(None, false, AppsConvention::Run), "No jobs.");
        assert_eq!(nothing_found(None, true, AppsConvention::Run), "No jobs failing.");
        assert!(nothing_found(Some("shop"), false, AppsConvention::Run).contains("its URL is the service's own."));
        assert!(nothing_found(Some("shop"), true, AppsConvention::Whiteghost).starts_with("No jobs failing of shop."));
        assert!(nothing_found(Some("shop"), true, AppsConvention::Whiteghost).contains("name starts with the app's"));
    }
}
