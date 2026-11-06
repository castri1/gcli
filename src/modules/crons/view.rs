//! Drawing the Crons module: the list (columns that give way on narrow terminals) with the selected
//! job below it, and a job's detail: what is wrong with it, and its recent attempts.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::Crons;
use crate::apps::Tone;
use crate::crons::history::{Attempts, WINDOW_TEXT};
use crate::crons::{
    self, Detail, NEGATIVE_CODE_EXPLAINED, Row, attempt_result, cells, columns, relative, schedule_text,
};
use crate::findings::{Finding, Level};
use crate::gcp::error::redact_tokens;
use crate::modules::common::{LoadState, filter_bar, fit, spinner_line, tone_style};
use crate::tui::module::View;
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_duration, fmt_utc};

/// The width of the labels on the left.
const KW: usize = 9;
/// The widest the list lets a job's name be, and half that for an app's or a schedule.
const NAME: usize = 44;
/// What the notes need to say something: other columns give way before they go under it.
const NOTES_ROOM: usize = 34;

impl Crons {
    /// The row of the first job on screen: filter bar, the failing jobs' line when shown, top
    /// border and the header row come before it.
    pub(super) fn list_top(&self) -> u16 {
        let banner = u16::from(self.listing.failing(crate::util::now_secs()) > 0);
        self.list_area.y + 3 + banner
    }

    pub(super) fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [list, preview] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(7)]));
        self.list_area = list;
        let banner = self.banner(list.width as usize, view.now);
        let [bar_area, banner_area, list_area] = list.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Fill(1),
        ]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search jobs");
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
                LoadState::Loading | LoadState::Idle => Line::styled("Loading jobs…", theme::muted()),
                LoadState::Loaded if !self.listing.rows.is_empty() => Line::styled("No job matches", theme::muted()),
                LoadState::Loaded => {
                    let region = if self.region.is_empty() { "every region" } else { self.region.as_str() };
                    Line::styled(format!("No jobs in {} ({region})", self.project), theme::muted())
                }
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            self.render_preview(f, preview, view);
            return;
        }

        let cols = columns();
        let table: Vec<Vec<(String, Tone)>> =
            self.visible.iter().map(|&i| cells(&self.listing.rows[i], view.now)).collect();
        let width = list_area.width.saturating_sub(4) as usize;
        let mut widths: Vec<usize> = (0..cols.len())
            .map(|i| table.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0))
            .collect();
        widths[1] = widths[1].min(NAME);
        widths[2] = widths[2].min(NAME / 2);
        widths[3] = widths[3].min(NAME / 2);
        let drops: Vec<u8> = cols.iter().map(|c| c.drop).collect();
        let keep = fit(&widths, &drops, width.saturating_sub(NOTES_ROOM));
        let last = cols.len() - 1;
        let fixed: usize = (0..last).filter(|&i| keep[i]).map(|i| widths[i] + 2).sum();
        widths[last] = width.saturating_sub(fixed);
        if fixed > width {
            // Still too narrow: the job's name gives way.
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

    /// `✗ FAILING · 3 jobs: a, b, …`, also those the list does not reach.
    fn banner(&self, width: usize, now: f64) -> Option<Line<'static>> {
        let failing: Vec<&str> =
            self.listing.rows.iter().filter(|r| r.level(now) == Some(Level::Bad)).map(|r| r.job.id()).collect();
        if failing.is_empty() {
            return None;
        }
        let head = format!(" ✗ FAILING · {} job{}: ", failing.len(), if failing.len() == 1 { "" } else { "s" });
        let names = ellipsize(&failing.join(", "), width.saturating_sub(head.chars().count() + 1));
        Some(Line::from(vec![
            Span::styled(head, Style::new().fg(theme::ERR).bold()),
            Span::styled(names, Style::new().fg(theme::ERR)),
        ]))
    }

    fn list_title(&self, view: &View) -> Line<'static> {
        let total = self.listing.rows.len();
        let mut title = vec![Span::raw(" ")];
        match &self.load {
            LoadState::Loading => title.push(Span::raw(format!("{} Crons · {total}", theme::spinner(view.tick)))),
            _ if self.visible.len() < total => {
                title.push(Span::raw(format!("Crons · {} of {total}", self.visible.len())))
            }
            _ => title.push(Span::raw(format!("Crons · {total}"))),
        }
        if let Some(app) = &self.app {
            title.push(Span::styled(format!(" of {app}"), theme::accent()));
        }
        if self.failed_only {
            title.push(Span::styled(" · failing only", theme::accent()));
        }
        if let LoadState::Failed(e) = &self.load
            && total > 0
        {
            title.push(Span::styled(format!(" · ✗ {}", e.short()), Style::new().fg(theme::ERR)));
        }
        if self.reading_history() {
            title.push(Span::styled(format!(" · {} attempts", theme::spinner(view.tick)), theme::muted()));
        }
        if let Some(e) = self.unread() {
            title.push(Span::styled(format!(" · not all read: {}", e.short()), Style::new().fg(theme::WARN)));
        }
        title.push(Span::raw(" "));
        Line::from(title)
    }

    /// The selected job in a few lines, under the list.
    fn render_preview(&self, f: &mut Frame, area: Rect, view: &View) {
        let Some(row) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a job", theme::muted()))
                    .block(widgets::panel(" Job ", false)),
                area,
            );
            return;
        };
        let mut lines = target_lines(row);
        lines.extend(row.findings(view.now).iter().flat_map(finding_lines));
        let name = row.job.id();
        let block = widgets::panel(
            Line::from(vec![Span::raw(format!(" {name} ")).bold(), Span::styled("Enter opens ", theme::muted())]),
            false,
        );
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }

    pub(super) fn render_detail(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(open) = &mut self.open else { return };
        let detail = match &open.detail {
            Some(Ok(detail)) => detail,
            Some(Err(e)) => {
                let text = Line::styled(e.short(), Style::new().fg(theme::ERR));
                f.render_widget(widgets::placeholder(text), area);
                return;
            }
            None => {
                f.render_widget(widgets::placeholder(spinner_line(view.tick, "reading the job…")), area);
                return;
            }
        };
        let lines = detail_lines(detail, view.now);
        // The summary as tall as it needs, leaving the attempts some room.
        let wanted = widgets::wrapped_height(&lines, area.width.saturating_sub(4)) + 2;
        let top = wanted.min(area.height.saturating_sub(6).max(area.height / 2));
        let [summary, attempts_area] = area.layout(&Layout::vertical([Constraint::Length(top), Constraint::Fill(1)]));
        let (result, tone) = crons::result(&detail.row.job);
        let block = widgets::panel(Line::from(format!(" {} ", detail.row.job.id())).bold(), true)
            .title_top(Line::styled(format!(" {result} "), tone_style(tone).bold()).right_aligned());
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), summary);

        let (title, lines) = attempts_lines(&detail.attempts, view.now);
        open.lines = u16::try_from(lines.len()).unwrap_or(u16::MAX);
        let block = widgets::panel(title, false);
        f.render_widget(Paragraph::new(Text::from(lines)).scroll((open.scroll, 0)).block(block), attempts_area);
    }
}

/// What it calls, when it runs and when it ran.
fn target_lines(row: &Row) -> Vec<Line<'static>> {
    let job = &row.job;
    let mut lines = Vec::new();
    if let Some(url) = job.url() {
        let token = job.auth_kind().unwrap_or("none");
        lines.push(widgets::kv("Target", format!("{} {}  token: {token}", job.target_kind(), redact_tokens(&url)), KW));
    } else if !job.target_kind().is_empty() {
        lines.push(widgets::kv("Target", job.target_kind(), KW));
    }
    let timeout = row.timeout.map(|t| format!(" · the service's request timeout {t:.0}s")).unwrap_or_default();
    lines.push(widgets::kv(
        "Schedule",
        format!("{} · deadline {}{timeout}", schedule_text(job), job.attempt_deadline),
        KW,
    ));
    lines
}

fn finding_lines(f: &Finding) -> Vec<Line<'static>> {
    let color = if f.level == Level::Bad { theme::ERR } else { theme::WARN };
    let mut lines = vec![widgets::kv("Finding", Span::styled(f.what.clone(), Style::new().fg(color)), KW)];
    if let Some(hint) = &f.hint {
        lines.push(widgets::kv("Likely", Span::styled(hint.clone(), theme::muted()), KW));
    }
    lines
}

/// What the detail says of a job.
fn detail_lines(d: &Detail, now: f64) -> Vec<Line<'static>> {
    let row = &d.row;
    let job = &row.job;
    let mut lines = Vec::new();
    if !job.description.is_empty() {
        lines.push(widgets::kv("About", redact_tokens(&job.description), KW));
    }
    let app = match (row.app(), row.live_app()) {
        (Some(app), Some(_)) => Span::raw(app.to_string()),
        (Some(app), None) => {
            Span::styled(format!("{app} (no Cloud Run service by that name)"), Style::new().fg(theme::ERR))
        }
        _ => Span::styled("none known", theme::muted()),
    };
    lines.push(widgets::kv("App", app, KW));
    lines.extend(target_lines(row));
    let when = |t: f64| format!("{} · {}", fmt_utc(t), relative(t, now));
    lines.push(widgets::kv("Last run", job.last_attempt().map_or("never".to_string(), when), KW));
    lines.push(widgets::kv("Next run", job.next_run().map_or("—".to_string(), when), KW));
    if let Some(retry) = job.retry_config.as_ref().filter(|r| r.retry_count > 0) {
        lines.push(widgets::kv("Retries", retry.retry_count.to_string(), KW));
    }
    if let Some(t) = job.updated() {
        lines.push(widgets::kv("Changed", Span::styled(when(t), theme::muted()), KW));
    }
    if !job.status.message.is_empty() {
        lines.push(widgets::kv("Status", redact_tokens(&job.status.message), KW));
    }
    if let Some(code) = job.status.code.filter(|c| *c < 0) {
        let text = format!("code {code}: {NEGATIVE_CODE_EXPLAINED}");
        lines.push(widgets::kv("Status", Span::styled(text, theme::muted()), KW));
    }
    if let Some(last) = d.last_success {
        lines.push(widgets::kv("Last OK", last.text(now), KW));
    }
    if let Some(e) = &d.services_error {
        let text = format!("cannot tell its app: {}", e.short());
        lines.push(widgets::kv("Reading", Span::styled(text, Style::new().fg(theme::WARN)), KW));
    }
    lines.extend(row.findings(now).iter().flat_map(finding_lines));
    lines
}

/// The attempts panel's title and lines.
fn attempts_lines(attempts: &crate::gcp::Result<Attempts>, now: f64) -> (Line<'static>, Vec<Line<'static>>) {
    let read = match attempts {
        Ok(read) => read,
        Err(e) => {
            let text = Line::styled(e.short(), Style::new().fg(theme::ERR));
            return (Line::from(format!(" Attempts · {WINDOW_TEXT} ")), vec![text]);
        }
    };
    let cut = if read.cut { " · older ones not read" } else { "" };
    let title = Line::from(format!(" Attempts · {WINDOW_TEXT} · {}{cut} ", read.list.len()));
    if read.list.is_empty() {
        return (title, vec![Line::styled(" None in the logs.", theme::muted())]);
    }
    let width = read.list.iter().map(|a| attempt_result(a).chars().count()).max().unwrap_or(0);
    let lines = read
        .list
        .iter()
        .map(|a| {
            let tone = if a.running {
                Tone::Warn
            } else if a.failed {
                Tone::Bad
            } else {
                Tone::Plain
            };
            let mark = if a.running {
                "●"
            } else if a.failed {
                "✗"
            } else {
                "✓"
            };
            let took = a.took.map(|t| format!("took {}", fmt_duration(t))).unwrap_or_default();
            let detail = if a.failed { redact_tokens(&a.detail) } else { String::new() };
            Line::from(vec![
                Span::styled(format!("{mark} "), tone_style(tone)),
                Span::styled(format!("{}  {:>8}  ", fmt_utc(a.at), relative(a.at, now)), theme::muted()),
                Span::styled(format!("{:<width$}  ", attempt_result(a)), tone_style(tone)),
                Span::styled(format!("{:<9}", a.answer()), tone_style(tone)),
                Span::styled(format!("{took:<14}"), theme::muted()),
                Span::styled(detail, theme::muted()),
            ])
        })
        .collect();
    (title, lines)
}
