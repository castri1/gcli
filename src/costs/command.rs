//! `gcli costs`: what an environment's project costs, from the billing export in BigQuery.
//!
//! What was read is used while it is fresh (`defaults.costs.autoRefreshHours`). Past that, or
//! with `--refresh`, the export is read again: its queries are dry-run and priced first, and
//! read without asking while under `defaults.costs.autoRefreshMaxUsd`. Messages about reading
//! go to stderr, so `--json` stays clean.

use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

use super::day::{Day, Range};
use super::report::{self, View, change, money};
use super::spikes::{self, Rule, Spike};
use super::{Kind, Snapshot};
use crate::cli::{confirm, current_env, gcp_for, interactive, paint, print_json};
use crate::config::{Config, CostDefaults};
use crate::findings::Finding;
use crate::gcp::GcpError;
use crate::tui::chart::sparkline;
use crate::util::{ellipsize, fmt_ago, fmt_utc, now_secs, program};

#[derive(clap::Args)]
pub struct Args {
    /// Environment (default: the current one, see `use`)
    env: Option<String>,
    /// List by service, by app (Cloud Run service, plus what the apps share) or by SKU
    #[arg(long, value_name = "VIEW", value_parser = ["service", "app", "sku"])]
    by: Option<String>,
    /// A past month instead of this one
    #[arg(long, value_name = "YYYY-MM", conflicts_with = "days")]
    month: Option<String>,
    /// The last N complete days instead of the month (against the N days before)
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(i64).range(1..=90))]
    days: Option<i64>,
    /// Read the billing export again now (BigQuery bills the bytes scanned: the estimate is shown first)
    #[arg(long)]
    refresh: bool,
    /// Exit 1 when an app or a shared service spiked on the last complete UTC day, 2 when the
    /// costs could not be read fresh enough to tell (for cron and CI)
    #[arg(long, conflicts_with_all = ["by", "month", "days"])]
    check: bool,
    /// With --check: the UTC day to check instead
    #[arg(long, value_name = "YYYY-MM-DD", requires = "check")]
    date: Option<String>,
    /// With --check: accept costs read longer ago than defaults.costs.autoRefreshHours
    #[arg(long, requires = "check")]
    allow_stale: bool,
    /// With --check: also raise a desktop notification on a spike or when it cannot tell (for a
    /// scheduled job nobody is watching)
    #[arg(long, requires = "check")]
    notify: bool,
    #[arg(long)]
    json: bool,
}

pub fn run(config: &Config, args: Args) -> Result<ExitCode> {
    let env = match &args.env {
        Some(name) => config.env(name).map(|_| name.clone())?,
        None => current_env(config)?,
    };
    // Settings missing: say what to set before asking for credentials.
    super::billing_of(&env, config.env(&env)?).map_err(cost_error)?;
    let rule = Rule::of(&config.defaults.costs);
    let now = now_secs();
    if args.check {
        // Any failure to read (credentials, permissions, BigQuery) means "could not tell", never
        // the 1 that means "spikes": a cron job alerting on 1 would misread an outage.
        return check(config, &env, &args, &rule, now).or_else(|e| {
            let reason = crate::util::sanitize(&format!("{e:#}"));
            eprintln!("error: {reason}");
            let login = e.downcast_ref::<GcpError>().is_some_and(GcpError::is_login_required);
            if login {
                eprintln!("hint: run `{} login` (or press L in the UI)", crate::util::program());
            }
            if args.notify {
                let (title, body) = could_not_tell(&env, if login { "Sign in again: gcli login" } else { &reason });
                crate::platform::notify_and_wait(&title, &body);
            }
            Ok(ExitCode::from(STALE_EXIT))
        });
    }
    let today = Day::of(now);
    let month = match &args.month {
        Some(text) => Day::parse_month(text).ok_or_else(|| anyhow!("--month is YYYY-MM, not '{text}'"))?,
        None => today.month_start(),
    };
    if args.by.as_deref() == Some("sku") && args.days.is_some() {
        bail!("SKUs are read by month: --by sku goes with --month, not --days");
    }
    let s = if month < today.month_start() {
        obtain(config, &env, Kind::Month(month), args.refresh, now)?
    } else {
        obtain(config, &env, Kind::Current, args.refresh, now)?
    };
    let (s, view) = match args.days {
        Some(n) => {
            let view = report::days_view(&s, n).ok_or_else(|| anyhow!("no complete day was read"))?;
            let need = Range::new(view.before.map_or(view.range.from, |b| b.from), view.range.to);
            if s.days.is_some_and(|d| d.covers(need)) {
                (s, view)
            } else {
                let window = obtain(config, &env, Kind::Days(need), args.refresh, now)?;
                (window, view)
            }
        }
        None => {
            let view =
                report::month_view(&s, month).ok_or_else(|| anyhow!("nothing of {} was read", month.month_name()))?;
            (s, view)
        }
    };
    let by = args.by.as_deref();
    if args.json {
        print_json(&summary(&s, month, &view, args.days.is_some(), by, &rule));
        return Ok(ExitCode::SUCCESS);
    }
    header(&s);
    if args.days.is_some() {
        print_days(&s, &view);
    } else {
        print_month(&s, month, by.is_none());
    }
    match by {
        Some("service") => print_services(&s, &view, args.days.is_none() && s.kind == Kind::Current),
        Some("app") => print_apps(&s, &view, &rule),
        Some("sku") => print_skus(&s, month),
        _ => {}
    }
    footer(&s, &rule, by.is_none() && args.days.is_none());
    Ok(ExitCode::SUCCESS)
}

fn cost_error(e: super::CostError) -> anyhow::Error {
    if e.login { anyhow::Error::new(GcpError::LoginRequired { detail: e.message }) } else { anyhow!(e.message) }
}

/// What was read for `env` (`kind`), read again when stale or asked (`refresh`).
fn obtain(config: &Config, env: &str, kind: Kind, refresh: bool, now: f64) -> Result<Snapshot> {
    let costs = &config.defaults.costs;
    let cached = super::load(env, kind);
    if !refresh && let Some(s) = cached.as_ref().filter(|s| !s.stale(now, costs.auto_refresh_hours)) {
        return Ok(s.clone());
    }
    let gcp = gcp_for(config, env)?;
    let prepared = super::prepare(&gcp, env, config.env(env)?, kind, now).map_err(cost_error)?;
    let estimate = prepared.describe(costs.bigquery_usd_per_tib);
    let price = super::usd(prepared.billed, costs.bigquery_usd_per_tib);
    if !refresh && price > costs.auto_refresh_max_usd {
        if let Some(s) = cached {
            eprintln!(
                "{}",
                paint(
                    &format!(
                        "note: costs read {}; reading again would scan {estimate}, over defaults.costs.autoRefreshMaxUsd: add --refresh to read",
                        fmt_ago(s.read_at)
                    ),
                    "33"
                )
            );
            return Ok(s);
        }
        let question = format!("Read the billing export of {env}: {estimate}?");
        if !interactive() {
            bail!(
                "reading the billing export of {env} would scan {estimate}, over defaults.costs.autoRefreshMaxUsd: add --refresh to read it"
            );
        }
        if !confirm(&question) {
            bail!("not read");
        }
    }
    eprintln!("{}", paint(&format!("Reading the billing export of {env}: {estimate}…"), "90"));
    let s = super::run(&gcp, prepared, now).map_err(cost_error)?;
    if let Err(e) = super::store(&s, costs) {
        eprintln!("{}", paint(&format!("! could not keep what was read: {e}"), "33"));
    }
    let ledger = super::ledger::month(&Day::of(now).month_key());
    eprintln!(
        "{}",
        paint(
            &format!(
                "Read: {} billed ({}) · BigQuery this month: {} queries, {} ({})",
                report::bytes(s.bytes_billed),
                report::usd_small(super::usd(s.bytes_billed, costs.bigquery_usd_per_tib)),
                ledger.queries,
                report::bytes(ledger.bytes),
                report::usd_small(super::usd(ledger.bytes, costs.bigquery_usd_per_tib))
            ),
            "90"
        )
    );
    Ok(s)
}

// ---- --check ---------------------------------------------------------------------------------

/// `--check`'s exit code when it cannot tell: distinct from 1 (spikes) so a cron job can alert
/// on both without mistaking one for the other.
const STALE_EXIT: u8 = 2;

fn check(config: &Config, env: &str, args: &Args, rule: &Rule, now: f64) -> Result<ExitCode> {
    let date = match &args.date {
        Some(text) => Some(Day::parse(text).ok_or_else(|| anyhow!("--date is YYYY-MM-DD, not '{text}'"))?),
        None => None,
    };
    let current = obtain(config, env, Kind::Current, args.refresh, now)?;
    // obtain() keeps an old read when reading again would cost more than autoRefreshMaxUsd. A
    // check run by cron must not report "no spike" from it: yesterday may not be in it at all.
    let hours = config.defaults.costs.auto_refresh_hours;
    if date.is_none() && !args.allow_stale && current.stale(now, hours) {
        eprintln!(
            "error: the costs of {env} were read {} (over defaults.costs.autoRefreshHours = {hours}) and were not read again: \
             run with --refresh, or --allow-stale to check the old read",
            fmt_ago(current.read_at)
        );
        if args.notify {
            let (title, body) = could_not_tell(env, &format!("The costs were read {}", fmt_ago(current.read_at)));
            crate::platform::notify_and_wait(&title, &body);
        }
        return Ok(ExitCode::from(STALE_EXIT));
    }
    let last = current.last_complete_day().ok_or_else(|| anyhow!("no complete day was read"))?;
    let day = date.unwrap_or(last);
    if day > last {
        bail!(
            "{day} is not a complete day in the billing export yet (UTC days; the export runs 4 to 12 hours behind): the last complete one is {last}"
        );
    }
    let need = Range::new(spikes::history(day).from, day);
    let s = if current.days.is_some_and(|d| d.covers(need)) {
        current
    } else {
        obtain(config, env, Kind::Days(need), args.refresh, now)?
    };
    let found = report::spikes(&s, day, rule).map_err(|e| anyhow!(e))?;
    if args.notify && !found.is_empty() {
        let (title, body) = spike_notification(env, day, &found, &s.currency);
        crate::platform::notify_and_wait(&title, &body);
    }
    if args.json {
        print_json(&json!({
            "env": env,
            "project": s.project,
            "day": day,
            "utcDays": true,
            "readAt": report::rfc3339(s.read_at),
            "spikes": found,
            "rule": rule_json(rule),
        }));
    } else {
        println!(
            "{}  {}",
            paint(env, "1"),
            paint(
                &format!("{} · cost spikes on {day} (UTC) · billing export read {}", s.project, fmt_ago(s.read_at)),
                "90"
            )
        );
        print_spikes(&found, &s.currency);
        if found.is_empty() {
            println!("{} no app or shared service spiked on {day}", paint("✓", "32"));
        }
        println!("{}", paint(&format!("Rule: {} (defaults.costs in the config).", rule.describe()), "90"));
    }
    Ok(if found.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// Notifications are short: the most expensive first, and how many more.
const NOTIFY_MAX_NAMES: usize = 3;

fn spike_notification(env: &str, day: Day, found: &[Spike], currency: &str) -> (String, String) {
    let mut sorted: Vec<&Spike> = found.iter().collect();
    sorted.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    let mut names: Vec<String> = sorted
        .iter()
        .take(NOTIFY_MAX_NAMES)
        .map(|x| format!("{} {}", ellipsize(&x.name, 30), money(x.cost, currency)))
        .collect();
    if found.len() > NOTIFY_MAX_NAMES {
        names.push(format!("and {} more", found.len() - NOTIFY_MAX_NAMES));
    }
    let what = if found.len() == 1 { "1 cost spike".to_string() } else { format!("{} cost spikes", found.len()) };
    (format!("gcli · {env}: {what}"), format!("{day} (UTC): {}", names.join(", ")))
}

fn could_not_tell(env: &str, reason: &str) -> (String, String) {
    (format!("gcli · {env}: cost check failed"), ellipsize(reason, 160))
}

fn print_spikes(found: &[Spike], currency: &str) {
    let w = found.iter().map(|x| x.name.chars().count()).max().unwrap_or(0).clamp(10, 40);
    for x in found {
        println!(
            "  {} {:<w$}  {:<6} {:>10}  {}",
            paint("✗", "31"),
            ellipsize(&x.name, w),
            match x.kind {
                spikes::Kind::App => "app",
                spikes::Kind::Shared => "shared",
            },
            money(x.cost, currency),
            x.reason()
        );
    }
}

fn rule_json(rule: &Rule) -> Value {
    json!({ "spikeRatio": rule.ratio, "spikeMinUsd": rule.min_usd, "newAppMinUsd": rule.new_min_usd, "medianDays": spikes::WINDOW })
}

/// The last complete day's spikes as findings, for `gcli status`. `read`: the config, to read the
/// billing export again when what was kept is old; without it only what was kept counts.
pub fn spike_findings(env: &str, costs: &CostDefaults, read: Option<&Config>, now: f64) -> Result<Vec<Finding>> {
    let rule = Rule::of(costs);
    let obtained = |kind| match read {
        Some(config) => obtain(config, env, kind, false, now),
        None => super::load(env, kind)
            .ok_or_else(|| anyhow!("no cost data kept: run `{} costs {env} --refresh`", program())),
    };
    let current = obtained(Kind::Current)?;
    if current.stale(now, costs.auto_refresh_hours) {
        bail!("the costs were read {}: run `{} costs {env} --refresh`", fmt_ago(current.read_at), program());
    }
    let last = current.last_complete_day().ok_or_else(|| anyhow!("no complete day was read"))?;
    let need = Range::new(spikes::history(last).from, last);
    let s = if current.days.is_some_and(|d| d.covers(need)) { current } else { obtained(Kind::Days(need))? };
    let found = report::spikes(&s, last, &rule).map_err(|e| anyhow!(e))?;
    Ok(found
        .iter()
        .map(|x| {
            Finding::bad(x.name.clone(), format!("{} on {last} (UTC): {}", money(x.cost, &s.currency), x.reason()))
        })
        .collect())
}

// ---- text ------------------------------------------------------------------------------------

fn header(s: &Snapshot) {
    let lag =
        if s.data_through > 0.0 { format!(" · data through {}", fmt_utc(s.data_through)) } else { String::new() };
    println!(
        "{}  {}",
        paint(&s.env, "1"),
        paint(&format!("{} · billing export read {}{lag} · UTC days", s.project, fmt_ago(s.read_at)), "90")
    );
}

fn totals(t: report::Totals, c: &str) -> String {
    format!("gross {}   net {}", paint(&format!("{:>9}", money(t.gross, c)), "1"), money(t.net, c))
}

/// The month's figures; with `full`, its budgets and credits too.
fn print_month(s: &Snapshot, month: Day, full: bool) {
    let c = s.currency.as_str();
    let Some(o) = report::overview(s, month) else { return };
    println!("{}", paint(&month.month_name(), "1"));
    let previous = month.previous_month();
    match (o.complete, s.kind) {
        (Some(r), Kind::Current) => {
            let vs = match (change(o.complete_total.gross, o.previous_total.gross), o.previous) {
                (Some(ch), Some(p)) => format!("   {ch} vs {} ({})", p.label(), money(o.previous_total.gross, c)),
                _ => String::new(),
            };
            println!("  {:<26}{}{vs}", format!("{} (complete)", r.label()), totals(o.complete_total, c));
        }
        (Some(_), _) => {
            let vs = o
                .previous_month
                .and_then(|p| change(o.total.gross, p.gross).map(|ch| (ch, p)))
                .map(|(ch, p)| format!("   {ch} vs {} ({})", previous.month_name(), money(p.gross, c)))
                .unwrap_or_default();
            println!("  {:<26}{}{vs}", month.month_name(), totals(o.total, c));
        }
        (None, _) => {}
    }
    for p in &o.partial {
        println!(
            "  {:<26}{}",
            format!("{} (partial)", p.day.short()),
            paint(&format!("gross {:>9} so far", money(p.gross, c)), "90")
        );
    }
    if s.kind == Kind::Current {
        println!("  {:<26}{}", "Month to date", totals(o.total, c));
        if let (Some(f), Some(r)) = (o.forecast, o.complete) {
            println!(
                "  {:<26}{}",
                "Forecast",
                paint(
                    &format!(
                        "gross {:>9}   net {}  (straight line from {} complete day{})",
                        money(f.gross, c),
                        money(f.net, c),
                        r.days(),
                        if r.days() == 1 { "" } else { "s" }
                    ),
                    "90"
                )
            );
        }
        if let Some(p) = o.previous_month {
            println!("  {:<26}{}", previous.month_name(), totals(p, c));
        }
    }
    if let Some(a) = o.account {
        let mut text = format!("{} gross {} · net {}", &month.month_name()[..3], money(a.gross, c), money(a.net, c));
        if let Some(p) = o.account_previous {
            text.push_str(&format!(
                " · {} gross {} · net {}",
                &previous.month_name()[..3],
                money(p.gross, c),
                money(p.net, c)
            ));
        }
        println!("  {:<26}{}", "Billing account", text);
    }
    if !full {
        return;
    }
    let budgets = report::budgets(s);
    if !budgets.is_empty() {
        println!("\n{}", paint("Budgets", "1"));
        let w = budgets.iter().map(|b| b.name.chars().count()).max().unwrap_or(0).clamp(10, 40);
        for b in &budgets {
            let currency = if b.currency.is_empty() { c } else { b.currency.as_str() };
            let amount = b.amount.map_or("?".to_string(), |a| money(a, currency));
            let pct = b.ratio().map_or(String::new(), |r| format!("{:>4.0} %", (r * 100.0).max(0.0)));
            let color = match b.ratio() {
                Some(r) if r >= 1.0 => "31",
                Some(r) if b.thresholds.iter().any(|t| t.crossed) || r >= 0.9 => "33",
                _ => "0",
            };
            let thresholds: Vec<String> = b
                .thresholds
                .iter()
                .map(|t| {
                    let text = format!("{}{:.0} %", if t.forecast { "forecast " } else { "" }, t.percent * 100.0);
                    if t.crossed { paint(&format!("{text} ✗"), "33") } else { text }
                })
                .collect();
            println!(
                "  {} {:<w$}  {:>9} of {:<9} {}  {}",
                if b.covers_project { "●" } else { " " },
                ellipsize(&b.name, w),
                money(b.spent, currency),
                amount,
                paint(&pct, color),
                paint(
                    &format!(
                        "{}{} · {}",
                        b.period.label(),
                        b.forecast.map_or(String::new(), |f| format!(" · forecast {}", money(f, currency))),
                        thresholds.join(" · ")
                    ),
                    "90"
                )
            );
            for n in &b.notes {
                println!("    {}", paint(n, "90"));
            }
        }
        println!("{}", paint("  ● covers this project; the others are other projects' or the billing account's", "90"));
    }
    if !o.credits.is_empty() {
        println!("\n{}", paint(&format!("Credits in {} (this project)", month.month_name()), "1"));
        for cr in o.credits.iter().take(10) {
            println!("  {:<10} {:>10}  {}", cr.kind, money(cr.amount, c), paint(&ellipsize(&cr.name, 70), "90"));
        }
    }
}

fn print_days(s: &Snapshot, view: &View) {
    let c = s.currency.as_str();
    let mut days: std::collections::BTreeMap<Day, report::Totals> = Default::default();
    for d in s.daily.iter().filter(|d| view.range.contains(d.day)) {
        let t = days.entry(d.day).or_default();
        t.gross += d.cost;
        t.net += d.cost + d.credits;
    }
    let total: f64 = days.values().map(|t| t.gross).sum();
    let before: Option<f64> = view
        .before
        .filter(|b| s.days.is_some_and(|d| d.covers(*b)))
        .map(|b| s.daily.iter().filter(|d| b.contains(d.day)).map(|d| d.cost).sum());
    let vs = match (before.and_then(|b| change(total, b)), view.before, before) {
        (Some(ch), Some(r), Some(b)) => format!("   {ch} vs {} ({})", r.label(), money(b, c)),
        _ => String::new(),
    };
    println!(
        "{}  gross {}{vs}",
        paint(&format!("{} (complete days)", view.range.label()), "1"),
        paint(&money(total, c), "1")
    );
    let max = days.values().map(|t| t.gross).fold(0.0f64, f64::max);
    for (day, t) in &days {
        let bar = if max > 0.0 { "█".repeat((t.gross / max * 30.0).round() as usize) } else { String::new() };
        println!("  {day}  {:>10}  {:>10}  {}", money(t.gross, c), paint(&money(t.net, c), "90"), paint(&bar, "36"));
    }
    println!("{}", paint("  day (UTC)        gross         net", "90"));
}

fn spark(values: &[f64]) -> String {
    sparkline(&values.iter().map(|v| Some(*v)).collect::<Vec<_>>())
}

fn print_services(s: &Snapshot, view: &View, with_spark: bool) {
    let c = s.currency.as_str();
    let rows = report::services(s, view);
    let w = rows.iter().map(|r| r.service.chars().count()).max().unwrap_or(0).clamp(10, 36);
    let compare = match (view.now, view.before) {
        (Some(n), Some(b)) if n == view.range => format!("vs {}", b.label()),
        (Some(_), Some(b)) => format!("complete vs {}", b.label()),
        _ => String::new(),
    };
    println!(
        "\n{}",
        paint(
            &format!(
                "{:<w$}  {:>10}  {:>10}  {:<24}  {}",
                "SERVICE",
                "GROSS",
                "NET",
                compare,
                if with_spark { format!("LAST {} DAYS", report::SPARK_DAYS) } else { String::new() }
            ),
            "90;1"
        )
    );
    for r in &rows {
        let (ch, was) = match r.before {
            Some(b) => (change(r.now, b).unwrap_or_else(|| "new".into()), format!("({})", money(b, c))),
            None => (String::new(), String::new()),
        };
        // Padded before painting: colour codes have no width.
        let pad = " ".repeat(24usize.saturating_sub(ch.chars().count() + 1 + was.chars().count()));
        println!(
            "{:<w$}  {:>10}  {:>10}  {ch} {}{pad}  {}{}",
            ellipsize(&r.service, w),
            money(r.total.gross, c),
            money(r.total.net, c),
            paint(&was, "90"),
            if with_spark { paint(&spark(&r.daily), "36") } else { String::new() },
            paint(if r.by_app { "  per app: --by app" } else { "  shared" }, "90")
        );
    }
    let gross: f64 = rows.iter().map(|r| r.total.gross).sum();
    let net: f64 = rows.iter().map(|r| r.total.net).sum();
    println!("{:<w$}  {:>10}  {:>10}", paint("Total", "1"), paint(&money(gross, c), "1"), money(net, c));
}

fn print_apps(s: &Snapshot, view: &View, rule: &Rule) {
    let c = s.currency.as_str();
    let (apps, shared) = report::apps(s, view, rule);
    let last = s.last_complete_day().map_or("last day".to_string(), Day::short);
    let w = apps.iter().map(|a| a.app.chars().count()).max().unwrap_or(0).clamp(10, 40);
    println!(
        "\n{}",
        paint(
            &format!(
                "{:<w$}  {:>10}  {:>10}  {:>10}  LAST {} DAYS",
                "APP (CLOUD RUN)",
                view.range.label(),
                last,
                "MEDIAN 14D",
                report::SPARK_DAYS
            ),
            "90;1"
        )
    );
    for a in &apps {
        println!(
            "{:<w$}  {:>10}  {}  {:>10}  {}{}",
            ellipsize(&a.app, w),
            money(a.total, c),
            if a.spike {
                paint(&format!("{:>10}", money(a.last_day, c)), "31;1")
            } else {
                format!("{:>10}", money(a.last_day, c))
            },
            a.median14.map_or("—".into(), |m| money(m, c)),
            paint(&spark(&a.daily), "36"),
            if a.spike { paint("  ✗ spike", "31") } else { String::new() }
        );
    }
    let app_total: f64 = apps.iter().map(|a| a.total).sum();
    println!("{:<w$}  {:>10}", paint("Apps", "1"), paint(&money(app_total, c), "1"));
    if !shared.is_empty() {
        println!("\n{}", paint("SHARED (no app can be told from these: shown whole, never spread)", "90;1"));
        for r in &shared {
            println!("{:<w$}  {:>10}", ellipsize(&r.service, w), money(r.total, c));
        }
        let total: f64 = shared.iter().map(|r| r.total).sum();
        println!("{:<w$}  {:>10}", paint("Shared", "1"), paint(&money(total, c), "1"));
    }
}

fn print_skus(s: &Snapshot, month: Day) {
    let c = s.currency.as_str();
    let rows = report::skus(s, month);
    if rows.is_empty() {
        println!("{}", paint(&format!("\nNo SKUs read for {}", month.month_name()), "90"));
        return;
    }
    let w = rows.iter().map(|r| r.sku.chars().count()).max().unwrap_or(0).clamp(10, 52);
    let mut service = "";
    for r in rows {
        if r.service != service {
            service = &r.service;
            println!("\n{}", paint(service, "1"));
        }
        let (amount, unit, rate) = report::sku_usage(r, c);
        println!(
            "  {:<w$}  {:>10}  {:>8} {:<14}  {}",
            ellipsize(&r.sku, w),
            money(r.cost, c),
            amount,
            ellipsize(&unit, 14),
            paint(&rate, "90")
        );
    }
    println!(
        "{}",
        paint("\nThe rate is the cost per unit of usage, as the export counts it (the project's own price).", "90")
    );
}

fn footer(s: &Snapshot, rule: &Rule, with_spikes: bool) {
    if with_spikes && let Some(last) = s.last_complete_day() {
        match report::spikes(s, last, rule) {
            Ok(found) if found.is_empty() => {
                println!(
                    "\n{} {}",
                    paint("✓", "32"),
                    paint(&format!("no cost spike on {last} (the last complete UTC day)"), "90")
                )
            }
            Ok(found) => {
                println!("\n{}", paint(&format!("Cost spikes on {last}"), "31;1"));
                print_spikes(&found, &s.currency);
            }
            Err(_) => {}
        }
    }
    for n in &s.notes {
        println!("{}", paint(&format!("note: {n}"), "90"));
    }
    println!(
        "{}",
        paint(
            &format!(
                "\nUTC days. --by service|app|sku, --month YYYY-MM, --days N, --check, --refresh ({} costs --help)",
                program()
            ),
            "90"
        )
    );
}

// ---- JSON ------------------------------------------------------------------------------------

/// Everything a reading says for `month` (or the last days), as `--json` and the MCP server
/// give it.
pub(crate) fn summary(s: &Snapshot, month: Day, view: &View, days: bool, by: Option<&str>, rule: &Rule) -> Value {
    let last = s.last_complete_day();
    let mut out = json!({
        "env": s.env,
        "project": s.project,
        "currency": s.currency,
        "utcDays": true,
        "readAt": report::rfc3339(s.read_at),
        "dataThrough": (s.data_through > 0.0).then(|| report::rfc3339(s.data_through)),
        "lastCompleteDay": last,
        "notes": s.notes,
    });
    if days {
        let services = report::services(s, view);
        out["days"] = json!({
            "range": view.range,
            "gross": services.iter().map(|r| r.total.gross).sum::<f64>(),
            "net": services.iter().map(|r| r.total.net).sum::<f64>(),
            "before": view.before,
            "beforeGross": view.before.filter(|b| s.days.is_some_and(|d| d.covers(*b))).map(|_| services.iter().filter_map(|r| r.before).sum::<f64>()),
        });
    } else if let Some(o) = report::overview(s, month) {
        out["month"] = serde_json::to_value(&o).unwrap_or_default();
        out["budgets"] = serde_json::to_value(report::budgets(s)).unwrap_or_default();
    }
    if let Some(d) = last {
        out["spikes"] = match report::spikes(s, d, rule) {
            Ok(found) => json!({ "day": d, "found": found, "rule": rule_json(rule) }),
            Err(e) => json!({ "day": d, "error": e }),
        };
    }
    match by {
        Some("service") => out["services"] = serde_json::to_value(report::services(s, view)).unwrap_or_default(),
        Some("app") => {
            let (apps, shared) = report::apps(s, view, rule);
            out["apps"] = serde_json::to_value(apps).unwrap_or_default();
            out["shared"] = serde_json::to_value(shared).unwrap_or_default();
        }
        Some("sku") => {
            let skus: Vec<Value> = report::skus(s, month)
                .into_iter()
                .map(|r| json!({ "service": r.service, "sku": r.sku, "cost": r.cost, "amount": r.amount, "unit": r.unit, "rate": r.rate() }))
                .collect();
            out["skus"] = Value::Array(skus);
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::costs::tests::day;

    fn spike(name: &str, cost: f64) -> Spike {
        Spike {
            name: name.into(),
            kind: spikes::Kind::App,
            day: day("2026-11-03"),
            cost,
            median: Some(0.5),
            history_days: 14,
        }
    }

    #[test]
    fn a_notification_names_the_most_expensive_spikes() {
        let d = day("2026-11-03");
        let (title, body) = spike_notification("prod", d, &[spike("small", 1.5)], "USD");
        assert_eq!(title, "gcli · prod: 1 cost spike");
        assert_eq!(body, "2026-11-03 (UTC): small US$ 1.50");

        let found = [spike("a", 1.0), spike("b", 4.0), spike("c", 3.0), spike("d", 2.0)];
        let (title, body) = spike_notification("prod", d, &found, "USD");
        assert_eq!(title, "gcli · prod: 4 cost spikes");
        assert_eq!(body, "2026-11-03 (UTC): b US$ 4.00, c US$ 3.00, d US$ 2.00, and 1 more");
    }

    #[test]
    fn a_failed_check_says_so_in_short() {
        let (title, body) = could_not_tell("prod", &"x".repeat(500));
        assert_eq!(title, "gcli · prod: cost check failed");
        assert!(body.chars().count() <= 160, "{body}");
    }
}
