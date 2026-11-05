//! Cost spikes: a day an app (or a shared service, as a whole) cost clearly more than usual.
//!
//! Day D, a complete UTC day, is flagged when its cost is over `spikeRatio` × the median of the
//! 14 days before it and over `spikeMinUsd`; an app with no cost in those 14 days is flagged
//! when it is over `newAppMinUsd`. Pure functions: the callers pick the day (never a partial
//! one) and hand over each name's costs by day.
//!
//! Only the days a name has rows count towards its median. A Cloud Run service that scales to
//! zero has no rows on the days nobody used it, and counting those as zero would flag every
//! Monday after a quiet weekend. The median of an even count is the lower of the two middle
//! values (as BigQuery's `PERCENTILE_DISC(0.5)`): a day that really happened, not an average.

use std::collections::BTreeMap;

use serde::Serialize;

use super::day::{Day, Range};
use crate::config::CostDefaults;

/// Days before D that make its median.
pub const WINDOW: i64 = 14;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rule {
    pub ratio: f64,
    pub min_usd: f64,
    pub new_min_usd: f64,
}

impl Rule {
    pub fn of(c: &CostDefaults) -> Rule {
        Rule { ratio: c.spike_ratio, min_usd: c.spike_min_usd, new_min_usd: c.new_app_min_usd }
    }

    /// `> 2× the median of the 14 days before and > US$ 1.00; with no history, > US$ 2.00`
    pub fn describe(&self) -> String {
        format!(
            "more than {}× the median of the {WINDOW} days before and more than US$ {:.2}; with no cost in those days, more than US$ {:.2}",
            trim(self.ratio),
            self.min_usd,
            self.new_min_usd
        )
    }
}

fn trim(v: f64) -> String {
    let s = format!("{v:.2}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// A Cloud Run service.
    App,
    /// A service of the project as a whole (Cloud SQL, Cloud Build…), shared by the apps.
    Shared,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Spike {
    pub name: String,
    pub kind: Kind,
    pub day: Day,
    pub cost: f64,
    /// None: no cost in the 14 days before (a new app).
    pub median: Option<f64>,
    /// Days of the 14 before with a cost.
    pub history_days: usize,
}

impl Spike {
    /// `2.5× its 14-day median of US$ 0.40`, `new: no cost in the 14 days before`.
    pub fn reason(&self) -> String {
        match self.median {
            Some(m) if m > 0.0 => format!("{:.1}× its 14-day median of US$ {m:.2}", self.cost / m),
            Some(m) => format!("its 14-day median is US$ {m:.2}"),
            None => format!("new: no cost in the {WINDOW} days before"),
        }
    }
}

/// The middle of `values`, the lower one for an even count; None when empty.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    Some(sorted[(sorted.len() - 1) / 2])
}

/// The days that make day `day`'s median.
pub fn history(day: Day) -> Range {
    Range::new(day.plus(-WINDOW), day.plus(-1))
}

/// Whether `cost` on a day is a spike against `history` (the costs of the days before it on
/// which there was one). Returns the median it was compared with (None: no history).
pub fn flags(cost: f64, history: &[f64], rule: &Rule) -> Option<Option<f64>> {
    match median(history) {
        Some(m) => (cost > rule.ratio * m && cost > rule.min_usd).then_some(Some(m)),
        None => (cost > rule.new_min_usd).then_some(None),
    }
}

/// The spikes on `day` among `series` (name → cost by day, days without cost left out),
/// biggest first.
pub fn detect(series: &BTreeMap<String, BTreeMap<Day, f64>>, day: Day, kind: Kind, rule: &Rule) -> Vec<Spike> {
    let window = history(day);
    let mut out: Vec<Spike> = series
        .iter()
        .filter_map(|(name, days)| {
            let cost = days.get(&day).copied().unwrap_or(0.0);
            let before: Vec<f64> = days.range(window.from..=window.to).map(|(_, c)| *c).collect();
            let median = flags(cost, &before, rule)?;
            Some(Spike { name: name.clone(), kind, day, cost, median, history_days: before.len() })
        })
        .collect();
    out.sort_by(|a, b| b.cost.total_cmp(&a.cost));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const RULE: Rule = Rule { ratio: 2.0, min_usd: 1.0, new_min_usd: 2.0 };

    fn day(s: &str) -> Day {
        Day::parse(s).unwrap()
    }

    #[test]
    fn the_median_is_a_real_day() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[5.0, 1.0, 3.0]), Some(3.0));
        // Even: the lower middle value, not the mean of the two.
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.0));
        assert_eq!(median(&[f64::NAN, 2.0]), Some(2.0));
    }

    #[test]
    fn thresholds() {
        let usual = [0.6; 14];
        assert_eq!(flags(1.5, &usual, &RULE), Some(Some(0.6)));
        assert_eq!(flags(1.1, &usual, &RULE), None, "not over twice the median");
        // Twice the median but under the minimum: noise.
        assert_eq!(flags(0.9, &[0.3; 14], &RULE), None);
        // Exactly at a threshold is not over it.
        assert_eq!(flags(2.0, &[1.0], &RULE), None);
        assert_eq!(flags(1.0, &[0.1], &RULE), None);
        // New: no history, compared with newAppMinUsd only.
        assert_eq!(flags(2.5, &[], &RULE), Some(None));
        assert_eq!(flags(1.5, &[], &RULE), None);
    }

    #[test]
    fn detects_per_name_with_only_the_days_it_ran() {
        let d = day("2026-11-03");
        let mut series: BTreeMap<String, BTreeMap<Day, f64>> = BTreeMap::new();
        // Every day of the window, lower median 0.50.
        let steady: Vec<f64> = vec![2.1, 2.05, 1.95, 0.9, 0.15, 0.12, 0.5, 0.45, 0.8, 0.55, 0.7, 0.14, 0.11, 0.16];
        series
            .insert("orders".into(), (0..14).map(|i| (d.plus(i - 14), steady[i as usize])).chain([(d, 1.4)]).collect());
        // Idle on weekends: no rows then. Zeros would bring the median down to 0.49 and flag it.
        let weekdays = [
            (-14, 0.52),
            (-13, 0.61),
            (-12, 0.49),
            (-11, 1.02),
            (-8, 0.57),
            (-7, 0.95),
            (-6, 0.9),
            (-5, 0.66),
            (-4, 0.47),
        ];
        series.insert("weekdays".into(), weekdays.iter().map(|(o, c)| (d.plus(*o), *c)).chain([(d, 1.15)]).collect());
        // New, and costly enough.
        series.insert("fresh".into(), [(d, 2.4)].into_iter().collect());
        // New but cheap; and an app that ran long ago only (outside the window): new too.
        series.insert("cheap".into(), [(d, 1.9)].into_iter().collect());
        series.insert("back".into(), [(d.plus(-20), 5.0), (d, 3.0)].into_iter().collect());
        // Nothing on the day.
        series.insert("idle".into(), [(d.plus(-1), 9.0)].into_iter().collect());
        let spikes = detect(&series, d, Kind::App, &RULE);
        let names: Vec<&str> = spikes.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["back", "fresh", "orders"]);
        let orders = &spikes[2];
        assert_eq!((orders.median, orders.history_days, orders.day), (Some(0.5), 14, d));
        assert_eq!(orders.reason(), "2.8× its 14-day median of US$ 0.50");
        assert_eq!((spikes[1].median, spikes[1].history_days), (None, 0));
        assert!(spikes[1].reason().starts_with("new"));
        // The next day: nothing.
        assert!(detect(&series, d.plus(1), Kind::App, &RULE).is_empty());
        assert_eq!(history(d), Range::new(day("2026-10-20"), day("2026-11-02")));
    }
}
