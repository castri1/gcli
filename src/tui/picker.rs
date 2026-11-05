//! Searchable list: type to filter, ↑↓ to move, Enter to choose; optionally several
//! with Space. Used for the module palette and the environment picker, inside the UI or
//! inline in the shell with [`pick`] (`gcli use`).

use std::collections::BTreeSet;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::disable_raw_mode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph};
use ratatui::{Frame, TerminalOptions, Viewport};

use super::theme;
use super::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, matches_filter};

pub struct Item {
    pub label: String,
    pub detail: String,
    /// Label colour (environment colour, for example).
    pub color: Option<Color>,
    /// Marked with ● (the current environment or module).
    pub current: bool,
    /// Listed only once something is typed (the many resources in the `:` palette).
    pub on_search: bool,
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Item {
        Item { label: label.into(), detail: detail.into(), color: None, current: false, on_search: false }
    }

    pub fn color(mut self, color: Color) -> Item {
        self.color = Some(color);
        self
    }

    pub fn current(mut self, current: bool) -> Item {
        self.current = current;
        self
    }

    #[allow(dead_code)] // the palette's search of an environment's resources, with more modules
    pub fn on_search(mut self) -> Item {
        self.on_search = true;
        self
    }
}

pub enum Outcome {
    Pending,
    Cancel,
    /// Indexes into the items, in list order.
    Done(Vec<usize>),
}

pub struct Picker {
    title: String,
    submit: &'static str,
    items: Vec<Item>,
    multi: bool,
    input: TextInput,
    visible: Vec<usize>,
    state: ListState,
    marked: BTreeSet<usize>,
}

impl Picker {
    /// `submit` names the Enter action in the hints ("open", "add"…).
    pub fn new(title: impl Into<String>, submit: &'static str, items: Vec<Item>, multi: bool) -> Picker {
        let mut p = Picker {
            title: title.into(),
            submit,
            items,
            multi,
            input: TextInput::default(),
            visible: Vec::new(),
            state: ListState::default(),
            marked: BTreeSet::new(),
        };
        p.refilter();
        p
    }

    /// Puts the cursor on item `index`.
    pub fn with_cursor(mut self, index: usize) -> Picker {
        if let Some(pos) = self.visible.iter().position(|&i| i == index) {
            self.state.select(Some(pos));
        }
        self
    }

    /// Replaces the items (they arrived while the list was open), keeping what was typed
    /// and the item under the cursor.
    #[allow(dead_code)] // lists that fill while the picker is open (the palette's resources)
    pub fn set_items(&mut self, items: Vec<Item>) {
        let keep = self.cursor().map(|i| (self.items[i].label.clone(), self.items[i].detail.clone()));
        self.items = items;
        self.marked.clear();
        self.refilter();
        if let Some(pos) = keep
            .and_then(|(l, d)| self.visible.iter().position(|&i| self.items[i].label == l && self.items[i].detail == d))
        {
            self.state.select(Some(pos));
        }
    }

    /// Label matches first, then matches on the detail only.
    fn refilter(&mut self) {
        let query = self.input.value.to_lowercase();
        let (mut by_label, mut by_detail) = (Vec::new(), Vec::new());
        for (i, item) in self.items.iter().enumerate() {
            if item.on_search && query.trim().is_empty() {
                continue;
            }
            if matches_filter(&item.label, &query) {
                by_label.push(i);
            } else if matches_filter(&format!("{} {}", item.label, item.detail), &query) {
                by_detail.push(i);
            }
        }
        by_label.append(&mut by_detail);
        self.visible = by_label;
        self.state.select(if self.visible.is_empty() { None } else { Some(0) });
    }

    fn cursor(&self) -> Option<usize> {
        self.state.selected().and_then(|pos| self.visible.get(pos).copied())
    }

    fn step(&mut self, delta: isize) {
        if self.visible.is_empty() {
            return;
        }
        let pos = self.state.selected().unwrap_or(0) as isize + delta;
        self.state.select(Some(pos.clamp(0, self.visible.len() as isize - 1) as usize));
    }

    pub fn handle(&mut self, key: KeyEvent) -> Outcome {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc if !self.input.value.is_empty() => {
                self.input.clear();
                self.refilter();
            }
            KeyCode::Esc => return Outcome::Cancel,
            KeyCode::Char('c') if ctrl => return Outcome::Cancel,
            KeyCode::Enter => {
                if !self.marked.is_empty() {
                    return Outcome::Done(self.marked.iter().copied().collect());
                }
                if let Some(i) = self.cursor() {
                    return Outcome::Done(vec![i]);
                }
            }
            KeyCode::Up => self.step(-1),
            KeyCode::Char('p') if ctrl => self.step(-1),
            KeyCode::Down => self.step(1),
            KeyCode::Char('n') if ctrl => self.step(1),
            KeyCode::PageUp => self.step(-10),
            KeyCode::PageDown => self.step(10),
            KeyCode::Char(' ') | KeyCode::Tab if self.multi => {
                if let Some(i) = self.cursor()
                    && !self.marked.remove(&i)
                {
                    self.marked.insert(i);
                }
                self.step(1);
            }
            KeyCode::Char('a') if ctrl && self.multi => {
                if self.visible.iter().all(|i| self.marked.contains(i)) {
                    self.visible.iter().for_each(|i| {
                        self.marked.remove(i);
                    });
                } else {
                    self.marked.extend(self.visible.iter().copied());
                }
            }
            _ => {
                if self.input.handle(key) == InputEvent::Changed {
                    self.refilter();
                }
            }
        }
        Outcome::Pending
    }

    pub fn paste(&mut self, text: &str) {
        self.input.insert_str(text, false);
        self.refilter();
    }

    /// Height that fits every item, capped at `max`.
    pub fn height(&self, max: u16) -> u16 {
        (self.items.len() as u16 + 5).clamp(7, max.max(7))
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect, color: Color) {
        f.render_widget(Clear, area);
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(color))
            .padding(Padding::horizontal(1))
            .title(Span::styled(format!(" {} ", self.title), Style::new().fg(color).bold()));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let [input_area, rule, list_area, hints_area] = inner.layout(&Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
        ]));

        let (line, col) = self.input.line("› ", "type to search");
        f.render_widget(Paragraph::new(line), input_area);
        let count = match self.marked.len() {
            0 => format!("{}/{}", self.visible.len(), self.items.len()),
            n => format!("{n} marked · {}/{}", self.visible.len(), self.items.len()),
        };
        f.render_widget(Paragraph::new(Line::styled(count, theme::muted()).right_aligned()), input_area);
        f.set_cursor_position((input_area.x + col, input_area.y));
        f.render_widget(Paragraph::new(Line::styled("─".repeat(rule.width as usize), theme::muted())), rule);

        if self.visible.is_empty() {
            f.render_widget(Paragraph::new(Line::styled("No matches", theme::muted())), list_area);
        } else {
            let label_width = self
                .visible
                .iter()
                .map(|&i| self.items[i].label.chars().count())
                .max()
                .unwrap_or(0)
                .min(list_area.width as usize / 2);
            let detail_width = (list_area.width as usize).saturating_sub(label_width + 9);
            let any_current = self.items.iter().any(|i| i.current);
            let items: Vec<ListItem> = self
                .visible
                .iter()
                .map(|&i| {
                    let item = &self.items[i];
                    let mut spans = Vec::new();
                    if self.multi {
                        let (mark, style) = if self.marked.contains(&i) {
                            ("◉ ", Style::new().fg(theme::OK))
                        } else {
                            ("○ ", theme::muted())
                        };
                        spans.push(Span::styled(mark, style));
                    }
                    if any_current {
                        spans.push(Span::styled(if item.current { "● " } else { "  " }, theme::accent()));
                    }
                    let style = item.color.map_or(theme::bold(), |c| Style::new().fg(c).bold());
                    spans.push(Span::styled(format!("{:<label_width$}  ", ellipsize(&item.label, label_width)), style));
                    spans.push(Span::styled(ellipsize(&item.detail, detail_width), theme::muted()));
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(items).highlight_style(theme::selected()).highlight_symbol("▶ ");
            f.render_stateful_widget(list, list_area, &mut self.state);
        }

        let mut hints = vec![("↑↓", "move")];
        if self.multi {
            hints.extend([("Space", "mark"), ("^A", "all")]);
        }
        hints.extend([("Enter", self.submit), ("Esc", "cancel")]);
        f.render_widget(Paragraph::new(widgets::hints_line(&hints)), hints_area);
    }
}

/// Runs a picker inline, below the shell prompt. `None` when cancelled.
pub fn pick(mut picker: Picker, color: Color) -> Result<Option<Vec<usize>>> {
    let height = picker.height(16);
    let mut terminal = ratatui::try_init_with_options(TerminalOptions { viewport: Viewport::Inline(height) })?;
    let result = (|| -> Result<Option<Vec<usize>>> {
        loop {
            terminal.draw(|f| {
                let area = f.area();
                picker.render(f, Rect { width: area.width.min(100), ..area }, color);
            })?;
            if let Event::Key(key) = event::read()?
                && key.kind != KeyEventKind::Release
            {
                match picker.handle(key) {
                    Outcome::Pending => {}
                    Outcome::Cancel => return Ok(None),
                    Outcome::Done(chosen) => return Ok(Some(chosen)),
                }
            }
        }
    })();
    // Erase the picker and put the cursor where it started; no alternate screen to leave.
    let top = terminal.get_frame().area().y;
    let _ = terminal.clear();
    let _ = terminal.set_cursor_position((0, top));
    let _ = terminal.show_cursor();
    let _ = disable_raw_mode();
    result
}
