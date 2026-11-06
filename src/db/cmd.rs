//! The Database commands: `gcli db [ENV]` (each instance: what it is, how it does, connections per
//! database) and `gcli db [ENV] --check` (what needs attention, for cron and CI). They only read.

use std::process::ExitCode;

use anyhow::Result;
use serde_json::json;

use super::{Health, cells, columns, facts, findings, gauges, quiet_line, read_all, report};
use crate::apps::Tone;
use crate::apps::cmd::{print_table, tone, warn};
use crate::cli::{env_or_current, gcp_for, paint, print_json};
use crate::config::Config;
use crate::findings::{COULD_NOT_TELL, Finding, Level};
use crate::gcp::GcpError;
use crate::util::{fmt_utc, now_secs, program, sanitize};

/// Columns of a sparkline: an hour each.
const SPARK: usize = 24;
/// Width of a line's key.
const KEY: usize = 12;

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Only this instance (default: every instance of the project)
    #[arg(long, value_name = "NAME")]
    instance: Option<String>,
    /// Exit 1 when something needs attention, 2 when it could not read (for cron and CI)
    #[arg(long)]
    check: bool,
    #[arg(long)]
    json: bool,
}

/// `gcli db [ENV] [--instance NAME] [--check] [--json]`.
pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    if args.check {
        // Any failure to read means "could not tell", never the 1 that means a finding.
        return check(config, &args).or_else(|e| {
            eprintln!("error: {}", sanitize(&format!("{e:#}")));
            if e.downcast_ref::<GcpError>().is_some_and(GcpError::is_login_required) {
                eprintln!("hint: run `{} login` (or press L in the UI)", program());
            }
            Ok(ExitCode::from(COULD_NOT_TELL))
        });
    }
    let (env_name, all) = load(config, &args)?;
    let project = &config.env(&env_name)?.project;
    if args.json {
        print_json(&report(&env_name, project, &all));
        return Ok(ExitCode::SUCCESS);
    }
    if all.is_empty() {
        println!("No Cloud SQL instances in {project}.");
        return Ok(ExitCode::SUCCESS);
    }
    for (n, h) in all.iter().enumerate() {
        if n > 0 {
            println!();
        }
        print_instance(h, &env_name, project);
    }
    Ok(ExitCode::SUCCESS)
}

fn load(config: &Config, args: &Args) -> Result<(String, Vec<Health>)> {
    let env_name = env_or_current(config, args.env.clone())?;
    let gcp = gcp_for(config, &env_name)?;
    let all = read_all(&gcp, args.instance.as_deref(), now_secs())?;
    // Credentials that expire half way mean a sign-in, not an instance that cannot be read.
    if let Some(e) = all.iter().find_map(Health::login_error) {
        return Err(e.clone().into());
    }
    Ok((env_name, all))
}

/// 1 when something needs attention; otherwise 2 when something could not be read, as all is
/// not known to be well; otherwise 0.
fn exit_code(found: &[Finding], unread: &[String]) -> ExitCode {
    if found.iter().any(|f| f.level == Level::Bad) || unread.is_empty() {
        crate::findings::exit_code(found)
    } else {
        ExitCode::from(COULD_NOT_TELL)
    }
}

fn check(config: &Config, args: &Args) -> Result<ExitCode> {
    let (env_name, all) = load(config, args)?;
    let project = &config.env(&env_name)?.project;
    let found = findings(&all);
    let unread: Vec<String> = all.iter().flat_map(Health::unread).collect();
    let code = exit_code(&found, &unread);
    if args.json {
        print_json(&json!({
            "env": env_name,
            "project": project,
            "instances": all.iter().map(|h| h.instance.name.as_str()).collect::<Vec<_>>(),
            "findings": found,
            "unread": unread,
            "notes": all.iter().flat_map(Health::notes).collect::<Vec<_>>(),
        }));
        return Ok(code);
    }
    let names = match all.len() {
        1 => "1 instance".to_string(),
        n => format!("{n} instances"),
    };
    println!("{}  {}", paint(&env_name, "1"), paint(&format!("{project} · {names}"), "90"));
    print_findings(&found);
    if found.is_empty() && unread.is_empty() {
        let text = if all.is_empty() { "No Cloud SQL instances." } else { "Nothing needs attention." };
        println!("{} {text}", paint("✓", "32"));
    }
    for line in &unread {
        warn(&format!("could not read: {line}"));
    }
    for line in all.iter().flat_map(Health::notes) {
        println!("{}", paint(&format!("Not judged: {line}"), "90"));
    }
    Ok(code)
}

fn print_findings(found: &[Finding]) {
    let width = found.iter().map(|f| f.subject.chars().count()).max().unwrap_or(0);
    for f in found {
        let (mark, t) = match f.level {
            Level::Bad => ("✗", Tone::Bad),
            Level::Warn => ("!", Tone::Warn),
        };
        println!("{} {}  {}", tone(mark, t), tone(&format!("{:<width$}", f.subject), t), f.what);
        if let Some(hint) = &f.hint {
            println!("  {:<width$}  {}", "", paint(hint, "90"));
        }
    }
}

fn print_instance(h: &Health, env_name: &str, project: &str) {
    let i = &h.instance;
    let state = if i.stopped() { "stopped" } else { i.state.as_str() };
    println!(
        "{}  {}  {}",
        paint(&i.name, "1"),
        tone(state, if i.is_runnable() && !i.stopped() { Tone::Plain } else { Tone::Bad }),
        paint(&format!("{env_name} · {project}"), "90")
    );
    let kv = |k: &str, v: String| println!("  {}  {v}", paint(&format!("{k:<KEY$}"), "90"));
    for fact in facts(i) {
        kv(fact.key, tone(&fact.text, fact.tone));
    }

    let found = h.findings();
    if !found.is_empty() {
        println!("\n{}", paint("NEEDS ATTENTION", "36;1"));
        let mut sorted = found;
        crate::findings::sort(&mut sorted);
        print_findings(&sorted);
    }

    let lines = gauges(h, SPARK);
    if !lines.is_empty() {
        println!("\n{}", paint(&format!("HEALTH · last 24 h · read {}", fmt_utc(h.read_at)), "36;1"));
    }
    for g in lines {
        let spark = g.spark.map(|s| format!("   {}", paint(&s, "36"))).unwrap_or_default();
        kv(g.key, format!("{}{spark}", tone(&g.text, g.tone)));
    }
    let unread = h.unread();
    if !unread.is_empty() {
        println!();
    }
    for line in unread {
        println!("{}", paint(&format!("! could not read: {line}"), "33"));
    }

    if let Some(Ok(m)) = &h.metrics
        && !m.databases.is_empty()
    {
        let at = m.states_at.map(|t| format!(" · {}", fmt_utc(t))).unwrap_or_default();
        println!("\n{}", paint(&format!("CONNECTIONS BY DATABASE{at}"), "36;1"));
        let (busy, quiet) = m.busy_and_quiet();
        print_table(&columns(), busy.iter().map(|d| cells(d, m.states_at.is_some())).collect());
        if let Some(line) = quiet_line(quiet) {
            println!("{}", paint(&line, "90"));
        }
        println!("{}", paint(super::DATABASES_EXPLAINED, "90"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exit_code_tells_a_finding_from_not_knowing() {
        let unread = ["shared-db: metrics: denied".to_string()];
        assert_eq!(exit_code(&[], &[]), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[Finding::warn("a", "x")], &[]), ExitCode::SUCCESS);
        assert_eq!(exit_code(&[Finding::bad("a", "x")], &[]), ExitCode::FAILURE);
        assert_eq!(exit_code(&[], &unread), ExitCode::from(COULD_NOT_TELL));
        assert_eq!(exit_code(&[Finding::warn("a", "x")], &unread), ExitCode::from(COULD_NOT_TELL));
        assert_eq!(
            exit_code(&[Finding::bad("a", "x")], &unread),
            ExitCode::FAILURE,
            "bad is known, whatever else is not"
        );
    }
}
