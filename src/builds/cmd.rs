//! The Builds commands: `gcli builds` (the list, `--check` as a check on the last deploys) and
//! `gcli build <id>` (steps, why it failed, `--log`, `--report` for an AI agent).

use std::process::ExitCode;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{
    Deploys, Detail, Failing, Failure, LAST_DEPLOY_EXPLAINED, Listing, bar, cells, columns, report, step_state,
};
use crate::apps::Tone;
use crate::apps::cmd::{print_table, tone, warn};
use crate::cli::{current_env, gcp_for, paint, print_json};
use crate::config::{AppsConvention, Config};
use crate::gcp::build::{self, Build};
use crate::gcp::{Gcp, GcpError};
use crate::util::{ellipsize, fmt_ago, fmt_duration, fmt_utc, now_secs, program};

/// How many builds a list shows unless asked.
const LIMIT: u32 = 30;
/// `--check`'s exit code when it cannot tell: distinct from 1 (a failed deploy), so a cron job
/// does not read an outage as a broken app. The same as `gcli costs --check`.
const CANNOT_TELL: u8 = 2;
/// Columns of a step's bar in `gcli build`.
const BAR: usize = 30;

#[derive(clap::Args)]
pub struct Args {
    /// An app (its Cloud Run service). A single name that is an environment means the environment
    #[arg(value_name = "APP")]
    app: Option<String>,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Only the builds that failed
    #[arg(long)]
    failed: bool,
    /// How many builds, newest first
    #[arg(long, value_name = "N", default_value_t = LIMIT, value_parser = clap::value_parser!(u32).range(1..=1000))]
    limit: u32,
    /// Exit 1 when some app's latest build failed and no later build succeeded, 2 when it could
    /// not read (for cron and CI)
    #[arg(long, conflicts_with_all = ["failed", "limit"])]
    check: bool,
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
pub struct OneArgs {
    /// The build's id, or the start of it as `builds` shows it
    id: String,
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// The whole log, oldest first
    #[arg(long)]
    log: bool,
    /// A Markdown report for an AI agent
    #[arg(long, conflicts_with_all = ["json", "log"])]
    report: bool,
    #[arg(long)]
    json: bool,
}

pub(crate) fn named_env(config: &Config, env: Option<String>) -> Result<String> {
    match env {
        Some(name) => config.env(&name).map(|_| name),
        None => current_env(config),
    }
}

/// `[APP] [ENV]`: which app and environment. A single name is the environment when one has that
/// name: `gcli builds shop prod` tells them apart when an app is named like an environment.
pub(crate) fn target(config: &Config, first: Option<String>, env: Option<String>) -> Result<(Option<String>, String)> {
    match (first, env) {
        (Some(one), None) if config.environments.contains_key(&one) => Ok((None, one)),
        (app, env) => Ok((app, named_env(config, env)?)),
    }
}

fn no_region_note(gcp: &Gcp) {
    if gcp.region.is_empty() {
        warn("this environment has no region: only Cloud Build's global builds are read (set one with `add --region`)");
    }
}

/// `gcli builds [APP] [ENV] [--failed] [--limit N] [--json]`, or `gcli builds [ENV] --check`.
pub fn list(config: &Config, args: Args) -> Result<ExitCode> {
    if args.check {
        // Any failure to read means "could not tell", never the 1 that means a failed deploy.
        return check(config, args.app, args.env, args.json).or_else(|e| {
            eprintln!("error: {}", crate::util::sanitize(&format!("{e:#}")));
            if e.downcast_ref::<GcpError>().is_some_and(GcpError::is_login_required) {
                eprintln!("hint: run `{} login` (or press L in the UI)", program());
            }
            Ok(ExitCode::from(CANNOT_TELL))
        });
    }
    let (app, env_name) = target(config, args.app, args.env)?;
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    no_region_note(&gcp);
    let listing = Listing::load(&gcp, env.apps, app.as_deref(), args.failed, args.limit as usize)?;
    if let Err(e) = &listing.deploys {
        if e.is_login_required() {
            return Err(e.clone().into());
        }
        warn(&format!("cannot tell which apps' last deploy failed: {}", e.short()));
    }
    let deploys = listing.deploys.as_ref().ok();
    let builds = &listing.builds;
    let now = now_secs();
    let rows = listing.rows(env.apps);
    if args.json {
        let failing = deploys.map(|d| d.failing.iter().map(Failing::json).collect::<Vec<_>>());
        print_json(&json!({
            "env": env_name,
            "project": env.project,
            "region": build::location(&gcp),
            "app": app,
            "builds": rows.iter().map(|r| r.json(now)).collect::<Vec<_>>(),
            "lastDeployFailed": failing,
        }));
        return Ok(ExitCode::SUCCESS);
    }
    if rows.is_empty() {
        println!("{}", nothing_found(app.as_deref(), args.failed, env.apps));
    } else {
        let table: Vec<Vec<(String, Tone)>> = rows.iter().map(|r| cells(r, now)).collect();
        print_table(&columns(), table);
    }
    if let Some(d) = deploys {
        print_failing(&d.failing, &env_name, Some(builds));
    }
    let mut footer = vec![format!("{} build{}", rows.len(), if rows.len() == 1 { "" } else { "s" })];
    if let Some(a) = &app {
        footer.push(format!("of {a}"));
    }
    if args.failed {
        footer.push("failed only".into());
    }
    footer.push(format!("{env_name} · {} · {}", env.project, build::location(&gcp)));
    footer.push("newest first".into());
    println!("\n{}", paint(&footer.join(" · "), "90"));
    Ok(ExitCode::SUCCESS)
}

fn nothing_found(app: Option<&str>, failed: bool, convention: AppsConvention) -> String {
    let what = if failed { "No failed builds" } else { "No builds" };
    match app {
        Some(a) => format!(
            "{what} of {a}. A build belongs to an app when its {} substitution names it.",
            convention.build_substitutions().join(" or ")
        ),
        None => format!("{what}."),
    }
}

/// The apps whose last deploy failed, those not already shown by `listed` in full.
fn print_failing(failing: &[Failing], env_name: &str, listed: Option<&[Build]>) {
    if failing.is_empty() {
        return;
    }
    let width = failing.iter().map(|f| f.app.chars().count()).max().unwrap_or(0).min(32);
    println!("\n{}", paint(&format!("LAST DEPLOY FAILED · {}", failing.len()), "31;1"));
    for f in failing {
        let b = &f.build;
        let shown = listed.is_some_and(|l| l.iter().any(|x| x.id == b.id));
        let mut parts = vec![format!("build {}", if shown { b.short_id() } else { &b.id })];
        if !b.short_commit().is_empty() {
            parts.push(format!("commit {}", b.short_commit()));
        }
        if let Some((_, s)) = b.failing_step() {
            parts.push(format!("step {}", s.label()));
        }
        println!("  {}  {}", paint(&format!("{:<width$}", ellipsize(&f.app, width)), "31"), f.text());
        println!("  {:<width$}  {}", "", paint(&parts.join(" · "), "90"));
    }
    println!("{}", paint(&format!("{LAST_DEPLOY_EXPLAINED} Why one failed: {} build ID {env_name}", program()), "90"));
}

fn check(config: &Config, first: Option<String>, env: Option<String>, json: bool) -> Result<ExitCode> {
    let env_name = match (first, env) {
        (Some(_), Some(_)) => bail!("--check reads every app: {} builds [ENV] --check", program()),
        (Some(name), None) => named_env(config, Some(name))?,
        (None, _) => current_env(config)?,
    };
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    no_region_note(&gcp);
    let deploys = super::failing_deploys(&gcp, env.apps)?;
    let code = if deploys.failing.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    if json {
        print_json(&check_json(&env_name, &env.project, &deploys));
        return Ok(code);
    }
    println!(
        "{}  {}",
        paint(&env_name, "1"),
        paint(&format!("{} · {} · each app's latest build", env.project, build::location(&gcp)), "90")
    );
    if deploys.failing.is_empty() {
        println!("{} No app's last deploy failed.", paint("✓", "32"));
    } else {
        print_failing(&deploys.failing, &env_name, None);
    }
    if !deploys.not_services.is_empty() {
        println!(
            "\n{}",
            paint(
                &format!(
                    "Not counted: {} name{} in failed builds with no Cloud Run service (deleted apps).",
                    deploys.not_services.len(),
                    if deploys.not_services.len() == 1 { "" } else { "s" }
                ),
                "90"
            )
        );
    }
    Ok(code)
}

fn check_json(env_name: &str, project: &str, d: &Deploys) -> Value {
    json!({
        "env": env_name,
        "project": project,
        "failing": d.failing.iter().map(Failing::json).collect::<Vec<_>>(),
        "means": LAST_DEPLOY_EXPLAINED,
        "notServices": d.not_services,
    })
}

/// `gcli build <id> [env] [--log | --report] [--json]`.
pub fn one(config: &Config, args: OneArgs) -> Result<ExitCode> {
    let env_name = named_env(config, args.env)?;
    let env = config.env(&env_name)?;
    let gcp = gcp_for(config, &env_name)?;
    let detail = Detail::load(&gcp, env.apps, &args.id, args.log)?;
    let now = now_secs();
    if args.json {
        let mut v = detail.json(now);
        if args.log
            && let Some(Ok(log)) = &detail.log
        {
            v["log"] = json!(log.lines.iter().map(|l| l.shown()).collect::<Vec<_>>());
            v["logCut"] = json!(log.cut);
        }
        print_json(&v);
    } else if args.report {
        print!("{}", report::markdown(&env_name, &env.project, build::location(&gcp), &detail, now));
    } else if args.log {
        print_log(&detail)?;
    } else {
        print_detail(&detail, &env_name, &gcp, now);
    }
    Ok(ExitCode::SUCCESS)
}

/// The whole log on stdout, the notes on stderr, so it can be piped.
fn print_log(detail: &Detail) -> Result<()> {
    match &detail.log {
        Some(Ok(log)) => {
            for line in &log.lines {
                println!("{}", line.shown());
            }
            if log.lines.is_empty() {
                warn(&no_log_lines(&detail.build));
            }
            if log.cut {
                warn(&format!("only the first {} lines were read", log.lines.len()));
            }
            Ok(())
        }
        Some(Err(e)) => Err(e.clone().into()),
        None => Ok(()),
    }
}

fn no_log_lines(b: &Build) -> String {
    format!(
        "Cloud Logging holds no line of this build: past its retention (30 days by default), or it logged elsewhere. Its page: {}",
        b.log_url
    )
}

fn print_detail(d: &Detail, env_name: &str, gcp: &Gcp, now: f64) {
    let b = &d.build;
    let kv = |k: &str, v: String| println!("  {}  {v}", paint(&format!("{k:<8}"), "90"));
    let (m, t) = super::mark(&b.status);
    println!(
        "{}  {}  {}",
        paint(d.app.as_deref().unwrap_or("(no app)"), "1"),
        tone(&format!("{m} {}", b.status.to_lowercase()), t),
        paint(&format!("{env_name} · {} · {}", build::location(gcp), b.id), "90")
    );
    let source: Vec<&str> = [b.commit(), b.branch(), b.repo()].into_iter().filter(|s| !s.is_empty()).collect();
    if !source.is_empty() {
        kv("Source", source.join(" · "));
    }
    if !b.trigger().is_empty() {
        kv("Trigger", b.trigger().to_string());
    }
    let when = b.created().map(|t| format!("{} · {}", fmt_utc(t), fmt_ago(t))).unwrap_or_default();
    let took = b.secs(now).map(|s| format!(" · ran {}", fmt_duration(s))).unwrap_or_default();
    kv("Created", format!("{when}{took}"));
    if !b.status_detail.is_empty() {
        kv("Status", b.status_detail.clone());
    }
    if !b.failure_info.detail.is_empty() {
        kv("Failure", paint(&b.failure_info.detail, "31"));
    }
    match &d.deploy {
        Some(Ok(_)) => match d.failing() {
            Some(f) => kv("The app", paint(&f.text(), "31")),
            None => kv("The app", d.since().unwrap_or_default()),
        },
        Some(Err(e)) => kv("The app", paint(&format!("! {}", e.short()), "33")),
        None => {}
    }

    if !b.steps.is_empty() {
        println!("\n{}", paint("STEPS", "36;1"));
        let width = b.steps.iter().map(|s| s.label().chars().count()).max().unwrap_or(0).min(32);
        for (n, s) in b.steps.iter().enumerate() {
            let (m, text, t) = step_state(s, b, now);
            let bar = bar(s, b, BAR, now)
                .map(|(left, long)| {
                    format!("{}{}{}", "░".repeat(left), "█".repeat(long), "░".repeat(BAR - left - long))
                })
                .unwrap_or_else(|| "░".repeat(BAR));
            println!(
                "  {} {}  {}  {}  {}",
                tone(m, t),
                paint(&format!("{n:>2}"), "90"),
                tone(
                    &format!("{:<width$}", ellipsize(s.label(), width)),
                    if s.failed() { Tone::Bad } else { Tone::Plain }
                ),
                paint(&bar, if s.failed() { "31" } else { "36" }),
                tone(&text, t)
            );
        }
    }

    match &d.log {
        Some(Err(e)) => println!("\n{}", paint(&format!("! log: {}", e.short()), "33")),
        Some(Ok(_)) => {
            if let Some(failure) = d.failure() {
                print_failure(&failure, b);
            }
        }
        None => {}
    }
    if b.failed() {
        println!(
            "\n{}",
            paint(
                &format!(
                    "The whole log: {p} build {id} {env_name} --log · For an AI agent: {p} build {id} {env_name} --report",
                    p = program(),
                    id = b.id
                ),
                "90"
            )
        );
    }
}

fn print_failure(failure: &Failure, b: &Build) {
    let step = failure.step.and_then(|n| b.steps.get(n)).map(|s| format!(" · step {}", s.label())).unwrap_or_default();
    let at = match failure.first_error {
        Some(i) if failure.said => format!(" · first error at line {} of {}", i + 1, failure.lines),
        _ => format!(" · no line of {} says an error", failure.lines),
    };
    println!("\n{}", paint(&format!("WHY IT FAILED{step}{at}"), "36;1"));
    if failure.excerpt.is_empty() {
        println!("  {}", paint(&no_log_lines(b), "33"));
        return;
    }
    let mut previous = None;
    for (i, text) in &failure.excerpt {
        if previous.is_some_and(|p: usize| p + 1 != *i) {
            println!("  {}", paint("…", "90"));
        }
        previous = Some(*i);
        let number = paint(&format!("{:>6}", i + 1), "90");
        if failure.said && failure.first_error == Some(*i) {
            println!("  {number}  {}", paint(text, "31"));
        } else {
            println!("  {number}  {text}");
        }
    }
    if failure.cut {
        println!("  {}", paint(&format!("… only the first {} lines were read", failure.lines), "33"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Environment;

    fn config() -> Config {
        let mut config = Config::default();
        for name in ["prod", "sandbox"] {
            config
                .environments
                .insert(name.into(), Environment { project: format!("acme-{name}"), ..Default::default() });
        }
        config
    }

    #[test]
    fn a_single_name_is_the_environment_when_one_has_it() {
        let config = config();
        let t =
            |first: Option<&str>, env: Option<&str>| target(&config, first.map(String::from), env.map(String::from));
        assert_eq!(t(Some("prod"), None).unwrap(), (None, "prod".to_string()));
        assert_eq!(t(Some("shop"), Some("sandbox")).unwrap(), (Some("shop".to_string()), "sandbox".to_string()));
        // An app named like an environment, told apart by the second name.
        assert_eq!(t(Some("prod"), Some("sandbox")).unwrap(), (Some("prod".to_string()), "sandbox".to_string()));
        assert!(t(Some("shop"), Some("nowhere")).is_err());
    }

    #[test]
    fn nothing_found_says_how_a_build_is_recognized() {
        assert_eq!(nothing_found(None, false, AppsConvention::Run), "No builds.");
        let text = nothing_found(Some("shop"), true, AppsConvention::Whiteghost);
        assert_eq!(
            text,
            "No failed builds of shop. A build belongs to an app when its _SERVICE substitution names it."
        );
        assert!(nothing_found(Some("shop"), false, AppsConvention::Run).contains("_SERVICE_NAME or _SERVICE"));
    }

    #[test]
    fn the_check_says_what_it_found_as_json() {
        let v = check_json("prod", "p", &Deploys { failing: vec![], not_services: vec!["gone".into()] });
        assert_eq!(v["failing"], json!([]));
        assert_eq!(v["notServices"], json!(["gone"]));
        assert_eq!(v["env"], "prod");
    }
}
