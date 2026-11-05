//! Apps: the environment's Cloud Run services, with their latest ready revision, when and by
//! whom they last changed, and their scaling. M2 makes it the full Apps module (traffic,
//! health, revisions, secrets, restart).

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

use super::common::{LoadState, filter_bar, matches_filter};
use crate::gcp::{GcpError, run};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{self, InputEvent, TextInput};
use crate::util::{ellipsize, fmt_ago, fmt_utc};

enum Msg {
    Page(Vec<run::Service>),
    Done,
    Failed(GcpError),
}

pub struct Apps {
    all: Vec<run::Service>,
    load: LoadState,
    filter: TextInput,
    filtering: bool,
    visible: Vec<usize>,
    list_state: ListState,
    project: String,
    /// Empty: every region.
    region: String,
    list_area: Rect,
}

impl Apps {
    pub fn new() -> Apps {
        Apps {
            all: Vec::new(),
            load: LoadState::Idle,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            project: String::new(),
            region: String::new(),
            list_area: Rect::default(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.all.clear();
        self.visible.clear();
        self.load = LoadState::Loading;
        let gcp = ctx.env().gcp.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut gone = false;
            let result = run::list_services(&gcp, |page| {
                gone = !em.emit(Msg::Page(page));
                !gone
            });
            if !gone {
                em.emit(match result {
                    Ok(()) => Msg::Done,
                    Err(e) => Msg::Failed(e),
                });
            }
        });
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.name.clone());
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.all.len())
            .filter(|&i| {
                let s = &self.all[i];
                matches_filter(&format!("{} {} {}", s.id(), s.revision(), s.last_modifier), &query)
            })
            .collect();
        visible.sort_by(|&a, &b| self.all[a].id().cmp(self.all[b].id()));
        self.visible = visible;
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.all[i].name == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&run::Service> {
        self.all.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn select(&mut self, index: usize) {
        if !self.visible.is_empty() {
            self.list_state.select(Some(index.min(self.visible.len() - 1)));
        }
    }

    /// Rows a page key moves: the list's height.
    fn page(&self) -> usize {
        self.list_area.height.saturating_sub(4).max(1) as usize
    }

    fn render_list(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.list_area = area;
        let [bar, list_area] = area.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        let (bar_line, cursor) = filter_bar(&self.filter, self.filtering, "type / to search services");
        f.render_widget(Paragraph::new(bar_line), bar);
        if let Some(col) = cursor {
            f.set_cursor_position((bar.x + col, bar.y));
        }
        let title = match &self.load {
            LoadState::Loading => format!(" {} Cloud Run services · {} ", theme::spinner(view.tick), self.all.len()),
            // Some pages came, then a failure: say so above what is shown.
            LoadState::Failed(e) if !self.all.is_empty() => {
                format!(" Cloud Run services · {} · ✗ {} ", self.all.len(), e.short())
            }
            _ if self.visible.len() < self.all.len() => {
                format!(" Cloud Run services · {} of {} ", self.visible.len(), self.all.len())
            }
            _ => format!(" Cloud Run services · {} ", self.all.len()),
        };
        if self.visible.is_empty() {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => Line::styled("Loading services…", theme::muted()),
                LoadState::Loaded if !self.all.is_empty() => Line::styled("No service matches", theme::muted()),
                LoadState::Loaded => {
                    let region = if self.region.is_empty() { "any region" } else { self.region.as_str() };
                    Line::styled(format!("No Cloud Run services in {} ({region})", self.project), theme::muted())
                }
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(title, true)), list_area);
            return;
        }

        // Columns: service, revision, scaling, updated, by (whatever room is left).
        let width = list_area.width.saturating_sub(4) as usize;
        let id_w = self.visible.iter().map(|&i| self.all[i].id().chars().count()).max().unwrap_or(0).clamp(8, 36);
        let rev_w =
            self.visible.iter().map(|&i| self.all[i].revision().chars().count()).max().unwrap_or(0).clamp(8, 32);
        let fixed = id_w + 2 + rev_w + 2 + 9 + 2 + 9 + 2;
        let by_w = width.saturating_sub(fixed);
        let header = format!(
            "{:<id_w$}  {:<rev_w$}  {:>9}  {:>9}  {}",
            "SERVICE",
            "REVISION",
            "MIN/MAX",
            "UPDATED",
            if by_w >= 8 { "BY" } else { "" }
        );
        let items: Vec<ListItem> = self
            .visible
            .iter()
            .map(|&i| {
                let s = &self.all[i];
                let scaling =
                    format!("{}/{}", s.min_instances(), s.max_instances().map_or("-".into(), |m| m.to_string()));
                let updated = s.updated().map(fmt_ago).unwrap_or_default();
                let mut spans = vec![
                    Span::raw(format!("{:<id_w$}  ", ellipsize(s.id(), id_w))),
                    Span::styled(format!("{:<rev_w$}  ", ellipsize(s.revision(), rev_w)), theme::muted()),
                    Span::styled(
                        format!("{scaling:>9}  "),
                        if s.min_instances() > 0 { Style::new().fg(theme::WARN) } else { Style::new() },
                    ),
                    Span::styled(format!("{updated:>9}  "), theme::muted()),
                ];
                if by_w >= 8 {
                    spans.push(Span::styled(ellipsize(&s.last_modifier, by_w), theme::muted()));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        let block = widgets::panel(title, true);
        let inner = block.inner(list_area);
        f.render_widget(block, list_area);
        let [head, rows] = inner.layout(&Layout::vertical([Constraint::Length(1), Constraint::Fill(1)]));
        f.render_widget(Paragraph::new(Line::styled(header, theme::muted().bold())), head);
        let list = List::new(items).highlight_style(theme::selected());
        f.render_stateful_widget(list, rows, &mut self.list_state);
    }

    fn render_detail(&self, f: &mut Frame, area: Rect) {
        let Some(s) = self.selected() else {
            f.render_widget(
                widgets::placeholder(Line::styled("Select a service", theme::muted()))
                    .block(widgets::panel(" Service ", false)),
                area,
            );
            return;
        };
        let kw = 9;
        let dash = || "—".to_string();
        let updated = s.updated().map(|t| format!("{} · {}", fmt_ago(t), fmt_utc(t))).unwrap_or_else(dash);
        let scaling = format!(
            "min {} · max {}",
            s.min_instances(),
            s.max_instances().map_or("default".into(), |m| m.to_string())
        );
        let lines = vec![
            widgets::kv(
                "URL",
                Span::styled(if s.uri.is_empty() { dash() } else { s.uri.clone() }, theme::accent()),
                kw,
            ),
            widgets::kv(
                "Revision",
                if s.revision().is_empty() { "none ready".to_string() } else { s.revision().to_string() },
                kw,
            ),
            widgets::kv("Updated", updated, kw),
            widgets::kv("By", if s.last_modifier.is_empty() { dash() } else { s.last_modifier.clone() }, kw),
            widgets::kv("Scaling", scaling, kw),
            widgets::kv("Region", s.region().to_string(), kw),
        ];
        let block = widgets::panel(Line::from(format!(" {} ", s.id())).bold(), false);
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block), area);
    }
}

fn copy(ctx: &mut Ctx, text: &str, done: &str) {
    let ok = crate::platform::copy_to_clipboard(text);
    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { done } else { "Clipboard not available" });
}

impl Module for Apps {
    fn title(&self) -> &'static str {
        "Apps"
    }

    fn description(&self) -> &'static str {
        "Cloud Run · services, latest revision, last change, scaling"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Apps::new();
        let env = ctx.env();
        self.project = env.project.clone();
        self.region = env.region.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let selected = self.list_state.selected().unwrap_or(0);
        if self.filtering {
            match self.filter.handle(key) {
                InputEvent::Changed => self.refilter(),
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.refilter();
                }
                InputEvent::Ignored => match key.code {
                    KeyCode::Up => self.select(selected.saturating_sub(1)),
                    KeyCode::Down => self.select(selected + 1),
                    _ => {}
                },
                InputEvent::Moved => {}
            }
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.select(selected.saturating_sub(self.page())),
            KeyCode::PageDown => self.select(selected + self.page()),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.refilter();
            }
            KeyCode::Char('y') => {
                if let Some(uri) = self.selected().map(|s| s.uri.clone()).filter(|u| !u.is_empty()) {
                    copy(ctx, &uri, "URL copied");
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.selected().map(|s| s.console_url(&self.project))
                    && !crate::platform::open_url(&url)
                {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                // Filter bar, top border and the header row come before the first service.
                let row = event.row.saturating_sub(self.list_area.y + 3) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, _ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(page) => {
                let first = self.all.is_empty();
                self.all.extend(page);
                self.refilter();
                if first {
                    self.select(0);
                }
            }
            Msg::Done => self.load = LoadState::Loaded,
            Msg::Failed(e) => self.load = LoadState::Failed(e),
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let [list, detail] = area.layout(&Layout::vertical([Constraint::Fill(1), Constraint::Length(8)]));
        self.render_list(f, list, view);
        self.render_detail(f, detail);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![("↑↓", "move"), ("/", "search"), ("y", "copy URL"), ("o", "console"), ("r", "reload")]
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
    }

    fn capturing(&self) -> bool {
        self.filtering
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }
}
