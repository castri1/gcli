//! What each budget has spent in its current period. The Budget API returns only a budget's
//! configuration, so its spend is summed from the billing export with the budget's own
//! filters: its projects (by number), its services (by id), and credits counted, left out or
//! counted by type as it says. A budget without projects covers the whole billing account.

use serde::Serialize;

use super::MonthCost;
use super::day::{Day, Range};
use crate::gcp::budgets::{Budget, Credits};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spend {
    pub name: String,
    /// Its current period.
    pub period: Range,
    pub spent: f64,
    /// Fixed, or last period's spend; None when last period was not read.
    pub amount: Option<f64>,
    pub currency: String,
    /// The period's spend at the pace so far (a straight line), once a day of it is read.
    pub forecast: Option<f64>,
    pub thresholds: Vec<Threshold>,
    /// Whether it covers the environment's project.
    pub covers_project: bool,
    /// Whether its spend includes other projects (no project filter, or more than this one): the
    /// MCP server shares those only when started with `--account-costs`.
    pub beyond_project: bool,
    /// What the figure cannot account for (label filters, a period cut mid-month).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Threshold {
    /// 0.9 is 90 %.
    pub percent: f64,
    /// Against the forecast rather than the spend.
    pub forecast: bool,
    /// Spend (or forecast) is at or over it.
    pub crossed: bool,
}

impl Spend {
    /// Spent as a share of the amount.
    pub fn ratio(&self) -> Option<f64> {
        self.amount.filter(|a| *a > 0.0).map(|a| self.spent / a)
    }
}

/// The current period of `b` on `today`, and the one before it.
pub fn periods(b: &Budget, today: Day) -> (Range, Option<Range>) {
    let f = &b.budget_filter;
    if let Some(p) = &f.custom_period {
        let date = |d: &crate::gcp::budgets::Date| Day::parse(&format!("{:04}-{:02}-{:02}", d.year, d.month, d.day));
        let from = date(&p.start_date).unwrap_or(today);
        let to = p.end_date.as_ref().and_then(date).unwrap_or(today.max(from));
        // A custom period happens once: it has no period before it.
        return (Range::new(from, to), None);
    }
    let month = today.month_start();
    let months_back = |start: Day, n: i64| (0..n).fold(start, |d, _| d.previous_month());
    let months_ahead = |start: Day, n: i64| (0..n).fold(start, |d, _| d.next_month());
    let (start, len) = match f.calendar_period.as_str() {
        "QUARTER" => {
            let (_, m, _) = month.ymd();
            (months_back(month, i64::from((m - 1) % 3)), 3)
        }
        "YEAR" => {
            let (_, m, _) = month.ymd();
            (months_back(month, i64::from(m - 1)), 12)
        }
        _ => (month, 1),
    };
    let current = Range::new(start, months_ahead(start, len).plus(-1));
    let before = Range::new(months_back(start, len), start.plus(-1));
    (current, Some(before))
}

/// The months `range` touches (`2026-11`), and whether it starts or ends mid-month.
fn months(range: Range) -> (Vec<String>, bool) {
    let mut keys = Vec::new();
    let mut m = range.from.month_start();
    while m <= range.to {
        keys.push(m.month_key());
        m = m.next_month();
    }
    let partial = range.from != range.from.month_start() || range.to != range.to.month_end();
    (keys, partial)
}

/// What `b` spent over `range` according to `cube` (the account's months); None when the
/// cube does not reach back to its start (`read` is the months read).
fn spent(b: &Budget, cube: &[MonthCost], range: Range, read: Range) -> Option<f64> {
    if range.from.month_start() < read.from.month_start() {
        return None;
    }
    let f = &b.budget_filter;
    let (projects, services) = (f.project_numbers(), f.service_ids());
    let (keys, _) = months(range);
    let credits = f.credits();
    let total = cube
        .iter()
        .filter(|r| keys.contains(&r.month))
        .filter(|r| projects.is_empty() || projects.contains(&r.project_number.as_str()))
        .filter(|r| services.is_empty() || services.contains(&r.service_id.as_str()))
        .filter(|r| match (r.credit_type.as_str(), credits) {
            ("", _) => true,
            (_, Credits::All) => true,
            (_, Credits::None) => false,
            (kind, Credits::Specified) => f.credit_types.iter().any(|t| t == kind),
        })
        .map(|r| r.amount)
        .sum();
    Some(total)
}

/// `b`'s spend on `today`. `cube` covers the months `read`; `project_number` is the
/// environment's; `data_through` (Unix seconds) is how far the export has come, for the pace.
pub fn spend(
    b: &Budget,
    cube: &[MonthCost],
    read: Range,
    today: Day,
    project_number: &str,
    data_through: f64,
) -> Spend {
    let (period, before) = periods(b, today);
    let spent_now = spent(b, cube, period, read).unwrap_or(0.0);
    let amount = b.fixed_amount().or_else(|| before.and_then(|p| spent(b, cube, p, read)));
    let mut notes = Vec::new();
    for what in b.budget_filter.unsupported() {
        notes.push(format!("its {what} filter is not applied here: the spend shown may be higher"));
    }
    if months(period).1 {
        notes.push("its period starts or ends mid-month: counted by whole months".into());
    }
    if amount.is_none() && b.fixed_amount().is_none() {
        notes.push("its amount is last period's spend, which was not read".into());
    }
    // The pace: spend so far over the time the export covers, stretched to the whole period.
    let elapsed = (data_through - period.from.start()) / 86_400.0;
    let forecast = (elapsed >= 1.0 && period.contains(today)).then(|| spent_now * period.days() as f64 / elapsed);
    let thresholds = b
        .threshold_rules
        .iter()
        .map(|t| {
            let forecast_basis = t.spend_basis == "FORECASTED_SPEND";
            let value = if forecast_basis { forecast } else { Some(spent_now) };
            let crossed = matches!((value, amount), (Some(v), Some(a)) if a > 0.0 && v >= t.threshold_percent * a);
            Threshold { percent: t.threshold_percent, forecast: forecast_basis, crossed }
        })
        .collect();
    let projects = b.budget_filter.project_numbers();
    Spend {
        name: b.display_name.clone(),
        period,
        spent: spent_now,
        amount,
        currency: b.currency().to_string(),
        forecast,
        thresholds,
        covers_project: projects.is_empty() || projects.contains(&project_number),
        beyond_project: projects.is_empty() || projects.iter().any(|p| *p != project_number),
        notes,
    }
}

/// The days the account query must reach back to for `budgets` on `today` (their current
/// periods, and the previous one for budgets set to last period's spend).
pub fn earliest(budgets: &[Budget], today: Day) -> Option<Day> {
    budgets
        .iter()
        .map(|b| {
            let (now, before) = periods(b, today);
            match before {
                Some(p) if b.fixed_amount().is_none() => p.from,
                _ => now.from,
            }
        })
        .min()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gcp::budgets::{Amount, CustomPeriod, Date, Filter, Money, ThresholdRule};

    fn day(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    fn row(month: &str, project: &str, service: &str, kind: &str, amount: f64) -> MonthCost {
        MonthCost {
            month: month.into(),
            project_number: project.into(),
            service_id: service.into(),
            credit_type: kind.into(),
            credit_name: if kind.is_empty() { String::new() } else { format!("{kind} credit") },
            amount,
        }
    }

    fn budget(filter: Filter, units: Option<&str>) -> Budget {
        Budget {
            display_name: "b".into(),
            budget_filter: filter,
            amount: Amount {
                specified_amount: units.map(|u| Money { currency_code: "USD".into(), units: u.into(), nanos: 0 }),
                last_period_amount: units.is_none().then(|| serde_json::json!({})),
            },
            threshold_rules: vec![
                ThresholdRule { threshold_percent: 0.5, spend_basis: "CURRENT_SPEND".into() },
                ThresholdRule { threshold_percent: 1.0, spend_basis: "FORECASTED_SPEND".into() },
            ],
            ..Default::default()
        }
    }

    fn cube() -> Vec<MonthCost> {
        vec![
            row("2026-10", "111", "RUN", "", 100.0),
            row("2026-10", "111", "RUN", "PROMOTION", -90.0),
            row("2026-11", "111", "RUN", "", 40.0),
            row("2026-11", "111", "RUN", "PROMOTION", -30.0),
            row("2026-11", "111", "RUN", "DISCOUNT", -2.0),
            row("2026-11", "111", "SQL", "", 20.0),
            row("2026-11", "222", "MAPS", "", 300.0),
            row("2026-11", "222", "MAPS", "PROMOTION", -200.0),
        ]
    }

    fn read() -> Range {
        Range::new(day("2026-10-01"), day("2026-11-05"))
    }

    #[test]
    fn spend_follows_each_filter() {
        let today = day("2026-11-05");
        // Ten days of data: through Nov 11 00:00.
        let through = day("2026-11-11").start();
        let gross = Filter {
            projects: vec!["projects/111".into()],
            credit_types_treatment: "EXCLUDE_ALL_CREDITS".into(),
            ..Default::default()
        };
        let s = spend(&budget(gross, Some("200")), &cube(), read(), today, "111", through);
        assert_eq!((s.spent, s.amount, s.covers_project), (60.0, Some(200.0), true));
        assert_eq!(s.period, Range::new(day("2026-11-01"), day("2026-11-30")));
        assert_eq!(s.forecast, Some(180.0), "60 in 10 days, 30 days");
        assert_eq!(s.thresholds.iter().map(|t| t.crossed).collect::<Vec<_>>(), [false, false]);
        assert_eq!(s.ratio(), Some(0.3));

        // Every credit, one service.
        let run = Filter {
            projects: vec!["projects/111".into()],
            services: vec!["services/RUN".into()],
            ..Default::default()
        };
        assert_eq!(spend(&budget(run, Some("5")), &cube(), read(), today, "111", through).spent, 8.0);

        // Only promotions.
        let promo = Filter {
            projects: vec!["projects/111".into()],
            credit_types_treatment: "INCLUDE_SPECIFIED_CREDITS".into(),
            credit_types: vec!["PROMOTION".into()],
            ..Default::default()
        };
        assert_eq!(spend(&budget(promo, Some("50")), &cube(), read(), today, "111", through).spent, 30.0);

        // No project: the whole account, net; another environment's project is still covered.
        let account = Filter::default();
        let s = spend(&budget(account, Some("100")), &cube(), read(), today, "999", through);
        assert_eq!((s.spent, s.covers_project), (128.0, true));
        assert_eq!(s.thresholds.iter().map(|t| t.crossed).collect::<Vec<_>>(), [true, true]);

        // Another project only.
        let maps = Filter { projects: vec!["projects/222".into()], ..Default::default() };
        let s = spend(&budget(maps, Some("1000")), &cube(), read(), today, "111", through);
        assert_eq!((s.spent, s.covers_project), (100.0, false));
    }

    #[test]
    fn last_period_amounts_and_long_periods() {
        let today = day("2026-11-05");
        // Half a day of November read.
        let through = day("2026-11-01").start() + 43_200.0;
        // Last period's spend as the amount: October, net.
        let filter = Filter { projects: vec!["projects/111".into()], ..Default::default() };
        let s = spend(&budget(filter.clone(), None), &cube(), read(), today, "111", through);
        assert_eq!((s.spent, s.amount), (28.0, Some(10.0)));
        assert_eq!(s.forecast, None, "less than a day read");
        // Not read back far enough: no amount, and it says so.
        let late = Range::new(day("2026-11-01"), today);
        let s = spend(&budget(filter.clone(), None), &cube(), late, today, "111", through);
        assert_eq!(s.amount, None);
        assert!(s.notes.iter().any(|n| n.contains("last period")), "{:?}", s.notes);

        let quarter = Filter { calendar_period: "QUARTER".into(), ..filter.clone() };
        let (now, before) = periods(&budget(quarter.clone(), Some("1")), today);
        assert_eq!((now.from, now.to), (day("2026-10-01"), day("2026-12-31")));
        assert_eq!(before, Some(Range::new(day("2026-07-01"), day("2026-09-30"))));
        assert_eq!(spend(&budget(quarter, Some("1")), &cube(), read(), today, "111", through).spent, 38.0);
        let year = Filter { calendar_period: "YEAR".into(), ..filter.clone() };
        let b = budget(year, None);
        assert_eq!(periods(&b, today).0, Range::new(day("2026-01-01"), day("2026-12-31")));
        assert_eq!(earliest(&[b], today), Some(day("2025-01-01")), "last year, for the amount");
        // Custom periods, cut mid-month: whole months, said.
        let custom = Filter {
            custom_period: Some(CustomPeriod {
                start_date: Date { year: 2026, month: 10, day: 15 },
                end_date: Some(Date { year: 2026, month: 12, day: 31 }),
            }),
            ..filter
        };
        let s = spend(&budget(custom, Some("100")), &cube(), read(), today, "111", through);
        assert_eq!((s.period.from, s.spent), (day("2026-10-15"), 38.0));
        assert!(s.notes.iter().any(|n| n.contains("whole months")), "{:?}", s.notes);
    }
}
