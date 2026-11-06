//! The command `gcli platform`: the load balancer's limits, leftover pieces, which apps are
//! exposed, certificates and images. `--check` makes it a check for cron and CI.

use std::process::ExitCode;

use anyhow::Result;
use serde_json::json;

use super::{Section, Snapshot, Table, Unread, Want, check_exit, findings, wanted};
use crate::apps::Tone;
use crate::apps::cmd::{print_table, warn};
use crate::cli::{current_env, gcp_for, paint, print_json};
use crate::config::Config;
use crate::findings::{Finding, Level};
use crate::gcp::GcpError;
use crate::util::{now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Exit 1 when something needs attention, 2 when it could not read (for cron and CI)
    #[arg(long, conflicts_with_all = ["limits", "orphans", "exposure", "certificates", "images"])]
    check: bool,
    /// Only what is counted against a limit
    #[arg(long)]
    limits: bool,
    /// Only the pieces nothing leads to, or that lead nowhere
    #[arg(long)]
    orphans: bool,
    /// Every service: ingress, load balancer, IAP, who may invoke it, reachable directly or not
    #[arg(long)]
    exposure: bool,
    /// Only certificates: status and expiry
    #[arg(long, visible_alias = "certs")]
    certificates: bool,
    /// Only image repositories: size and cleanup policy
    #[arg(long)]
    images: bool,
    #[arg(long)]
    json: bool,
}

impl Args {
    /// What the selectors ask for: everything, unless one is given.
    fn want(&self) -> Want {
        let picked = self.limits || self.orphans || self.exposure || self.certificates || self.images;
        if !picked {
            return Want::ALL;
        }
        Want {
            limits: self.limits,
            orphans: self.orphans,
            exposure: self.exposure,
            certificates: self.certificates,
            images: self.images,
        }
    }
}

/// `gcli platform [ENV] [--limits] [--orphans] [--exposure] [--certificates] [--images] [--json]`,
/// or `gcli platform [ENV] --check [--json]`.
pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    if args.check {
        // Any failure to read means "could not tell", never the 1 that means a finding.
        return check(config, &args).or_else(|e| {
            eprintln!("error: {}", crate::util::sanitize(&format!("{e:#}")));
            if e.downcast_ref::<GcpError>().is_some_and(GcpError::is_login_required) {
                eprintln!("hint: run `{} login` (or press L in the UI)", program());
            }
            Ok(ExitCode::from(crate::findings::COULD_NOT_TELL))
        });
    }
    show(config, &args)
}

fn env_name(config: &Config, args: &Args) -> Result<String> {
    match &args.env {
        Some(name) => config.env(name).map(|_| name.clone()),
        None => current_env(config),
    }
}

fn say_unread(unread: &[Unread]) {
    for u in unread {
        warn(&format!("could not read {}: {}", u.what, u.error.short()));
    }
    if unread.iter().any(|u| u.error.is_login_required()) {
        eprintln!("hint: run `{} login` (or press L in the UI)", program());
    }
}

fn check(config: &Config, args: &Args) -> Result<ExitCode> {
    let name = env_name(config, args)?;
    let env = config.env(&name)?;
    let gcp = gcp_for(config, &name)?;
    let now = now_secs();
    let snapshot = Snapshot::load(&gcp, &env.platform.exposed, Want::ALL);
    let found = findings(&snapshot, now);
    let unread = snapshot.unread();
    let code = check_exit(&found, &unread);
    if args.json {
        print_json(&json!({
            "env": name,
            "project": env.project,
            "findings": found,
            "unread": unread.iter().map(super::unread_json).collect::<Vec<_>>(),
        }));
        return Ok(code);
    }
    println!("{}  {}", paint(&name, "1"), paint(&format!("{} · what the apps share", env.project), "90"));
    say_unread(&unread);
    print_findings(&found);
    Ok(code)
}

fn print_findings(found: &[Finding]) {
    if found.is_empty() {
        println!("{} Nothing needs a look.", paint("✓", "32"));
        return;
    }
    for f in found {
        let (mark, color) = if f.level == Level::Bad { ("✗", "31") } else { ("!", "33") };
        println!("{} {}  {}", paint(mark, color), paint(&f.subject, color), f.what);
        if let Some(hint) = &f.hint {
            println!("  {}", paint(hint, "90"));
        }
    }
}

fn print_section(section: Section, table: Table, all: bool) {
    let total = table.rows.len();
    let shown: Vec<Vec<(String, Tone)>> =
        table.rows.into_iter().filter(|r| all || r.overview).map(|r| r.cells).collect();
    println!("\n{}", paint(&format!("{} · {total}", section.title().to_uppercase()), "1"));
    if shown.is_empty() && total == 0 {
        println!("{}", paint(nothing_in(section), "90"));
        return;
    }
    if !shown.is_empty() {
        print_table(&table.cols, shown.clone());
    }
    if shown.len() < total {
        let rest = total - shown.len();
        println!(
            "{}",
            paint(
                &format!(
                    "The other {rest}: the internet cannot get in past their ingress. All of them: {} platform --exposure",
                    program()
                ),
                "90"
            )
        );
    }
}

fn nothing_in(section: Section) -> &'static str {
    match section {
        Section::Limits => "Nothing read.",
        Section::Orphans => "None: every backend service, NEG and host rule leads somewhere.",
        Section::Exposure => "No Cloud Run services.",
        Section::Certificates => "No certificates.",
        Section::Images => "No repositories.",
    }
}

/// The sections asked for: reads what they need, says what it could not.
fn show(config: &Config, args: &Args) -> Result<ExitCode> {
    let name = env_name(config, args)?;
    let env = config.env(&name)?;
    let gcp = gcp_for(config, &name)?;
    let now = now_secs();
    let want = args.want();
    let snapshot = Snapshot::load(&gcp, &env.platform.exposed, want);
    let sections = wanted(want);
    let unread = snapshot.unread();
    // Nothing to show at all is a failure; otherwise what failed is said, and the rest shown.
    let nothing = sections.iter().all(|s| snapshot.failure(*s).is_some() && snapshot.table(*s, now).rows.is_empty());
    if let Some(u) = unread.first().filter(|_| nothing) {
        return Err(u.error.clone().into());
    }
    if args.json {
        print_json(&snapshot.json(&name, want, now));
        return Ok(ExitCode::SUCCESS);
    }
    println!("{}  {}", paint(&name, "1"), paint(&env.summary(), "90"));
    say_unread(&unread);
    let everything = want != Want::ALL;
    for section in &sections {
        print_section(*section, snapshot.table(*section, now), everything);
    }
    for note in snapshot.notes() {
        println!("\n{}", paint(&note, "90"));
    }
    let mut found: Vec<Finding> = sections.iter().flat_map(|s| super::section_findings(&snapshot, *s, now)).collect();
    crate::findings::sort(&mut found);
    println!("\n{}", paint("FINDINGS", "1"));
    print_findings(&found);
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(line: &[&str]) -> std::result::Result<Args, clap::Error> {
        Cli::try_parse_from(std::iter::once("platform").chain(line.iter().copied())).map(|c| c.args)
    }

    #[test]
    fn selectors_narrow_what_is_read() {
        assert_eq!(parse(&[]).unwrap().want(), Want::ALL);
        let args = parse(&["prod", "--orphans", "--json"]).unwrap();
        assert_eq!(args.env.as_deref(), Some("prod"));
        assert!(args.json);
        let want = args.want();
        assert!(want.orphans && !want.limits && !want.exposure && !want.images && !want.certificates);
        assert!(parse(&["--certs"]).unwrap().want().certificates);
        assert!(parse(&["--check", "--orphans"]).is_err(), "a check reads everything");
    }

    #[test]
    fn nothing_found_says_what_is_clean() {
        assert!(nothing_in(Section::Orphans).contains("leads somewhere"));
        assert_eq!(nothing_in(Section::Images), "No repositories.");
    }
}
