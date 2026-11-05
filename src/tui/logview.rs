//! Scrollable log viewer, for build logs and Cloud Logging entries.
//! Shown inside a panel, or zoomed to the whole screen without borders (`z`), where it
//! wraps long lines, searches (`/`, `n`, `N`), jumps between error lines (`e`, `E`) and
//! leaves the mouse to the terminal so text can be selected and copied.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

use super::theme;
use super::widgets::{self, InputEvent, TextInput};
use crate::util::looks_like_error;

/// Lines kept at most; older ones are dropped.
const MAX_LINES: usize = 20_000;

pub enum LogKey {
    /// The key moved, searched or changed the view.
    Handled,
    /// Not a viewer key; the module may use it.
    Ignored,
    /// `y` in the zoomed view: copy every line.
    Copy,
}

#[derive(Default)]
pub struct LogView {
    lines: Vec<String>,
    /// First line shown; `None` follows the end.
    top: Option<usize>,
    search: TextInput,
    searching: bool,
    /// Active search, lower-cased.
    query: String,
    pub zoomed: bool,
    /// Lines that fit the last render, for paging.
    page: usize,
    /// Shown in the zoomed status line (search result, nothing found…).
    note: Option<String>,
}

impl LogView {
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Appends lines, made safe to print: object contents, inputs and logs can carry
    /// terminal escape sequences.
    pub fn extend(&mut self, lines: impl IntoIterator<Item = String>) {
        self.lines.extend(lines.into_iter().map(|l| crate::util::sanitize(&l)));
        if self.lines.len() > MAX_LINES {
            let drop = self.lines.len() - MAX_LINES;
            self.lines.drain(..drop);
            self.top = self.top.map(|t| t.saturating_sub(drop));
        }
    }

    /// True while following the end of the log.
    pub fn following(&self) -> bool {
        self.top.is_none()
    }

    pub fn scroll(&mut self, delta: isize) {
        let last = self.lines.len().saturating_sub(1);
        let current = self.top.unwrap_or_else(|| self.lines.len().saturating_sub(self.page.max(1)));
        let next = (current as isize + delta).clamp(0, last as isize) as usize;
        // Scrolling down past the last page goes back to following.
        self.top = if delta > 0 && next + self.page >= self.lines.len() { None } else { Some(next) };
    }

    fn jump_to(&mut self, line: usize) {
        self.top = Some(line.saturating_sub(3));
    }

    /// Next (or previous) line after the view's top matching `pred`.
    fn find(&mut self, forward: bool, what: &str, pred: impl Fn(&str) -> bool) {
        if self.lines.is_empty() {
            self.note = Some("The log is empty".into());
            return;
        }
        let from = self.top.map_or(self.lines.len(), |t| t + 3);
        let hit = if forward {
            (from + 1..self.lines.len())
                .chain(0..=from.min(self.lines.len().saturating_sub(1)))
                .find(|&i| pred(&self.lines[i]))
        } else {
            (0..from.min(self.lines.len())).rev().chain((from..self.lines.len()).rev()).find(|&i| pred(&self.lines[i]))
        };
        match hit {
            Some(i) => {
                self.jump_to(i);
                self.note = Some(format!("{what} at line {}", i + 1));
            }
            None => self.note = Some(format!("No {what} found")),
        }
    }

    fn find_match(&mut self, forward: bool) {
        if self.query.is_empty() {
            self.note = Some("Press / to search".into());
            return;
        }
        let query = self.query.clone();
        self.find(forward, "match", |l| l.to_lowercase().contains(&query));
    }

    pub fn key(&mut self, key: KeyEvent) -> LogKey {
        if self.searching {
            match self.search.handle(key) {
                InputEvent::Submit => {
                    self.searching = false;
                    self.query = self.search.value.trim().to_lowercase();
                    self.find_match(true);
                }
                InputEvent::Cancel => self.searching = false,
                _ => {}
            }
            return LogKey::Handled;
        }
        let page = self.page.max(1) as isize;
        match key.code {
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::PageDown => self.scroll(page),
            KeyCode::End => self.top = None,
            _ if !self.zoomed => return LogKey::Ignored,
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('z') => self.zoomed = false,
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Home | KeyCode::Char('g') => self.top = Some(0),
            KeyCode::Char('G') => self.top = None,
            KeyCode::Char('/') => {
                self.searching = true;
                self.search.clear();
            }
            KeyCode::Char('n') => self.find_match(true),
            KeyCode::Char('N') => self.find_match(false),
            KeyCode::Char('e') => self.find(true, "error", looks_like_error),
            KeyCode::Char('E') => self.find(false, "error", looks_like_error),
            KeyCode::Char('y') => return LogKey::Copy,
            _ => {}
        }
        LogKey::Handled
    }

    /// A log line with error / warning colours and the search term highlighted.
    fn styled(&self, text: &str) -> Line<'static> {
        let lower = text.to_lowercase();
        let base = if looks_like_error(text) {
            Style::new().fg(theme::ERR)
        } else if lower.contains("warn") {
            Style::new().fg(theme::WARN)
        } else if text.starts_with("[Container]") {
            theme::accent()
        } else {
            Style::new()
        };
        if self.query.is_empty() || !lower.contains(&self.query) || lower.len() != text.len() {
            return Line::styled(text.to_string(), base);
        }
        let hit = base.add_modifier(Modifier::REVERSED);
        let mut spans = Vec::new();
        let mut at = 0;
        while let Some(i) = lower[at..].find(&self.query) {
            let start = at + i;
            spans.push(Span::styled(text[at..start].to_string(), base));
            spans.push(Span::styled(text[start..start + self.query.len()].to_string(), hit));
            at = start + self.query.len();
        }
        spans.push(Span::styled(text[at..].to_string(), base));
        Line::from(spans)
    }

    /// Inside a panel: one row per line, the newest at the bottom unless scrolled.
    pub fn render_panel(&mut self, f: &mut Frame, area: Rect, block: Block, empty: &str) {
        let height = area.height.saturating_sub(2) as usize;
        self.page = height;
        if self.lines.is_empty() {
            f.render_widget(widgets::placeholder(Line::styled(empty.to_string(), theme::muted())).block(block), area);
            return;
        }
        let start = match self.top {
            Some(t) => t.min(self.lines.len().saturating_sub(1)),
            None => self.lines.len().saturating_sub(height),
        };
        let end = (start + height).min(self.lines.len());
        let lines: Vec<Line> = self.lines[start..end].iter().map(|l| self.styled(&format!(" {l}"))).collect();
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    /// Lines scrolled up from the end, for the panel title ("↑ 120 lines").
    pub fn lines_above_end(&self) -> Option<usize> {
        let top = self.top?;
        Some(self.lines.len().saturating_sub(top + self.page))
    }

    /// The whole area, no borders, long lines wrapped; `title` goes in the status line.
    pub fn render_zoomed(&mut self, f: &mut Frame, area: Rect, title: &str) {
        let [body, status] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]));
        let width = body.width.max(1) as usize;
        let rows_of = |l: &String| l.chars().count().max(1).div_ceil(width);
        let height = body.height as usize;

        // Following: start where the last lines fill the screen.
        let start = match self.top {
            Some(t) => t.min(self.lines.len().saturating_sub(1)),
            None => {
                let mut used = 0;
                let mut i = self.lines.len();
                while i > 0 && used + rows_of(&self.lines[i - 1]) <= height {
                    used += rows_of(&self.lines[i - 1]);
                    i -= 1;
                }
                i
            }
        };
        let mut rows = 0;
        let mut end = start;
        while end < self.lines.len() && rows < height {
            rows += rows_of(&self.lines[end]);
            end += 1;
        }
        self.page = (end - start).max(1);
        let lines: Vec<Line> = self.lines[start..end].iter().map(|l| self.styled(l)).collect();
        f.render_widget(Paragraph::new(lines).wrap(ratatui::widgets::Wrap { trim: false }), body);

        let position = if self.lines.is_empty() {
            "empty".to_string()
        } else if self.top.is_none() {
            format!("end · {} lines", self.lines.len())
        } else {
            format!("line {}/{}", start + 1, self.lines.len())
        };
        let mut spans = vec![
            Span::styled(format!(" {title} "), theme::accent().add_modifier(Modifier::BOLD)),
            Span::styled(format!("{position}  "), theme::muted()),
        ];
        if self.searching {
            let (line, col) = self.search.line("/", "search");
            let x = status.x + spans.iter().map(|s| s.width() as u16).sum::<u16>() + col;
            spans.extend(line.spans);
            f.set_cursor_position((x, status.y));
        } else {
            if let Some(note) = &self.note {
                spans.push(Span::styled(format!("{note}  "), Style::new().fg(theme::WARN)));
            }
            spans.extend(
                widgets::hints_line(&[
                    ("/", "search"),
                    ("n/N", "next/prev"),
                    ("e/E", "error"),
                    ("y", "copy all"),
                    ("Esc", "back"),
                ])
                .spans,
            );
            spans.push(Span::styled("   mouse: select and copy", theme::muted()));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::new()), status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyModifiers;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    #[test]
    fn finds_errors_and_matches() {
        let mut v = LogView { zoomed: true, page: 5, ..LogView::default() };
        v.extend((0..100).map(|i| match i {
            40 => "Error: boom".to_string(),
            70 => "needle here".to_string(),
            _ => format!("line {i}"),
        }));
        v.top = Some(0);
        v.key(key('e'));
        assert_eq!(v.top, Some(37));
        v.key(key('/'));
        for c in "NEEDLE".chars() {
            v.key(key(c));
        }
        v.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(v.top, Some(67));
        assert!(matches!(v.key(key('y')), LogKey::Copy));
        v.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!v.zoomed);
    }

    #[test]
    fn keeps_a_bounded_number_of_lines() {
        let mut v = LogView::default();
        v.extend((0..MAX_LINES + 10).map(|i| i.to_string()));
        assert_eq!(v.lines().len(), MAX_LINES);
        assert_eq!(v.lines()[0], "10");
    }
}
