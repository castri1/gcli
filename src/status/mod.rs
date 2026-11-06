//! One look at an environment: what every area's check finds, read at the same time. This is the
//! control tower's first screen and its daily check. A finding belongs to one area and is judged
//! there; this only gathers them, and remembers which ones it has already told about, so a daily
//! notification means something new.

pub mod cmd;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::{Value, json};

use crate::apps::Row;
use crate::config::{Config, CostDefaults, Environment};
use crate::findings::{COULD_NOT_TELL, Finding, Level, sort};
use crate::gcp::{Gcp, GcpError};
use crate::infra::{Snapshot, Want};

/// An app with this share of 5xx answers over its last day, and at least [`ERRORS_MIN_REQUESTS`]
/// requests, in percent: worth a look, and needs attention.
pub const ERRORS_WARN: f64 = 5.0;
pub const ERRORS_BAD: f64 = 25.0;
pub const ERRORS_MIN_REQUESTS: f64 = 50.0;

/// What one area found.
#[derive(Debug, Clone, Default)]
pub struct Area {
    /// As the module is titled.
    pub name: &'static str,
    pub found: Vec<Finding>,
    /// What could not be read: the area cannot vouch for it.
    pub unread: Vec<String>,
    /// Not set up for this environment (no billing export, say): it says nothing.
    pub skipped: Option<String>,
}

impl Area {
    fn new(name: &'static str) -> Area {
        Area { name, ..Area::default() }
    }

    fn of(name: &'static str, read: Result<Vec<Finding>, String>) -> Area {
        let mut area = Area::new(name);
        match read {
            Ok(mut found) => {
                sort(&mut found);
                area.found = found;
            }
            Err(e) => area.unread.push(e),
        }
        area
    }

    pub fn count(&self, level: Level) -> usize {
        self.found.iter().filter(|f| f.level == level).count()
    }

    /// The command that says more, for environment `env`.
    pub fn command(&self, env: &str) -> String {
        let program = crate::util::program();
        match self.name {
            "Apps" => format!("{program} apps {env} --held"),
            "Costs" => format!("{program} costs {env} --check"),
            "Builds" => format!("{program} builds {env} --check"),
            "Crons" => format!("{program} crons {env} --check"),
            "Database" => format!("{program} db {env} --check"),
            "Platform" => format!("{program} platform {env} --check"),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Status {
    pub areas: Vec<Area>,
    pub read_at: f64,
    /// Google asked to sign in again.
    pub login_required: bool,
}

/// `read_costs`: the config, to read the billing export again when what was kept is old. Without
/// it only what was kept counts: BigQuery bills every read, so the UI and the MCP server never
/// start one on their own.
pub fn read(
    gcp: &Gcp,
    env_name: &str,
    env: &Environment,
    costs: &CostDefaults,
    read_costs: Option<&Config>,
    now: f64,
) -> Status {
    let login = std::sync::atomic::AtomicBool::new(false);
    let text = |e: GcpError| {
        if e.is_login_required() {
            login.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        crate::util::sanitize(&e.to_string())
    };
    let areas = std::thread::scope(|s| {
        let apps = s.spawn(|| Area::of("Apps", apps_findings(gcp, env_name, now).map_err(text)));
        let builds = s.spawn(|| {
            let read = crate::builds::failing_deploys(gcp, env.apps);
            Area::of("Builds", read.map(|d| d.failing.iter().map(build_finding).collect()).map_err(text))
        });
        let crons = s.spawn(|| {
            let read = crate::crons::Listing::load(gcp, env.apps, now).and_then(|l| l.check(now));
            Area::of("Crons", read.map_err(text))
        });
        let database = s.spawn(|| {
            let mut area = Area::new("Database");
            match crate::db::read_all(gcp, None, now) {
                Ok(all) => {
                    area.found = crate::db::findings(&all);
                    sort(&mut area.found);
                    area.unread = all.iter().flat_map(crate::db::Health::unread).collect();
                }
                Err(e) => area.unread.push(text(e)),
            }
            area
        });
        let platform = s.spawn(|| {
            let snapshot = Snapshot::load(gcp, &env.platform.exposed, Want::ALL);
            let mut area = Area::new("Platform");
            area.found = crate::infra::findings(&snapshot, now);
            sort(&mut area.found);
            area.unread = snapshot
                .unread()
                .into_iter()
                .map(|u| {
                    let error = text(u.error);
                    format!("{}: {error}", u.what)
                })
                .collect();
            area
        });
        let costs = s.spawn(|| costs_area(env_name, env, costs, read_costs, now));
        let lost = |name| Area { name, unread: vec!["the read stopped unexpectedly".to_string()], ..Area::default() };
        vec![
            costs.join().unwrap_or_else(|_| lost("Costs")),
            builds.join().unwrap_or_else(|_| lost("Builds")),
            crons.join().unwrap_or_else(|_| lost("Crons")),
            database.join().unwrap_or_else(|_| lost("Database")),
            platform.join().unwrap_or_else(|_| lost("Platform")),
            apps.join().unwrap_or_else(|_| lost("Apps")),
        ]
    });
    Status { areas, read_at: now, login_required: login.into_inner() }
}

fn costs_area(env_name: &str, env: &Environment, costs: &CostDefaults, read_costs: Option<&Config>, now: f64) -> Area {
    if crate::costs::billing_of(env_name, env).is_err() {
        return Area { name: "Costs", skipped: Some("no billing export set up".to_string()), ..Area::default() };
    }
    let read = crate::costs::command::spike_findings(env_name, costs, read_costs, now);
    Area::of("Costs", read.map_err(|e| crate::util::sanitize(&format!("{e:#}"))))
}

fn build_finding(f: &crate::builds::Failing) -> Finding {
    Finding::bad(f.app.clone(), f.text()).hint(format!("{} build {}", crate::util::program(), f.build.short_id()))
}

fn apps_findings(gcp: &Gcp, env_name: &str, now: f64) -> Result<Vec<Finding>, GcpError> {
    let listing = crate::apps::Listing::load(gcp, env_name, now)?;
    if let Err(e) = &listing.health {
        return Err(e.clone());
    }
    Ok(listing.rows().iter().flat_map(app_findings).collect())
}

fn app_findings(row: &Row) -> Vec<Finding> {
    let mut out = Vec::new();
    let name = row.service.id();
    if row.held() {
        out.push(Finding::warn(name, "requests run until the request timeout, keeping instances billed"));
    }
    if let Some(h) = row.health.filter(|h| h.requests >= ERRORS_MIN_REQUESTS)
        && let Some(share) = h.percent("5xx").filter(|s| *s >= ERRORS_WARN)
    {
        let what = format!("{share:.0}% of {:.0} requests answered 5xx in the last 24 h", h.requests);
        let finding = if share >= ERRORS_BAD { Finding::bad(name, what) } else { Finding::warn(name, what) };
        out.push(finding.hint(format!("{} logs {name} --errors --group", crate::util::program())));
    }
    out
}

impl Status {
    pub fn count(&self, level: Level) -> usize {
        self.areas.iter().map(|a| a.count(level)).sum()
    }

    pub fn unread(&self) -> usize {
        self.areas.iter().map(|a| a.unread.len()).sum()
    }

    /// 1 when something needs attention, whatever else; 2 when nothing does but something could
    /// not be read; 0 otherwise.
    pub fn exit_code(&self) -> ExitCode {
        if self.count(Level::Bad) > 0 {
            ExitCode::FAILURE
        } else if self.unread() > 0 {
            ExitCode::from(COULD_NOT_TELL)
        } else {
            ExitCode::SUCCESS
        }
    }

    pub fn json(&self, env: &str, project: &str) -> Value {
        json!({
            "env": env,
            "project": project,
            "readAt": crate::gcp::monitoring::rfc3339(self.read_at),
            "needAttention": self.count(Level::Bad),
            "toLookAt": self.count(Level::Warn),
            "couldNotRead": self.unread(),
            "loginRequired": self.login_required,
            "areas": self.areas.iter().map(|a| json!({
                "area": a.name,
                "findings": a.found,
                "unread": a.unread,
                "skipped": a.skipped,
                "command": a.command(env),
            })).collect::<Vec<_>>(),
        })
    }
}

// ---- what has already been told ------------------------------------------------------------

fn seen_key(area: &str, f: &Finding) -> String {
    format!("{area}|{}", f.subject)
}

fn unread_key(area: &str) -> String {
    format!("{area}|could not read")
}

/// What to tell now, and what to remember for next time.
#[derive(Debug, Default, PartialEq)]
pub struct News {
    /// Area and subject of what needs attention and was not told before.
    pub new: Vec<(String, String)>,
    /// Areas that could not be read and were not told before.
    pub unread: Vec<String>,
    pub remember: BTreeSet<String>,
}

/// An area that could not be read keeps what was known about it: when it can be read again,
/// what it holds is not news.
pub fn news(status: &Status, seen: &BTreeSet<String>) -> News {
    let mut out = News::default();
    for area in &status.areas {
        if !area.unread.is_empty() {
            let key = unread_key(area.name);
            if !seen.contains(&key) {
                out.unread.push(area.name.to_string());
            }
            out.remember.insert(key);
            let prefix = format!("{}|", area.name);
            out.remember
                .extend(seen.iter().filter(|k| k.starts_with(&prefix) && **k != unread_key(area.name)).cloned());
        }
        for f in area.found.iter().filter(|f| f.level == Level::Bad) {
            let key = seen_key(area.name, f);
            if !seen.contains(&key) {
                let item = (area.name.to_string(), f.subject.clone());
                if !out.new.contains(&item) {
                    out.new.push(item);
                }
            }
            out.remember.insert(key);
        }
    }
    out
}

/// Names in a notification at most.
const NOTIFY_MAX_NAMES: usize = 3;

/// The notification for `news`, if there is anything to say.
pub fn notification(env: &str, news: &News, login_required: bool) -> Option<(String, String)> {
    if login_required && !news.unread.is_empty() {
        return Some((format!("gcli · {env}: sign in again"), format!("{} login", crate::util::program())));
    }
    if news.new.is_empty() && news.unread.is_empty() {
        return None;
    }
    let mut parts: Vec<String> =
        news.new.iter().take(NOTIFY_MAX_NAMES).map(|(area, subject)| format!("{area}: {subject}")).collect();
    if news.new.len() > NOTIFY_MAX_NAMES {
        parts.push(format!("and {} more", news.new.len() - NOTIFY_MAX_NAMES));
    }
    if !news.unread.is_empty() {
        parts.push(format!("could not read {}", news.unread.join(", ")));
    }
    let title = match news.new.len() {
        0 => format!("gcli · {env}: a check could not read"),
        1 => format!("gcli · {env}: 1 new problem"),
        n => format!("gcli · {env}: {n} new problems"),
    };
    Some((title, crate::util::ellipsize(&parts.join(" · "), 200)))
}

fn seen_path(env: &str) -> Option<PathBuf> {
    let safe: String =
        env.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    Some(crate::config::config_dir()?.join("status").join(format!("{safe}.json")))
}

pub fn load_seen(env: &str) -> BTreeSet<String> {
    seen_path(env)
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

pub fn store_seen(env: &str, seen: &BTreeSet<String>) -> std::io::Result<()> {
    let Some(path) = seen_path(env) else { return Ok(()) };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(seen).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(areas: Vec<Area>) -> Status {
        Status { areas, read_at: 0.0, login_required: false }
    }

    fn area(name: &'static str, found: Vec<Finding>) -> Area {
        Area { name, found, ..Area::default() }
    }

    #[test]
    fn the_exit_says_attention_before_it_says_unread() {
        let bad = area("Crons", vec![Finding::bad("job", "failing")]);
        let warn = area("Database", vec![Finding::warn("db", "disk")]);
        let unread = Area { name: "Platform", unread: vec!["no".into()], ..Area::default() };
        assert_eq!(status(vec![bad.clone(), unread.clone()]).exit_code(), ExitCode::FAILURE);
        assert_eq!(status(vec![warn.clone(), unread]).exit_code(), ExitCode::from(COULD_NOT_TELL));
        assert_eq!(status(vec![warn]).exit_code(), ExitCode::SUCCESS);
    }

    #[test]
    fn only_what_is_new_is_news() {
        let first = status(vec![area("Crons", vec![Finding::bad("a", "failing"), Finding::warn("b", "paused")])]);
        let told = news(&first, &BTreeSet::new());
        assert_eq!(told.new, [("Crons".to_string(), "a".to_string())], "a warning is no news");
        // The same again, with other words: nothing to say.
        let again = status(vec![area("Crons", vec![Finding::bad("a", "failing for 2 days")])]);
        let second = news(&again, &told.remember);
        assert!(second.new.is_empty() && second.unread.is_empty());
        assert_eq!(notification("prod", &second, false), None);
        // It got better, then failed again: news again.
        let better = news(&status(vec![area("Crons", vec![])]), &second.remember);
        assert!(better.remember.is_empty());
        assert_eq!(news(&again, &better.remember).new.len(), 1);
    }

    #[test]
    fn an_area_that_could_not_be_read_forgets_nothing() {
        let known = status(vec![area("Crons", vec![Finding::bad("a", "failing")])]);
        let seen = news(&known, &BTreeSet::new()).remember;
        let down = status(vec![Area { name: "Crons", unread: vec!["503".into()], ..Area::default() }]);
        let told = news(&down, &seen);
        assert_eq!(told.unread, ["Crons"]);
        assert!(told.remember.contains("Crons|a"));
        let still_down = news(&down, &told.remember);
        assert!(still_down.unread.is_empty(), "told once");
        let back = news(&known, &still_down.remember);
        assert!(back.new.is_empty() && back.unread.is_empty(), "what it held is not news");
        assert!(!back.remember.contains("Crons|could not read"), "and it can tell again next time");
    }

    #[test]
    fn a_notification_is_short() {
        let many: Vec<_> = (0..5).map(|i| ("Crons".to_string(), format!("job{i}"))).collect();
        let (title, body) = notification("prod", &News { new: many, ..News::default() }, false).unwrap();
        assert_eq!(title, "gcli · prod: 5 new problems");
        assert_eq!(body, "Crons: job0 · Crons: job1 · Crons: job2 · and 2 more");
        let down = News { unread: vec!["Costs".to_string()], ..News::default() };
        assert_eq!(notification("prod", &down, false).unwrap().1, "could not read Costs");
        assert!(notification("prod", &down, true).unwrap().0.contains("sign in again"));
    }

    #[test]
    fn an_app_is_judged_by_its_errors_and_how_it_holds_requests() {
        use crate::apps::health::Health;
        let service: crate::gcp::run::Service =
            serde_json::from_str(r#"{"name":"projects/p/locations/r/services/shop"}"#).unwrap();
        let health = |requests: f64, errors: f64| {
            let mut h = Health { requests, ..Health::default() };
            h.classes.insert("5xx".into(), errors);
            h
        };
        let check = |h: &Health| app_findings(&Row { service: &service, health: Some(h), cost: None });
        assert!(check(&health(1000.0, 49.0)).is_empty(), "under 5%");
        assert!(check(&health(49.0, 49.0)).is_empty(), "too few requests to say");
        assert_eq!(check(&health(1000.0, 50.0))[0].level, Level::Warn);
        assert_eq!(check(&health(1000.0, 250.0))[0].level, Level::Bad);
        assert!(check(&health(1000.0, 250.0))[0].what.contains("25%"));
    }
}
