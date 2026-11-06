//! Status: the first screen, one look at everything. What every area's check finds, read at the
//! same time on a background thread; `Enter` goes to the area. It changes nothing, and costs come
//! from what `gcli costs` kept: BigQuery bills every read, so this screen never starts one.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::common::{LoadState, spinner_line};
use crate::findings::Level as Severity;
use crate::status::{self, Status as Read};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::util::{ellipsize, now_secs};

pub struct Status {
    load: LoadState,
    read: Option<Read>,
    /// The selected area.
    current: usize,
    scroll: u16,
    /// Read again now, or only when the screen is opened for the first time.
    env_name: String,
}

struct Msg(Read);

impl Status {
    pub fn new() -> Status {
        Status { load: LoadState::Idle, read: None, current: 0, scroll: 0, env_name: String::new() }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.load = LoadState::Loading;
        let env = ctx.env();
        let costs = ctx.config().defaults.costs.clone();
        ctx.spawn(move || Msg(status::read(&env.gcp, &env.name, &env.config, &costs, None, now_secs())));
    }

    fn lines(&self, width: usize) -> (Vec<Line<'static>>, u16) {
        let mut lines = Vec::new();
        let mut selected_at = 0;
        let Some(read) = &self.read else { return (lines, 0) };
        for (i, area) in read.areas.iter().enumerate() {
            let (bad, warn) = (area.count(Severity::Bad), area.count(Severity::Warn));
            let (mark, color) = if bad > 0 {
                ("✗", theme::ERR)
            } else if warn > 0 || !area.unread.is_empty() {
                ("!", theme::WARN)
            } else if area.skipped.is_some() {
                ("·", Color::DarkGray)
            } else {
                ("✓", theme::OK)
            };
            let summary = match (&area.skipped, bad, warn) {
                (Some(why), _, _) => why.clone(),
                (_, 0, 0) if area.unread.is_empty() => "nothing to look at".to_string(),
                (_, 0, 0) => "could not read".to_string(),
                (_, b, 0) => format!("{b} need attention"),
                (_, 0, w) => format!("{w} to look at"),
                (_, b, w) => format!("{b} need attention, {w} to look at"),
            };
            if i == self.current {
                selected_at = lines.len() as u16;
            }
            let name = Style::new().bold();
            let name = if i == self.current { name.reversed() } else { name };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::new().fg(color)),
                Span::styled(format!("{:<10}", area.name), name),
                Span::raw(format!(" {summary}")),
            ]));
            for f in &area.found {
                let c = if f.level == Severity::Bad { theme::ERR } else { theme::WARN };
                let text = format!("{}  {}", ellipsize(&f.subject, 36), f.what);
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(ellipsize(&text, width.saturating_sub(6).max(20)), Style::new().fg(c)),
                ]));
            }
            for line in &area.unread {
                let text = format!("could not read: {line}");
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(ellipsize(&text, width.saturating_sub(6).max(20)), Style::new().fg(theme::WARN)),
                ]));
            }
        }
        (lines, selected_at)
    }
}

impl Module for Status {
    fn title(&self) -> &'static str {
        "Status"
    }

    fn description(&self) -> &'static str {
        "One look at everything · what every area's check finds"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Status::new();
        self.env_name = ctx.env().name.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let areas = self.read.as_ref().map_or(0, |r| r.areas.len());
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.current = self.current.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.current = (self.current + 1).min(areas.saturating_sub(1)),
            KeyCode::Enter => {
                if let Some(area) = self.read.as_ref().and_then(|r| r.areas.get(self.current)) {
                    ctx.jump(area.name, "");
                }
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        if let Ok(msg) = payload.downcast::<Msg>() {
            let read = msg.0;
            if read.login_required {
                ctx.toast(Level::Warn, "Sign in again: press L");
            }
            self.current = self.current.min(read.areas.len().saturating_sub(1));
            self.read = Some(read);
            self.load = LoadState::Loaded;
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.read.is_none() {
            let line = spinner_line(view.tick, "Reading every area of this environment…");
            f.render_widget(Paragraph::new(line), area);
            return;
        }
        let (mut lines, selected_at) = self.lines(area.width as usize);
        if matches!(self.load, LoadState::Loading) {
            lines.insert(0, spinner_line(view.tick, "Reading again…"));
        }
        let height = area.height.max(1);
        if selected_at < self.scroll {
            self.scroll = selected_at;
        } else if selected_at >= self.scroll + height {
            self.scroll = selected_at + 1 - height;
        }
        f.render_widget(Paragraph::new(lines).scroll((self.scroll, 0)), area);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        vec![("↑↓", "area"), ("Enter", "open it"), ("r", "read again")]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let bad = self.read.as_ref().map_or(0, |r| r.count(Severity::Bad));
        (bad > 0).then(|| (format!("{bad} bad"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
    }
}
