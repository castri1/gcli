//! `gcli status [ENV] [--check] [--notify] [--json]`.

use std::process::ExitCode;

use anyhow::Result;

use super::{Area, News, Status};
use crate::cli::{env_or_current, gcp_for, paint, print_json};
use crate::config::Config;
use crate::findings::Level;
use crate::util::{ellipsize, now_secs};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// Exit 1 when something needs attention, 2 when nothing does but something could not be
    /// read, 0 otherwise (for cron and CI)
    #[arg(long)]
    check: bool,
    /// With --check: raise a desktop notification for what needs attention and was not told
    /// before, and for an area it could not read (for a scheduled job nobody is watching)
    #[arg(long, requires = "check")]
    notify: bool,
    /// Every finding, not only the first of each area
    #[arg(long)]
    all: bool,
    #[arg(long)]
    json: bool,
}

/// Findings of an area shown at most, unless `--all`.
const SHOWN: usize = 5;

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let name = env_or_current(config, args.env.clone())?;
    let env = config.env(&name)?;
    let gcp = gcp_for(config, &name)?;
    let status = super::read(&gcp, &name, env, &config.defaults.costs, Some(config), now_secs());
    if args.notify {
        tell(&name, &status);
    }
    if args.json {
        print_json(&status.json(&name, &env.project));
    } else {
        print_status(&name, &env.project, &status, args.all);
    }
    Ok(if args.check { status.exit_code() } else { ExitCode::SUCCESS })
}

fn tell(env: &str, status: &Status) {
    let seen = super::load_seen(env);
    let news: News = super::news(status, &seen);
    if let Some((title, body)) = super::notification(env, &news, status.login_required) {
        crate::platform::notify_and_wait(&title, &body);
    }
    // Not remembered, the same news comes again tomorrow: annoying, not wrong.
    if let Err(e) = super::store_seen(env, &news.remember) {
        eprintln!("warning: could not remember what was told: {e}");
    }
}

fn print_status(env: &str, project: &str, status: &Status, all: bool) {
    println!("{}  {}", paint(env, "1"), paint(project, "90"));
    let width = status.areas.iter().map(|a| a.name.len()).max().unwrap_or(0);
    for area in &status.areas {
        print_area(env, area, width, all);
    }
    let (bad, warn, unread) = (status.count(Level::Bad), status.count(Level::Warn), status.unread());
    println!();
    if bad == 0 && warn == 0 && unread == 0 {
        println!("{} Nothing needs attention.", paint("✓", "32"));
    } else {
        println!(
            "{} need attention · {} to look at{}",
            paint(&bad.to_string(), if bad > 0 { "31;1" } else { "1" }),
            warn,
            if unread > 0 { format!(" · {unread} could not be read") } else { String::new() }
        );
    }
    if status.login_required {
        eprintln!("hint: run `{} login` (or press L in the UI)", crate::util::program());
    }
}

fn print_area(env: &str, area: &Area, width: usize, all: bool) {
    let (bad, warn) = (area.count(Level::Bad), area.count(Level::Warn));
    let (mark, color) = if bad > 0 {
        ("✗", "31")
    } else if !area.unread.is_empty() || warn > 0 {
        ("!", "33")
    } else if area.skipped.is_some() {
        ("·", "90")
    } else {
        ("✓", "32")
    };
    let summary = match (&area.skipped, bad, warn) {
        (Some(why), _, _) => why.clone(),
        (_, 0, 0) if area.unread.is_empty() => "nothing to look at".to_string(),
        (_, 0, 0) => String::new(),
        (_, b, 0) => format!("{b} need attention"),
        (_, 0, w) => format!("{w} to look at"),
        (_, b, w) => format!("{b} need attention, {w} to look at"),
    };
    println!("{} {:<width$}  {}", paint(mark, color), paint(area.name, "1"), summary);
    let names = area.found.iter().map(|f| f.subject.chars().count()).max().unwrap_or(0).min(36);
    let shown = if all { area.found.len() } else { SHOWN };
    for f in area.found.iter().take(shown) {
        let (m, c) = if f.level == Level::Bad { ("✗", "31") } else { ("!", "33") };
        println!("    {} {}  {}", paint(m, c), paint(&format!("{:<names$}", ellipsize(&f.subject, names)), c), f.what);
    }
    if area.found.len() > shown {
        println!("    {}", paint(&format!("and {} more: {}", area.found.len() - shown, area.command(env)), "90"));
    }
    for line in &area.unread {
        println!("    {} {}", paint("!", "33"), paint(&format!("could not read: {}", ellipsize(line, 160)), "33"));
    }
}
