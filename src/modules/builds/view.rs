//! Drawing the Builds module: the list (columns that give way on narrow terminals) with the
//! selected build below it, and a build's detail: its summary, step timeline and log.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::{Builds, Open};
use crate::apps::Tone;
use crate::builds::{self, Failure, LAST_DEPLOY_EXPLAINED, bar, cells, columns, mark, step_state};
use crate::gcp::build::Build;
use crate::modules::common::{LoadState, filter_bar, fit, spinner_line, tone_style};
use crate::tui::module::View;
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_ago, fmt_duration, fmt_utc};

/// Columns of a step's bar, when there is room.
const BAR: usize = 40;

impl Builds {
    /// The row of the first build on screen: filter bar, the failed deploys' line when shown,
    /// top border and the header row come before it.
    pub(super) fn list_top(&self) -> u16 {
        let banner = u16::from(self.deploys().is_some_and(|d| !d.failing.is_empty()));
        self.list_area.y + 3 + banner
    }

    pub(super) fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [list, preview] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(7)]));
        self.list_area = list;
        let banner = self.banner(list.width as usize);
        let [bar_area, banner_area, list_area] = list.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Fill(1),
        ]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search builds");
        f.render_widget(Paragraph::new(bar_line), bar_area);
        if let Some(col) = cursor {
            f.set_cursor_position((bar_area.x + col, bar_area.y));
        }
        if let Some(line) = banner {
            f.render_widget(Paragraph::new(line), banner_area);
        }
        let title = self.list_title(view);

        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading builds…", theme::muted()),
                LoadState::Loaded if !self.builds.is_empty() => Line::styled("No build matches", theme::muted()),
                LoadState::Loaded => {
                    let region = if self.region.is_empty() { "global" } else { self.region.as_str() };
                    Line::styled(format!("No builds in {} ({region})", self.project), theme::muted())
                }
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            self.render_preview(f, preview, view);
            return;
        }

        let cols = columns();
        let table: Vec<Vec<(String, Tone)>> =
            self.visible.iter().map(|&i| cells(&self.row(&self.builds[i]), view.now)).collect();
        let width = list_area.width.saturating_sub(4) as usize;
        let mut widths: Vec<usize> = (0..cols.len())
            .map(|i| table.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0))
            .collect();
        widths[1] = widths[1].min(28);
        widths[6] = widths[6].min(28);
        let drops: Vec<u8> = cols.iter().map(|c| c.drop).collect();
        let keep = fit(&widths, &drops, width);
        let last = cols.len() - 1;
        let fixed: usize = (0..last).filter(|&i| keep[i]).map(|i| widths[i] + 2).sum();
        widths[last] = width.saturating_sub(fixed);
        if fixed > width {
            // Still too narrow: the app's name gives way.
            widths[1] = widths[1].saturating_sub(fixed - width).max(8);
        }
        let render_cells = |row: Vec<(String, Tone)>, head: bool| -> Line<'static> {
            let mut spans = Vec::new();
            for (i, (text, tone)) in row.into_iter().enumerate().filter(|(i, _)| keep[*i]) {
                let w = widths[i];
                let text = ellipsize(&text, w);
                let cell = if cols[i].right { format!("{text:>w$}") } else { format!("{text:<w$}") };
                let s = if head { theme::muted().bold() } else { tone_style(tone) };
                spans.push(Span::styled(cell, s));
                if i != last {
                    spans.push(Span::raw("  "));
                }
            }
            Line::from(spans)
        };
        let header = render_cells(cols.iter().map(|c| (c.head.to_string(), Tone::Plain)).collect(), true);
        let items: Vec<ListItem> = table.into_iter().map(|row| ListItem::new(render_cells(row, false))).collect();
        let block = widgets::panel(title, true);
        let inner = block.inner(list_area);
        f.render_widget(block, list_area);
        let [head, rows] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(header), head);
        f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), rows, &mut self.list_state);
        self.render_preview(f, preview, view);
    }

    /// `✗ LAST DEPLOY FAILED · 3 apps: shop, orders, …`: also those whose build the list does not reach.
    fn banner(&self, width: usize) -> Option<Line<'static>> {
        let failing = &self.deploys()?.failing;
        if failing.is_empty() {
            return None;
        }
        let head =
            format!(" ✗ LAST DEPLOY FAILED · {} app{}: ", failing.len(), if failing.len() == 1 { "" } else { "s" });
        let names: Vec<&str> = failing.iter().map(|f| f.app.as_str()).collect();
        let names = ellipsize(&names.join(", "), width.saturating_sub(head.chars().count() + 1));
        Some(Line::from(vec![
            Span::styled(head, Style::new().fg(theme::ERR).bold()),
            Span::styled(names, Style::new().fg(theme::ERR)),
        ]))
    }

    fn list_title(&self, view: &View) -> Line<'static> {
        let mut title = vec![Span::raw(" ")];
        match &self.load {
            LoadState::Loading => {
                title.push(Span::raw(format!("{} Builds · {}", theme::spinner(view.tick), self.builds.len())))
            }
            _ if self.visible.len() < self.builds.len() => {
                title.push(Span::raw(format!("Builds · {} of {}", self.visible.len(), self.builds.len())))
            }
            _ => title.push(Span::raw(format!("Builds · {}", self.builds.len()))),
        }
        if let Some(app) = &self.app {
            title.push(Span::styled(format!(" of {app}"), theme::accent()));
        }
        if self.failed_only {
            title.push(Span::styled(" · failed only", theme::accent()));
        }
        if let LoadState::Failed(e) = &self.load
            && !self.builds.is_empty()
        {
            title.push(Span::styled(format!(" · ✗ {}", e.short()), Style::new().fg(theme::ERR)));
        }
        match &self.deploys {
            None if !matches!(self.load, LoadState::Idle) => {
                title.push(Span::styled(format!(" · {} last deploys", theme::spinner(view.tick)), theme::muted()))
            }
            Some(Err(e)) => {
                title.push(Span::styled(format!(" · last deploys unread: {}", e.short()), Style::new().fg(theme::WARN)))
            }
            _ => {}
        }
        title.push(Span::raw(" "));
        Line::from(title)
    }

    /// The selected build in a few lines, under the list.
    fn render_preview(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(b) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a build", theme::muted()))
                    .block(widgets::panel(" Build ", false)),
                area,
            );
            return;
        };
        let row = self.row(b);
        let kw = 9;
        let mut lines = source_lines(b, kw);
        if let Some(failing) = row.failing {
            lines.push(widgets::kv("The app", Span::styled(failing.text(), Style::new().fg(theme::ERR)), kw));
        }
        if !b.failure_info.detail.is_empty() && row.failing.is_none() {
            lines.push(widgets::kv("Failure", Span::styled(b.failure_info.detail.clone(), theme::muted()), kw));
        }
        lines.push(steps_summary(b, kw, view.now));
        let name = row.app.map_or_else(|| b.short_id().to_string(), str::to_string);
        let block = widgets::panel(
            Line::from(vec![Span::raw(format!(" {name} ")).bold(), Span::styled("Enter opens ", theme::muted())]),
            false,
        );
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }

    pub(super) fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let convention = self.convention;
        let Some(open) = &mut self.open else { return };
        if open.view.zoomed {
            let name = open.build.as_ref().map_or(open.id.clone(), |b| log_title(b, convention.build_app(b)));
            open.view.render_zoomed(f, area, &name);
            return;
        }
        let Some(b) = open.build.clone() else {
            let text = match &open.error {
                Some(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                None => spinner_line(view.tick, "reading the build…"),
            };
            f.render_widget(widgets::placeholder(text), area);
            return;
        };
        let app = convention.build_app(&b);
        let width = area.width.saturating_sub(4) as usize;
        let lines = detail_lines(&b, open, width, view);
        // The summary as tall as it needs, leaving the log some room.
        let wanted = widgets::wrapped_height(&lines, area.width.saturating_sub(4)) + 2;
        let top = wanted.min(area.height.saturating_sub(6).max(area.height / 2));
        let [summary, log_area] = area.layout(&Layout::vertical([Constraint::Length(top), Constraint::Fill(1)]));
        let (m, tone) = mark(&b.status);
        let mut block = widgets::panel(
            Line::from(format!(" {} · {} ", app.unwrap_or("no app"), b.short_id())).bold(),
            !open.view.zoomed,
        );
        block = block.title_top(
            Line::styled(format!(" {m} {} ", b.status.to_lowercase()), tone_style(tone).bold()).right_aligned(),
        );
        // Too tall for its room: the steps, at the end, matter most.
        let overflow = wanted.saturating_sub(top);
        f.render_widget(
            Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((overflow, 0)).block(block),
            summary,
        );

        let (title, empty) = log_panel(open, view);
        let block = widgets::panel(title, false);
        open.view.render_panel(f, log_area, block, &empty);
    }
}

fn log_title(b: &Build, app: Option<&str>) -> String {
    format!("{} {}", app.unwrap_or("build"), b.short_id())
}

/// The log panel's title, and what it says while it has no lines.
fn log_panel(open: &Open, view: &View) -> (Line<'static>, String) {
    match &open.log {
        None => (Line::from(" Log "), format!("{} reading the log…", theme::spinner(view.tick))),
        Some(Err(e)) => (Line::from(" Log "), e.short()),
        Some(Ok(log)) => {
            let mut title = match open.failure() {
                Some(Failure { first_error: Some(at), said: true, .. }) => {
                    format!(" Log · first error at line {} of {}", at + 1, log.lines.len())
                }
                Some(failure) if failure.first_error.is_some() => {
                    format!(" Log · {} lines · no line says an error: the failing step's end", log.lines.len())
                }
                _ => format!(" Log · {} lines", log.lines.len()),
            };
            if log.cut {
                title.push_str(" · the rest was not read");
            }
            if let Some(below) = open.view.lines_above_end().filter(|n| *n > 0) {
                title.push_str(&format!(" · {below} more below"));
            }
            title.push(' ');
            let empty = "Cloud Logging holds no line of this build: past its retention (30 days by default), or it logged elsewhere (o opens its page).";
            (Line::from(title), empty.to_string())
        }
    }
}

/// Commit, trigger and when, for the preview and the detail.
fn source_lines(b: &Build, kw: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let source: Vec<&str> = [b.commit(), b.branch(), b.repo()].into_iter().filter(|s| !s.is_empty()).collect();
    if !source.is_empty() {
        lines.push(widgets::kv("Source", source.join(" · "), kw));
    }
    if !b.trigger().is_empty() {
        lines.push(widgets::kv("Trigger", b.trigger().to_string(), kw));
    }
    let when = b.created().map(|t| format!("{} · {}", fmt_ago(t), fmt_utc(t))).unwrap_or_default();
    lines.push(widgets::kv("Created", Span::styled(when, theme::muted()), kw));
    lines
}

/// `Steps  3 · ✗ build-or-fail · 2 not run`
fn steps_summary(b: &Build, kw: usize, now: f64) -> Line<'static> {
    let took = b.secs(now).map(|s| format!("ran {}", fmt_duration(s))).unwrap_or_default();
    let mut spans = vec![
        Span::styled(format!("{:<kw$} ", "Steps"), theme::muted()),
        Span::raw(format!("{} · {took}", b.steps.len())),
    ];
    if let Some((_, s)) = b.failing_step() {
        spans.push(Span::styled(format!(" · ✗ {}", s.label()), Style::new().fg(theme::ERR)));
    }
    Line::from(spans)
}

/// The summary, then each step with its place in the build.
fn detail_lines(b: &Build, open: &Open, width: usize, view: &View) -> Vec<Line<'static>> {
    let kw = 9;
    let mut lines = source_lines(b, kw);
    let took = b.secs(view.now).map(fmt_duration).unwrap_or_else(|| "did not start".into());
    lines.push(widgets::kv("Ran", took, kw));
    if !b.status_detail.is_empty() {
        lines.push(widgets::kv("Status", b.status_detail.clone(), kw));
    }
    if !b.failure_info.detail.is_empty() {
        lines.push(widgets::kv(
            "Failure",
            Span::styled(b.failure_info.detail.clone(), Style::new().fg(theme::ERR)),
            kw,
        ));
    }
    match &open.deploy {
        Some(Ok(deploys)) => match deploys.failing.first().filter(|f| f.build.id == b.id) {
            Some(failing) => {
                lines.push(widgets::kv("The app", Span::styled(failing.text(), Style::new().fg(theme::ERR)), kw));
                lines.push(widgets::kv("", Span::styled(LAST_DEPLOY_EXPLAINED, theme::muted()), kw));
            }
            None => {
                let text = builds::since_text(&b.id, deploys);
                lines.push(widgets::kv("The app", Span::styled(text, theme::muted()), kw));
            }
        },
        Some(Err(e)) => lines.push(widgets::kv(
            "The app",
            Span::styled(format!("cannot tell: {}", e.short()), Style::new().fg(theme::WARN)),
            kw,
        )),
        None => {}
    }
    if let Some(e) = &open.error {
        lines.push(widgets::kv("Reading", Span::styled(e.short(), Style::new().fg(theme::WARN)), kw));
    }
    if !b.steps.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(step_lines(b, width, view.now));
    }
    lines
}

fn step_lines(b: &Build, width: usize, now: f64) -> Vec<Line<'static>> {
    let label = b.steps.iter().map(|s| s.label().chars().count()).max().unwrap_or(0).min(32);
    // Mark, number, label and what is said of it leave the rest to the bar.
    let bar_width = width.saturating_sub(label + 28).min(BAR);
    b.steps
        .iter()
        .enumerate()
        .map(|(n, s)| {
            let (m, text, tone) = step_state(s, b, now);
            let style = tone_style(tone);
            let mut spans = vec![
                Span::styled(format!("{m} "), style),
                Span::styled(format!("{n:>2} "), theme::muted()),
                Span::styled(
                    format!("{:<label$}  ", ellipsize(s.label(), label)),
                    if s.failed() { style } else { Style::new() },
                ),
            ];
            if bar_width >= 8 {
                let color = if s.failed() { theme::ERR } else { theme::ACCENT };
                let (left, long) = bar(s, b, bar_width, now).unwrap_or((bar_width, 0));
                spans.push(Span::styled("░".repeat(left), theme::muted()));
                spans.push(Span::styled("█".repeat(long), Style::new().fg(color)));
                spans.push(Span::styled("░".repeat(bar_width - left - long), theme::muted()));
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(text, style));
            Line::from(spans)
        })
        .collect()
}
