//! Drawing the Platform module: the tabs, what is wrong in this one, the list (columns that give
//! way on narrow terminals) and more about the selected row.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, Paragraph, Wrap};

use super::Platform;
use crate::apps::Tone;
use crate::findings::Level;
use crate::infra::Section;
use crate::modules::common::{filter_bar, fit, spinner_line, tone_style};
use crate::tui::module::View;
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::ellipsize;

impl Platform {
    pub(super) fn render_all(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let banner = self.banner(area.width as usize);
        let [tabs, banner_area, bar, list, detail] = area.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(u16::from(banner.is_some())),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(6),
        ]));
        self.list_area = list;
        self.rows_top = list.y + 2;
        f.render_widget(Paragraph::new(self.tabs_line(view)), tabs);
        if let Some(line) = banner {
            f.render_widget(Paragraph::new(line), banner_area);
        }
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        self.render_list(f, list, view);
        self.render_detail(f, detail);
    }

    fn tabs_line(&self, view: &View) -> Line<'static> {
        let mut spans = Vec::new();
        for (i, section) in Section::ALL.iter().enumerate() {
            let state = &self.tabs[i];
            let label = match state.flagged {
                0 => section.title().to_string(),
                n => format!("{} {n}", section.title()),
            };
            let style = if *section == self.tab {
                theme::selected()
            } else {
                match state.found.first().map(|f| f.level) {
                    Some(Level::Bad) => Style::new().fg(theme::ERR),
                    Some(Level::Warn) => Style::new().fg(theme::WARN),
                    None => theme::muted(),
                }
            };
            spans.push(Span::raw(" "));
            spans.push(Span::styled(format!(" {label} "), style));
        }
        if self.reading() {
            spans.push(Span::styled(format!("   {} reading…", theme::spinner(view.tick)), theme::accent()));
        }
        Line::from(spans)
    }

    /// The worst thing in this tab, and how many more.
    fn banner(&self, width: usize) -> Option<Line<'static>> {
        let found = &self.tabs[self.tab_index()].found;
        let first = found.first()?;
        let (mark, color) = if first.level == Level::Bad { ("✗", theme::ERR) } else { ("!", theme::WARN) };
        let more = if found.len() > 1 { format!("  +{} more", found.len() - 1) } else { String::new() };
        let head = format!(" {mark} {}: ", first.subject);
        let room = width.saturating_sub(head.chars().count() + more.chars().count() + 1);
        Some(Line::from(vec![
            Span::styled(head, Style::new().fg(color).bold()),
            Span::styled(ellipsize(&first.what, room), Style::new().fg(color)),
            Span::styled(more, theme::muted()),
        ]))
    }

    fn list_title(&self, view: &View) -> Line<'static> {
        let total = self.table.rows.len();
        let name = self.tab.title();
        let mut title = vec![Span::raw(" ")];
        if self.snapshot.reading(self.tab) {
            title.push(Span::raw(format!("{} {name} · {total}", theme::spinner(view.tick))));
        } else if self.visible.len() < total {
            title.push(Span::raw(format!("{name} · {} of {total}", self.visible.len())));
        } else {
            title.push(Span::raw(format!("{name} · {total}")));
        }
        if self.flagged_only {
            title.push(Span::styled(" · what needs a look", theme::accent()));
        }
        if let Some(u) = self.snapshot.unread().first() {
            title.push(Span::styled(format!(" · could not read {}", u.what), Style::new().fg(theme::WARN)));
        }
        title.push(Span::raw(" "));
        Line::from(title)
    }

    fn empty_text(&self, view: &View) -> Line<'static> {
        if let Some(e) = self.snapshot.failure(self.tab).filter(|e| !e.is_api_off()) {
            return Line::styled(e.short(), Style::new().fg(theme::ERR));
        }
        if self.snapshot.reading(self.tab) {
            return spinner_line(view.tick, "reading…");
        }
        if !self.table.rows.is_empty() {
            return Line::styled("No row matches", theme::muted());
        }
        if let Some(note) = self.snapshot.notes().first() {
            return Line::styled(note.clone(), theme::muted());
        }
        Line::styled(
            match self.tab {
                Section::Limits => "Nothing read",
                Section::Orphans => "None: every backend service, NEG and host rule leads somewhere",
                Section::Exposure => "No Cloud Run services",
                Section::Certificates => "No certificates",
                Section::Images => "No repositories",
            },
            theme::muted(),
        )
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let title = self.list_title(view);
        if self.visible.is_empty() {
            f.render_widget(widgets::placeholder(self.empty_text(view)).block(widgets::panel(title, true)), area);
            return;
        }
        let cols = &self.table.cols;
        let rows: Vec<&Vec<(String, Tone)>> = self.visible.iter().map(|&i| &self.table.rows[i].cells).collect();
        let width = area.width.saturating_sub(4) as usize;
        let mut widths: Vec<usize> = (0..cols.len())
            .map(|i| rows.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0).min(44))
            .collect();
        let drops: Vec<u8> = cols.iter().map(|c| c.drop).collect();
        let keep = fit(&widths, &drops, width);
        let last = cols.len() - 1;
        let fixed: usize = (0..last).filter(|&i| keep[i]).map(|i| widths[i] + 2).sum();
        widths[last] = width.saturating_sub(fixed);
        if fixed > width {
            // Still too narrow: the name gives way.
            widths[0] = widths[0].saturating_sub(fixed - width).max(8);
        }
        let cells = |row: Vec<(String, Tone)>, head: bool| -> Line<'static> {
            let mut spans = Vec::new();
            for (i, (text, tone)) in row.into_iter().enumerate().filter(|(i, _)| keep[*i]) {
                let w = widths[i];
                let text = ellipsize(&text, w);
                let cell = if cols[i].right { format!("{text:>w$}") } else { format!("{text:<w$}") };
                let style = if head { theme::muted().bold() } else { tone_style(tone) };
                spans.push(Span::styled(cell, style));
                if i != last {
                    spans.push(Span::raw("  "));
                }
            }
            Line::from(spans)
        };
        let header = cells(cols.iter().map(|c| (c.head.to_string(), Tone::Plain)).collect(), true);
        let items: Vec<ListItem> = rows.iter().map(|r| ListItem::new(cells((*r).clone(), false))).collect();
        let block = widgets::panel(title, true);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [head, list] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        self.rows_top = list.y;
        f.render_widget(Paragraph::new(header), head);
        f.render_stateful_widget(List::new(items).highlight_style(theme::selected()), list, &mut self.list_state);
    }

    /// More about the selected row.
    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let Some(row) = self.selected() else {
            f.render_widget(widgets::panel(" Details ", false), area);
            return;
        };
        let lines: Vec<Line> = row.detail.iter().map(|l| Line::raw(l.clone())).collect();
        let title = Line::from(vec![Span::raw(format!(" {} ", row.name)).bold()]);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(widgets::panel(title, false)), area);
    }
}
