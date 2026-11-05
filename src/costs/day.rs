//! UTC days, the billing export's unit (`DATE(usage_start_time)` is a UTC date): every day,
//! month and comparison in Costs is in UTC, whatever the local time zone.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::util::{parse_rfc3339, utc_parts};

const DAY: f64 = 86_400.0;

/// A UTC day, as days since 1970-01-01.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(i64);

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

impl Day {
    /// The day `epoch` (Unix seconds) falls on.
    pub fn of(epoch: f64) -> Day {
        Day((epoch / DAY).floor() as i64)
    }

    /// `2026-11-03`.
    pub fn parse(text: &str) -> Option<Day> {
        let t = text.trim();
        if t.len() != 10 {
            return None;
        }
        let day = Day::of(parse_rfc3339(&format!("{t}T00:00:00Z"))?);
        // `2026-02-30` would roll over into March: refused instead.
        (day.to_string() == t).then_some(day)
    }

    /// `2026-11`: the first day of that month.
    pub fn parse_month(text: &str) -> Option<Day> {
        Day::parse(&format!("{}-01", text.trim()))
    }

    /// Unix seconds at 00:00 UTC.
    pub fn start(self) -> f64 {
        self.0 as f64 * DAY
    }

    pub fn ymd(self) -> (i64, u32, u32) {
        let (y, m, d, ..) = utc_parts(self.0 * 86_400);
        (y, m, d)
    }

    pub fn plus(self, days: i64) -> Day {
        Day(self.0 + days)
    }

    /// Days from `earlier` to this one.
    pub fn since(self, earlier: Day) -> i64 {
        self.0 - earlier.0
    }

    pub fn month_start(self) -> Day {
        let (_, _, d) = self.ymd();
        self.plus(1 - i64::from(d))
    }

    /// The first day of the next month.
    pub fn next_month(self) -> Day {
        let start = self.month_start();
        start.plus(32).month_start()
    }

    /// The first day of the previous month.
    pub fn previous_month(self) -> Day {
        self.month_start().plus(-1).month_start()
    }

    pub fn month_end(self) -> Day {
        self.next_month().plus(-1)
    }

    pub fn days_in_month(self) -> i64 {
        self.next_month().since(self.month_start())
    }

    /// `2026-11`
    pub fn month_key(self) -> String {
        let (y, m, _) = self.ymd();
        format!("{y:04}-{m:02}")
    }

    /// `November 2026`
    pub fn month_name(self) -> String {
        let (y, m, _) = self.ymd();
        format!("{} {y}", MONTHS[(m as usize).clamp(1, 12) - 1])
    }

    /// `Nov 3`
    pub fn short(self) -> String {
        let (_, m, d) = self.ymd();
        format!("{} {d}", &MONTHS[(m as usize).clamp(1, 12) - 1][..3])
    }
}

impl fmt::Display for Day {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = self.ymd();
        write!(f, "{y:04}-{m:02}-{d:02}")
    }
}

impl Serialize for Day {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Day {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Day, D::Error> {
        let text = String::deserialize(d)?;
        Day::parse(&text).ok_or_else(|| serde::de::Error::custom(format!("not a day: {text}")))
    }
}

/// Days `from` to `to`, both included.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub from: Day,
    pub to: Day,
}

impl Range {
    pub fn new(from: Day, to: Day) -> Range {
        Range { from, to }
    }

    pub fn contains(&self, day: Day) -> bool {
        day >= self.from && day <= self.to
    }

    /// Whether `other` lies entirely inside.
    pub fn covers(&self, other: Range) -> bool {
        other.from >= self.from && other.to <= self.to
    }

    pub fn days(&self) -> i64 {
        (self.to.since(self.from) + 1).max(0)
    }

    /// `Nov 1–4`, `Oct 30 – Nov 3`, `Nov 3`.
    pub fn label(&self) -> String {
        let (from, to) = (self.from.ymd(), self.to.ymd());
        if self.from == self.to {
            self.from.short()
        } else if (from.0, from.1) == (to.0, to.1) {
            format!("{}–{}", self.from.short(), to.2)
        } else {
            format!("{} – {}", self.from.short(), self.to.short())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_and_months() {
        let d = Day::parse("2026-11-03").unwrap();
        assert_eq!(d.to_string(), "2026-11-03");
        assert_eq!(Day::of(d.start() + 86_399.0), d);
        assert_eq!(
            (d.month_start().to_string(), d.month_end().to_string()),
            ("2026-11-01".into(), "2026-11-30".into())
        );
        assert_eq!(d.previous_month().to_string(), "2026-10-01");
        assert_eq!(d.next_month().to_string(), "2026-12-01");
        assert_eq!(Day::parse("2026-01-31").unwrap().previous_month().to_string(), "2025-12-01");
        assert_eq!(Day::parse("2026-12-31").unwrap().next_month().to_string(), "2027-01-01");
        assert_eq!(Day::parse("2028-02-10").unwrap().days_in_month(), 29);
        assert_eq!(
            (d.month_key(), d.month_name(), d.short()),
            ("2026-11".into(), "November 2026".into(), "Nov 3".into())
        );
        assert_eq!(Day::parse_month("2026-10"), Day::parse("2026-10-01"));
        for bad in ["2026-02-30", "2026-13-01", "nope", "2026-1-01", ""] {
            assert!(Day::parse(bad).is_none(), "{bad}");
        }
        assert_eq!(serde_json::to_string(&d).unwrap(), "\"2026-11-03\"");
        assert_eq!(serde_json::from_str::<Day>("\"2026-11-03\"").unwrap(), d);
    }

    #[test]
    fn ranges() {
        let day = |s| Day::parse(s).unwrap();
        let r = Range::new(day("2026-11-01"), day("2026-11-04"));
        assert_eq!((r.days(), r.label()), (4, "Nov 1–4".to_string()));
        assert!(r.contains(day("2026-11-04")) && !r.contains(day("2026-11-05")));
        assert!(r.covers(Range::new(day("2026-11-02"), day("2026-11-03"))));
        assert!(!r.covers(Range::new(day("2026-10-31"), day("2026-11-03"))));
        assert_eq!(Range::new(day("2026-10-30"), day("2026-11-03")).label(), "Oct 30 – Nov 3");
    }
}
