//! What a reading says, put together for the Costs module, `gcli costs`, the MCP tools and
//! `apps.json`: pure functions over a [`Snapshot`], and the figures as text.
//!
//! Gross is what was used (credits left out), net what is paid (credits included). Months and
//! days are UTC. Comparisons use complete days only, against the same days of the month before.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use super::budget::{self, Spend};
use super::day::{Day, Range};
use super::spikes::{self, Kind, Rule, Spike};
use super::sql::CLOUD_RUN;
use super::{SkuCost, Snapshot};

/// Days in a sparkline.
pub const SPARK_DAYS: i64 = 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Totals {
    pub gross: f64,
    pub net: f64,
}

impl Totals {
    fn add(&mut self, cost: f64, credits: f64) {
        self.gross += cost;
        self.net += cost + credits;
    }
}

/// What is summed and what is compared: `range` (every day read, today's partial one
/// included), and its complete days `now` against the same days `before`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct View {
    pub range: Range,
    pub now: Option<Range>,
    pub before: Option<Range>,
}

/// The days of `month` read, its complete days and the same days of the month before.
pub fn month_view(s: &Snapshot, month: Day) -> Option<View> {
    let days = s.days?;
    let range = Range::new(month.max(days.from), month.month_end().min(days.to));
    if range.from > range.to {
        return None;
    }
    let now = s.last_complete_day().map(|last| Range::new(range.from, last.min(range.to))).filter(|r| r.from <= r.to);
    let before = now.map(|r| {
        let previous = month.previous_month();
        Range::new(previous, previous.plus(r.days() - 1).min(previous.month_end()))
    });
    Some(View { range, now, before })
}

/// The last `n` complete days, against the `n` days before them.
pub fn days_view(s: &Snapshot, n: i64) -> Option<View> {
    let last = s.last_complete_day()?;
    let now = Range::new(last.plus(1 - n.max(1)), last);
    let before = Range::new(now.from.plus(-n.max(1)), now.from.plus(-1));
    Some(View { range: now, now: Some(now), before: Some(before) })
}

fn sum<'a>(rows: impl Iterator<Item = &'a super::DayCost>, range: Option<Range>) -> Totals {
    let mut t = Totals::default();
    if let Some(r) = range {
        for d in rows.filter(|d| r.contains(d.day)) {
            t.add(d.cost, d.credits);
        }
    }
    t
}

/// Whether the days of `r` were all read.
fn read(s: &Snapshot, r: Option<Range>) -> Option<Range> {
    r.filter(|r| s.days.is_some_and(|d| d.covers(*r)))
}

// ---- the month -----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Credit {
    #[serde(rename = "type")]
    pub kind: String,
    pub name: String,
    pub amount: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PartialDay {
    pub day: Day,
    pub gross: f64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub month: String,
    /// Every day read, the partial ones included.
    pub total: Totals,
    pub complete: Option<Range>,
    pub complete_total: Totals,
    /// The same days of the month before, and what they cost.
    pub previous: Option<Range>,
    pub previous_total: Totals,
    /// All of the month before, when read.
    pub previous_month: Option<Totals>,
    /// The whole month at the pace of its complete days (a straight line).
    pub forecast: Option<Totals>,
    /// Days after the last complete one, with their gross so far.
    pub partial: Vec<PartialDay>,
    /// The whole billing account this month and the month before, when read.
    pub account: Option<Totals>,
    pub account_previous: Option<Totals>,
    /// The project's credits this month, by credit.
    pub credits: Vec<Credit>,
}

/// The billing account's gross and net over `month` (`2026-10`).
fn account_month(s: &Snapshot, month: &str) -> Option<Totals> {
    let months = s.months?;
    let first = Day::parse_month(month)?;
    if first < months.from.month_start() || first > months.to {
        return None;
    }
    let mut t = Totals::default();
    for r in s.account_months.iter().filter(|r| r.month == month) {
        if r.credit_type.is_empty() {
            t.add(r.amount, 0.0);
        } else {
            t.net += r.amount;
        }
    }
    Some(t)
}

pub fn overview(s: &Snapshot, month: Day) -> Option<Overview> {
    let view = month_view(s, month)?;
    let previous = month.previous_month();
    let full_previous = Range::new(previous, previous.month_end());
    let last = s.last_complete_day();
    let complete_total = sum(s.daily.iter(), view.now);
    // A straight line from the complete days, while the month is not over.
    let forecast = view.now.filter(|r| r.to < month.month_end()).map(|r| {
        let scale = month.days_in_month() as f64 / r.days() as f64;
        Totals { gross: complete_total.gross * scale, net: complete_total.net * scale }
    });
    let mut partial: BTreeMap<Day, f64> = BTreeMap::new();
    for d in s.daily.iter().filter(|d| view.range.contains(d.day) && last.is_none_or(|l| d.day > l)) {
        *partial.entry(d.day).or_default() += d.cost;
    }
    let mut credits: BTreeMap<(String, String), f64> = BTreeMap::new();
    for r in s.account_months.iter().filter(|r| r.month == month.month_key() && !r.credit_type.is_empty()) {
        if r.project_number == s.project_number {
            *credits.entry((r.credit_type.clone(), r.credit_name.clone())).or_default() += r.amount;
        }
    }
    let mut credits: Vec<Credit> = credits
        .into_iter()
        .map(|((kind, name), amount)| Credit { kind, name, amount })
        .filter(|c| c.amount.abs() >= 0.005)
        .collect();
    credits.sort_by(|a, b| a.amount.total_cmp(&b.amount));
    Some(Overview {
        month: month.month_key(),
        total: sum(s.daily.iter(), Some(view.range)),
        complete: view.now,
        complete_total,
        previous: view.before,
        previous_total: sum(s.daily.iter(), read(s, view.before)),
        previous_month: read(s, Some(full_previous)).map(|r| sum(s.daily.iter(), Some(r))),
        forecast,
        partial: partial.into_iter().map(|(day, gross)| PartialDay { day, gross }).collect(),
        account: account_month(s, &month.month_key()),
        account_previous: account_month(s, &previous.month_key()),
        credits,
    })
}

// ---- services and SKUs ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRow {
    pub service: String,
    pub service_id: String,
    pub total: Totals,
    /// Gross of the view's complete days, and of the days compared with.
    pub now: f64,
    pub before: Option<f64>,
    /// Gross per day of the last [`SPARK_DAYS`] complete days, oldest first.
    pub daily: Vec<f64>,
    /// Cloud Run: split by app. The others are shared by every app.
    pub by_app: bool,
}

/// The last [`SPARK_DAYS`] complete days.
pub fn spark_range(s: &Snapshot) -> Option<Range> {
    let last = s.last_complete_day()?;
    Some(Range::new(last.plus(1 - SPARK_DAYS), last))
}

fn series(spark: Option<Range>, days: &BTreeMap<Day, f64>) -> Vec<f64> {
    let Some(r) = spark else { return Vec::new() };
    (0..r.days()).map(|i| days.get(&r.from.plus(i)).copied().unwrap_or(0.0)).collect()
}

pub fn services(s: &Snapshot, view: &View) -> Vec<ServiceRow> {
    let spark = spark_range(s);
    let mut by: BTreeMap<&str, (ServiceRow, BTreeMap<Day, f64>)> = BTreeMap::new();
    let before = read(s, view.before);
    for d in &s.daily {
        let (row, days) = by.entry(d.service.as_str()).or_insert_with(|| {
            let row = ServiceRow {
                service: d.service.clone(),
                service_id: d.service_id.clone(),
                total: Totals::default(),
                now: 0.0,
                before: before.map(|_| 0.0),
                daily: Vec::new(),
                by_app: d.service_id == CLOUD_RUN,
            };
            (row, BTreeMap::new())
        });
        if view.range.contains(d.day) {
            row.total.add(d.cost, d.credits);
        }
        if view.now.is_some_and(|r| r.contains(d.day)) {
            row.now += d.cost;
        }
        if let (Some(r), Some(b)) = (before, row.before.as_mut())
            && r.contains(d.day)
        {
            *b += d.cost;
        }
        *days.entry(d.day).or_default() += d.cost;
    }
    let mut rows: Vec<ServiceRow> = by
        .into_values()
        .map(|(mut row, days)| {
            row.daily = series(spark, &days);
            row
        })
        .filter(|r| r.total.gross.abs() >= 0.005 || r.before.is_some_and(|b| b.abs() >= 0.005))
        .collect();
    rows.sort_by(|a, b| b.total.gross.total_cmp(&a.total.gross));
    rows
}

/// The project's SKUs in `month`, by service then cost.
pub fn skus(s: &Snapshot, month: Day) -> Vec<&SkuCost> {
    let key = month.month_key();
    let mut rows: Vec<&SkuCost> =
        s.skus.iter().filter(|r| r.month == key && (r.cost.abs() >= 0.005 || r.amount > 0.0)).collect();
    rows.sort_by(|a, b| a.service.cmp(&b.service).then(b.cost.total_cmp(&a.cost)));
    rows
}

// ---- apps ----------------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AppRow {
    pub app: String,
    /// Gross over the view.
    pub total: f64,
    /// Gross on the last complete day.
    pub last_day: f64,
    /// Of the 14 days before the last complete one (only the days it ran).
    pub median14: Option<f64>,
    pub spike: bool,
    pub daily: Vec<f64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SharedRow {
    pub service: String,
    pub total: f64,
}

/// Cost by day of each app (Cloud Run service), or of each other service as a whole.
fn by_day(s: &Snapshot, kind: Kind) -> BTreeMap<String, BTreeMap<Day, f64>> {
    let mut out: BTreeMap<String, BTreeMap<Day, f64>> = BTreeMap::new();
    for d in &s.daily {
        let name = match kind {
            Kind::App if d.service_id == CLOUD_RUN && !d.app.is_empty() => &d.app,
            Kind::Shared if d.service_id != CLOUD_RUN => &d.service,
            _ => continue,
        };
        *out.entry(name.clone()).or_default().entry(d.day).or_default() += d.cost;
    }
    out
}

/// Each app over `view`, most expensive first, and what no app can be told from: the other
/// services, and Cloud Run rows without a service.
pub fn apps(s: &Snapshot, view: &View, rule: &Rule) -> (Vec<AppRow>, Vec<SharedRow>) {
    let last = s.last_complete_day();
    let spark = spark_range(s);
    let flagged: Vec<String> = last
        .and_then(|d| spikes(s, d, rule).ok())
        .unwrap_or_default()
        .into_iter()
        .filter(|x| x.kind == Kind::App)
        .map(|x| x.name)
        .collect();
    let mut apps: Vec<AppRow> = by_day(s, Kind::App)
        .into_iter()
        .map(|(app, days)| {
            let total = days.range(view.range.from..=view.range.to).map(|(_, c)| c).sum();
            let (last_day, median14) = match last {
                Some(d) => {
                    let h = spikes::history(d);
                    let before: Vec<f64> = days.range(h.from..=h.to).map(|(_, c)| *c).collect();
                    (days.get(&d).copied().unwrap_or(0.0), spikes::median(&before))
                }
                None => (0.0, None),
            };
            let spike = flagged.contains(&app);
            AppRow { daily: series(spark, &days), app, total, last_day, median14, spike }
        })
        .filter(|a| a.total.abs() >= 0.005 || a.last_day >= 0.005 || a.spike)
        .collect();
    apps.sort_by(|a, b| b.total.total_cmp(&a.total).then(a.app.cmp(&b.app)));
    let mut shared: BTreeMap<String, f64> = BTreeMap::new();
    for d in s.daily.iter().filter(|d| view.range.contains(d.day)) {
        let name = match (d.service_id == CLOUD_RUN, d.app.is_empty()) {
            (true, false) => continue,
            (true, true) => format!("{} (no service named)", d.service),
            _ => d.service.clone(),
        };
        *shared.entry(name).or_default() += d.cost;
    }
    let mut shared: Vec<SharedRow> = shared
        .into_iter()
        .map(|(service, total)| SharedRow { service, total })
        .filter(|r| r.total.abs() >= 0.005)
        .collect();
    shared.sort_by(|a, b| b.total.total_cmp(&a.total));
    (apps, shared)
}

// ---- spikes and budgets --------------------------------------------------------------------

/// The spikes on `day`: apps first, then shared services as a whole. Refused for a day that is
/// not complete (a partial day would look like a drop, or hide a spike) or whose 14 days
/// before were not read.
pub fn spikes(s: &Snapshot, day: Day, rule: &Rule) -> Result<Vec<Spike>, String> {
    let last = s.last_complete_day().ok_or("no complete day was read")?;
    if day > last {
        return Err(format!(
            "{day} is not complete yet in the billing export (UTC days; the export runs 4 to 12 hours behind): the last complete day is {last}"
        ));
    }
    let need = Range::new(spikes::history(day).from, day);
    if !s.days.is_some_and(|d| d.covers(need)) {
        return Err(format!("the days {} to {} were not read", need.from, need.to));
    }
    let mut out = spikes::detect(&by_day(s, Kind::App), day, Kind::App, rule);
    out.extend(spikes::detect(&by_day(s, Kind::Shared), day, Kind::Shared, rule));
    Ok(out)
}

/// Each budget's spend in its current period (only a current read has them).
pub fn budgets(s: &Snapshot) -> Vec<Spend> {
    let Some(months) = s.months else { return Vec::new() };
    let today = Day::of(s.read_at);
    let mut out: Vec<Spend> = s
        .budgets
        .iter()
        .map(|b| budget::spend(b, &s.account_months, months, today, &s.project_number, s.data_through))
        .collect();
    // The ones that cover this project first.
    out.sort_by_key(|b| !b.covers_project);
    out
}

/// `apps.json`, for the Apps module: each app's month to date, its last complete day (as
/// "yesterday"), its 14-day median and whether that day spiked. Gross, UTC days.
pub fn apps_file(s: &Snapshot, rule: &Rule) -> Value {
    let month = Day::of(s.read_at).month_start();
    let view = month_view(s, month);
    let last = s.last_complete_day();
    let mut apps = serde_json::Map::new();
    if let Some(view) = view {
        for a in self::apps(s, &view, rule).0 {
            apps.insert(
                a.app,
                json!({ "monthToDate": a.total, "yesterday": a.last_day, "median14": a.median14.unwrap_or(0.0), "spike": a.spike }),
            );
        }
    }
    json!({
        "readAt": rfc3339(s.read_at),
        "lastCompleteDay": last.unwrap_or_else(|| Day::of(s.read_at).plus(-1)).to_string(),
        "apps": apps,
    })
}

/// `2026-11-05T20:55:00Z`
pub fn rfc3339(epoch: f64) -> String {
    let (y, mo, d, h, mi, se) = crate::util::utc_parts(epoch as i64);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{se:02}Z")
}

// ---- figures as text -----------------------------------------------------------------------

fn symbol(currency: &str) -> &str {
    match currency {
        "USD" | "" => "US$",
        "EUR" => "€",
        "GBP" => "£",
        c => c,
    }
}

/// `1,234,567` with `digits` decimals.
fn grouped(v: f64, digits: usize) -> String {
    let text = format!("{v:.digits$}");
    let (int, frac) = text.split_once('.').map_or((text.as_str(), ""), |(i, f)| (i, f));
    let mut out = String::new();
    for (i, c) in int.chars().enumerate() {
        if i > 0 && (int.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if frac.is_empty() { out } else { format!("{out}.{frac}") }
}

/// `US$ 4,812`, `US$ 37.54`, `-US$ 2.15`: an amount of the bill.
pub fn money(v: f64, currency: &str) -> String {
    let digits = if v.abs() >= 100.0 { 0 } else { 2 };
    let sign = if v <= -0.005 { "-" } else { "" };
    format!("{sign}{} {}", symbol(currency), grouped(v.abs(), digits))
}

/// `US$ 0.0036`: a small price (a BigQuery read), with two significant digits.
pub fn usd_small(v: f64) -> String {
    if v >= 0.01 || v == 0.0 {
        return format!("US$ {v:.2}");
    }
    let digits = ((-v.log10()).floor() as usize + 2).min(8);
    format!("US$ {v:.digits$}")
}

/// `US$ 2.10`, `US$ 0.519`, `US$ 0.0000167`: a rate, with three significant digits however small.
pub fn rate(v: f64, currency: &str) -> String {
    let a = v.abs();
    let digits = if a >= 1.0 || a == 0.0 {
        2
    } else if a >= 0.01 {
        3
    } else {
        ((-a.log10()).floor() as usize + 3).min(15)
    };
    format!("{} {}", symbol(currency), grouped(a, digits))
}

/// `▲ 14 %`, `▼ 3 %`, `≈ 0 %`: how `now` compares with `before`.
pub fn change(now: f64, before: f64) -> Option<String> {
    (before > 0.0).then(|| {
        let c = (now / before - 1.0) * 100.0;
        if c.abs() < 0.5 {
            "≈ 0 %".to_string()
        } else {
            format!("{} {:.0} %", if c > 0.0 { "▲" } else { "▼" }, c.abs())
        }
    })
}

/// `581k`, `1.2M`, `412T`: a usage amount.
pub fn quantity(v: f64) -> String {
    const SUFFIXES: [&str; 7] = ["", "k", "M", "G", "T", "P", "E"];
    let mut v = v;
    let mut i = 0;
    while v.abs() >= 1000.0 && i + 1 < SUFFIXES.len() {
        v /= 1000.0;
        i += 1;
    }
    let digits = if v.abs() >= 100.0 || (i == 0 && v.fract() == 0.0) { 0 } else { 1 };
    format!("{v:.digits$}{}", SUFFIXES[i])
}

/// A SKU's usage in a unit people read, and its effective rate in that unit: (`120k`,
/// `GiB-seconds`, `US$ 0.0000020 per GiB-second`), the rate empty when nothing was used.
pub fn sku_usage(k: &SkuCost, currency: &str) -> (String, String, String) {
    let (amount, unit) = readable_unit(k.amount, &k.unit);
    let per = if amount > 0.0 {
        format!("{} per {}", rate(k.cost / amount, currency), singular(&unit))
    } else {
        String::new()
    };
    (quantity(amount), unit, per)
}

/// A usage amount in a unit people read: the export counts memory and traffic in bytes
/// (`byte-seconds`), which become GiB (`GiB-seconds`).
fn readable_unit(amount: f64, unit: &str) -> (f64, String) {
    const GIB: f64 = (1u64 << 30) as f64;
    match unit.strip_prefix("byte") {
        Some("s") => (amount / GIB, "GiB".into()),
        Some(rest) if rest.starts_with('-') => (amount / GIB, format!("GiB{rest}")),
        _ => (amount, unit.to_string()),
    }
}

/// `second` for `seconds`: the unit one item of a rate is priced in.
fn singular(unit: &str) -> &str {
    unit.strip_suffix('s').filter(|u| !u.is_empty()).unwrap_or(unit)
}

/// `629 MB`, `1.2 GB`: bytes as BigQuery prices them (decimal units).
pub fn bytes(n: u64) -> String {
    let v = n as f64;
    if v >= 1e12 {
        format!("{:.1} TB", v / 1e12)
    } else if v >= 1e9 {
        format!("{:.1} GB", v / 1e9)
    } else {
        format!("{:.0} MB", v / 1e6)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::tests::{NOW, day};
    use super::super::{DayCost, Kind as ReadKind, MonthCost};
    use super::*;
    use crate::gcp::budgets::{Amount, Budget, Filter, Money};

    const RULE: Rule = Rule { ratio: 2.0, min_usd: 1.0, new_min_usd: 2.0 };

    fn cost(day_: &str, service_id: &str, service: &str, app: &str, cost: f64, credits: f64) -> DayCost {
        DayCost {
            day: day(day_),
            service_id: service_id.into(),
            service: service.into(),
            app: app.into(),
            cost,
            credits,
        }
    }

    /// A project read on 2026-11-05 20:55 UTC, the export through 16:00: October and November,
    /// two apps (orders spikes on Nov 4), Cloud SQL shared, a Cloud Run row with no service.
    pub(crate) fn snapshot() -> Snapshot {
        let mut daily = Vec::new();
        let mut d = day("2026-10-01");
        while d <= day("2026-11-05") {
            let s = d.to_string();
            let orders = if s == "2026-11-04" { 3.0 } else { 1.0 };
            daily.push(cost(&s, CLOUD_RUN, "Cloud Run", "orders", orders, -0.5));
            if d.since(day("2026-10-01")) % 2 == 0 {
                daily.push(cost(&s, CLOUD_RUN, "Cloud Run", "api", 0.5, 0.0));
            }
            daily.push(cost(&s, "9662-B51E-5089", "Cloud SQL", "", 2.0, -2.0));
            d = d.plus(1);
        }
        daily.push(cost("2026-10-10", CLOUD_RUN, "Cloud Run", "", 0.25, 0.0));
        Snapshot {
            env: "platform".into(),
            project: "my-platform-123456".into(),
            project_number: "123456789012".into(),
            account: "012345-6789AB-CDEF01".into(),
            read_at: NOW,
            currency: "USD".into(),
            kind: ReadKind::Current,
            days: Some(Range::new(day("2026-10-01"), day("2026-11-05"))),
            months: Some(Range::new(day("2026-10-01"), day("2026-11-05"))),
            data_through: day("2026-11-05").start() + 16.0 * 3600.0,
            daily,
            account_months: vec![
                MonthCost {
                    month: "2026-11".into(),
                    project_number: "123456789012".into(),
                    service_id: CLOUD_RUN.into(),
                    amount: 20.0,
                    ..Default::default()
                },
                MonthCost {
                    month: "2026-11".into(),
                    project_number: "123456789012".into(),
                    service_id: CLOUD_RUN.into(),
                    credit_type: "PROMOTION".into(),
                    credit_name: "trial".into(),
                    amount: -15.0,
                },
                MonthCost {
                    month: "2026-11".into(),
                    project_number: "999".into(),
                    service_id: "MAPS".into(),
                    amount: 100.0,
                    ..Default::default()
                },
                MonthCost {
                    month: "2026-10".into(),
                    project_number: "999".into(),
                    service_id: "MAPS".into(),
                    amount: 300.0,
                    ..Default::default()
                },
            ],
            budgets: vec![Budget {
                display_name: "platform gross".into(),
                budget_filter: Filter {
                    projects: vec!["projects/123456789012".into()],
                    credit_types_treatment: "EXCLUDE_ALL_CREDITS".into(),
                    ..Default::default()
                },
                amount: Amount {
                    specified_amount: Some(Money { currency_code: "USD".into(), units: "100".into(), nanos: 0 }),
                    ..Default::default()
                },
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_month_against_the_same_complete_days() {
        let s = snapshot();
        let o = overview(&s, day("2026-11-01")).unwrap();
        assert_eq!(o.complete, Some(Range::new(day("2026-11-01"), day("2026-11-04"))));
        assert_eq!(o.previous, Some(Range::new(day("2026-10-01"), day("2026-10-04"))));
        // Nov 1–4: orders 1+1+1+3, api on Nov 2 and 4 (every other day from Oct 1), SQL 4×2.
        assert_eq!(o.complete_total.gross, 6.0 + 1.0 + 8.0);
        assert_eq!(o.complete_total.net, 15.0 - 2.0 - 8.0);
        assert_eq!(o.previous_total.gross, 4.0 + 1.0 + 8.0);
        // Nov 5 is partial: in the total, not in the comparison.
        assert_eq!(o.partial, [PartialDay { day: day("2026-11-05"), gross: 1.0 + 2.0 }]);
        assert_eq!(o.total.gross, 15.0 + 3.0);
        assert_eq!(o.forecast.unwrap().gross, 15.0 / 4.0 * 30.0);
        assert!(o.previous_month.is_some());
        assert_eq!(o.account, Some(Totals { gross: 120.0, net: 105.0 }));
        assert_eq!(o.account_previous, Some(Totals { gross: 300.0, net: 300.0 }));
        assert_eq!(o.credits.len(), 1);
        assert_eq!((o.credits[0].kind.as_str(), o.credits[0].amount), ("PROMOTION", -15.0));
        // October, read in full: no forecast, compared with September (not read).
        let o = overview(&s, day("2026-10-01")).unwrap();
        assert!(o.forecast.is_none() && o.previous_month.is_none());
        assert_eq!(o.complete, Some(Range::new(day("2026-10-01"), day("2026-10-31"))));
    }

    #[test]
    fn services_and_apps_with_what_is_shared() {
        let s = snapshot();
        let view = month_view(&s, day("2026-11-01")).unwrap();
        let rows = services(&s, &view);
        assert_eq!(rows.iter().map(|r| r.service.as_str()).collect::<Vec<_>>(), ["Cloud SQL", "Cloud Run"]);
        let run = &rows[1];
        assert!(run.by_app && !rows[0].by_app);
        assert_eq!((run.now, run.before), (7.0, Some(5.0)));
        assert_eq!(run.daily.len(), SPARK_DAYS as usize);
        assert_eq!(*run.daily.last().unwrap(), 3.5, "the last complete day ends the sparkline");

        let (apps, shared) = apps(&s, &month_view(&s, day("2026-10-01")).unwrap(), &RULE);
        assert_eq!(apps.iter().map(|a| a.app.as_str()).collect::<Vec<_>>(), ["orders", "api"]);
        let orders = &apps[0];
        assert_eq!((orders.total, orders.last_day, orders.median14, orders.spike), (31.0, 3.0, Some(1.0), true));
        // api runs every other day: its median is of those days only.
        assert_eq!(apps[1].median14, Some(0.5));
        assert_eq!(
            shared.iter().map(|r| r.service.as_str()).collect::<Vec<_>>(),
            ["Cloud SQL", "Cloud Run (no service named)"]
        );
        assert_eq!(shared[1].total, 0.25);
    }

    #[test]
    fn spikes_only_on_complete_days_with_their_history() {
        let s = snapshot();
        let found = spikes(&s, day("2026-11-04"), &RULE).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(
            (found[0].name.as_str(), found[0].kind, found[0].cost, found[0].median),
            ("orders", Kind::App, 3.0, Some(1.0))
        );
        assert!(spikes(&s, day("2026-11-03"), &RULE).unwrap().is_empty());
        // Today is partial.
        let e = spikes(&s, day("2026-11-05"), &RULE).unwrap_err();
        assert!(e.contains("not complete") && e.contains("2026-11-04"), "{e}");
        // Not enough days before.
        assert!(spikes(&s, day("2026-10-10"), &RULE).unwrap_err().contains("not read"));
        // A shared service as a whole.
        let mut s = s;
        s.daily.push(cost("2026-11-04", "8B5D-EF7D-EB12", "Cloud Build", "", 6.0, 0.0));
        let found = spikes(&s, day("2026-11-04"), &RULE).unwrap();
        assert_eq!(
            found.iter().map(|x| (x.name.as_str(), x.kind)).collect::<Vec<_>>(),
            [("orders", Kind::App), ("Cloud Build", Kind::Shared)]
        );
    }

    #[test]
    fn budgets_and_the_last_days() {
        let s = snapshot();
        let b = budgets(&s);
        assert_eq!((b[0].name.as_str(), b[0].spent, b[0].amount), ("platform gross", 20.0, Some(100.0)));
        let v = days_view(&s, 7).unwrap();
        assert_eq!(v.range, Range::new(day("2026-10-29"), day("2026-11-04")));
        assert_eq!(v.before, Some(Range::new(day("2026-10-22"), day("2026-10-28"))));
    }

    #[test]
    fn figures_as_text() {
        assert_eq!(money(4812.4, "USD"), "US$ 4,812");
        assert_eq!(money(37.544, "USD"), "US$ 37.54");
        assert_eq!(money(-2.15, ""), "-US$ 2.15");
        assert_eq!(money(-0.001, "USD"), "US$ 0.00");
        assert_eq!(usd_small(0.0036), "US$ 0.0036");
        assert_eq!(usd_small(0.25), "US$ 0.25");
        assert_eq!(rate(0.0000167, "USD"), "US$ 0.0000167");
        assert_eq!(change(110.0, 100.0).as_deref(), Some("▲ 10 %"));
        assert_eq!(change(1.0, 0.0), None);
        assert_eq!((bytes(629_145_600), bytes(1_234_000_000)), ("629 MB".to_string(), "1.2 GB".to_string()));
        assert_eq!([581_000.0, 1_234_567.0, 4.1186e14, 60.0, 0.5].map(quantity), ["581k", "1.2M", "412T", "60", "0.5"]);
        assert_eq!(readable_unit(2.0 * (1u64 << 30) as f64, "byte-seconds"), (2.0, "GiB-seconds".to_string()));
        assert_eq!(readable_unit(3.0 * (1u64 << 30) as f64, "bytes"), (3.0, "GiB".to_string()));
        assert_eq!(readable_unit(5.0, "requests"), (5.0, "requests".to_string()));
        assert_eq!((singular("GiB-seconds"), singular("GiB"), singular("requests")), ("GiB-second", "GiB", "request"));
        assert_eq!(rfc3339(NOW), "2026-11-05T20:55:00Z");
    }
}
