//! Compact time-series charts drawn with block characters. They are lines of text, so they
//! sit inside any panel and scroll with it: bars over a time window ending now, an optional
//! series stacked at the bottom of each bar (errors within requests), and a threshold line
//! (a timeout, a budget) with the bars that cross it in the alert colour.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::theme;
use crate::util::fmt_duration;

/// How the values that fall in one column are combined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Sum,
    Max,
    Mean,
}

/// Lower blocks, one to seven eighths of a cell.
const LOWER: [char; 8] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇'];

/// The values of `points` (time, value) in `n` equal columns of `window`; `None` where a
/// column has no point. With fewer points than columns, each point spans several columns
/// instead of leaving gaps between them.
pub fn columns(points: &[(f64, f64)], (start, end): (f64, f64), n: usize, agg: Agg) -> Vec<Option<f64>> {
    let inside: Vec<(f64, f64)> =
        points.iter().copied().filter(|(t, v)| *t >= start && *t <= end && v.is_finite()).collect();
    let buckets = n.min(inside.len()).max(1);
    let span = (end - start).max(1e-9);
    let mut acc = vec![(0.0f64, 0usize); buckets];
    for (t, v) in inside {
        let (sum, count) = &mut acc[(((t - start) / span * buckets as f64) as usize).min(buckets - 1)];
        *sum = match agg {
            Agg::Max if *count > 0 => sum.max(v),
            Agg::Max => v,
            Agg::Sum | Agg::Mean => *sum + v,
        };
        *count += 1;
    }
    let values: Vec<Option<f64>> = acc
        .into_iter()
        .map(|(sum, count)| (count > 0).then(|| if agg == Agg::Mean { sum / count as f64 } else { sum }))
        .collect();
    (0..n).map(|c| values[c * buckets / n.max(1)]).collect()
}

/// One row of blocks for `values` (`▁` to `█`, blank where there is none), as in the CLI.
pub fn sparkline(values: &[Option<f64>]) -> String {
    sparkline_to(values, values.iter().flatten().copied().fold(0.0f64, f64::max))
}

/// [`sparkline`] on a fixed scale: `max` is a full block (100 for a percentage).
pub fn sparkline_to(values: &[Option<f64>], max: f64) -> String {
    values
        .iter()
        .map(|v| match v {
            None => ' ',
            Some(v) if *v <= 0.0 || max <= 0.0 => '▁',
            Some(v) => ['▂', '▃', '▄', '▅', '▆', '▇', '█'][((v / max * 6.0).round() as usize).min(6)],
        })
        .collect()
}

/// `0`, `7`, `0.4`, `82`, `1.2k`, `48k`, `1.5M`.
pub fn fmt_count(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.1}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else if a >= 10.0 || v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Milliseconds: `212 ms`, `1.8 s`, `30 s`, `2m 05s`.
pub fn fmt_ms(v: f64) -> String {
    if v < 1000.0 {
        format!("{v:.0} ms")
    } else if v < 60_000.0 && v % 1000.0 == 0.0 {
        format!("{:.0} s", v / 1000.0)
    } else if v < 60_000.0 {
        format!("{:.1} s", v / 1000.0)
    } else {
        fmt_duration(v / 1000.0)
    }
}

/// `30m`, `1h`, `24h`, `7d`: the length of a window, for the time axis.
pub fn fmt_window(secs: f64) -> String {
    let s = secs.round() as i64;
    if (s >= 2 * 86_400 && s % 86_400 == 0) || s >= 4 * 86_400 {
        // Whole days, or long enough that hours would only be noise (636h → 27d).
        format!("{}d", (s + 43_200) / 86_400)
    } else if s >= 3600 {
        format!("{}h", (s + 1800) / 3600)
    } else {
        format!("{}m", (s + 30) / 60)
    }
}

/// A chart to draw with [`Chart::lines`].
pub struct Chart<'a> {
    pub title: String,
    /// Shown after the title: totals, the latest value…
    pub summary: Vec<Span<'static>>,
    pub points: &'a [(f64, f64)],
    /// Drawn from the bottom of each bar, on the same scale (errors within invocations).
    pub stacked: Option<&'a [(f64, f64)]>,
    pub agg: Agg,
    /// Start and end (now) of the time window, Unix seconds.
    pub window: (f64, f64),
    /// Height of the bars, in lines.
    pub rows: usize,
    pub color: Color,
    pub stacked_color: Color,
    /// A horizontal line; bars above it turn red. Left out when it is far above the data.
    pub threshold: Option<f64>,
    pub fmt: fn(f64) -> String,
    /// Time labels under the bars.
    pub axis: bool,
}

impl<'a> Chart<'a> {
    pub fn new(title: impl Into<String>, points: &'a [(f64, f64)], window: (f64, f64)) -> Chart<'a> {
        Chart {
            title: title.into(),
            summary: Vec::new(),
            points,
            stacked: None,
            agg: Agg::Sum,
            window,
            rows: 3,
            color: theme::ACCENT,
            stacked_color: theme::ERR,
            threshold: None,
            fmt: fmt_count,
            axis: true,
        }
    }

    /// The threshold, when it is close enough to the data to share its scale.
    fn visible_threshold(&self, data_max: f64) -> Option<f64> {
        self.threshold.filter(|t| *t > 0.0 && (data_max <= 0.0 || *t <= data_max * 1.5))
    }

    /// The chart in `width` columns: a title line, `rows` lines of bars with the scale on
    /// the left, and the time axis.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        const LABEL_W: usize = 7;
        let plot_w = width.saturating_sub(LABEL_W + 1).max(4);
        let values = columns(self.points, self.window, plot_w, self.agg);
        let stacked = self.stacked.map(|s| columns(s, self.window, plot_w, self.agg));
        let data_max = values.iter().flatten().copied().fold(0.0f64, f64::max);
        let threshold = self.visible_threshold(data_max);
        let scale = match threshold {
            Some(t) if t >= data_max => t * 1.15,
            _ => data_max,
        };
        let scale = if scale > 0.0 { scale } else { 1.0 };
        let rows = self.rows.max(1);
        let eighths = |v: f64| if v <= 0.0 { 0 } else { ((v / scale * (rows * 8) as f64).round() as usize).max(1) };
        let threshold_row = threshold.map(|t| ((t / scale * rows as f64).ceil() as usize).clamp(1, rows) - 1);

        let mut out = vec![Line::from(
            [
                vec![Span::styled(self.title.clone(), Style::new().fg(self.color).bold()), Span::raw("  ")],
                self.summary.clone(),
            ]
            .concat(),
        )];
        let alert = Style::new().fg(theme::ERR);
        for r in (0..rows).rev() {
            let (label, label_style) = match (threshold_row == Some(r), r) {
                (true, _) => ((self.fmt)(threshold.unwrap_or_default()), alert),
                (_, r) if r == rows - 1 => ((self.fmt)(data_max), theme::muted()),
                (_, 0) => ("0".to_string(), theme::muted()),
                _ => (String::new(), theme::muted()),
            };
            let tick = if label.is_empty() { '│' } else { '┤' };
            let mut spans = vec![Span::styled(format!("{label:>w$} {tick}", w = LABEL_W - 1), label_style)];
            let (mut text, mut style) = (String::new(), Style::new());
            for (c, v) in values.iter().enumerate() {
                let height = v.map_or(0, eighths);
                let fill = height.saturating_sub(r * 8).min(8);
                let (ch, s) = if fill == 0 {
                    if threshold_row == Some(r) { ('┄', alert) } else { (' ', Style::new()) }
                } else {
                    let ch = if fill == 8 { '█' } else { LOWER[fill] };
                    let under = stacked.as_ref().and_then(|s| s[c]).map_or(0, eighths);
                    let color = if threshold.is_some_and(|t| v.unwrap_or_default() > t) || under > r * 8 + 4 {
                        if under > r * 8 + 4 { self.stacked_color } else { theme::ERR }
                    } else {
                        self.color
                    };
                    (ch, Style::new().fg(color))
                };
                if s != style && !text.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut text), style));
                }
                style = s;
                text.push(ch);
            }
            spans.push(Span::styled(text, style));
            out.push(Line::from(spans));
        }
        if self.axis {
            let span = self.window.1 - self.window.0;
            let mut axis = vec![' '; plot_w];
            let mut put = |at: usize, label: String| {
                let label: Vec<char> = label.chars().collect();
                let at = at.min(plot_w.saturating_sub(label.len()));
                for (i, c) in label.into_iter().enumerate() {
                    if let Some(cell) = axis.get_mut(at + i) {
                        *cell = c;
                    }
                }
            };
            put(0, format!("-{}", fmt_window(span)));
            if plot_w >= 24 {
                let mid = format!("-{}", fmt_window(span / 2.0));
                put((plot_w - mid.len()) / 2, mid);
            }
            put(plot_w, "now".into());
            out.push(Line::styled(
                format!("{} {}", " ".repeat(LABEL_W), axis.into_iter().collect::<String>()),
                theme::muted(),
            ));
        }
        out
    }
}
