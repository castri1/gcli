//! Database: a project's Cloud SQL instances, from the outside. What needs attention, connections
//! and CPU over the last day, connections per database, the rest of the instance's health and what
//! it is. Every read runs on a background thread, one for each instance, so the first answers show
//! as they come. It changes nothing and never connects to a database.

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseEvent, MouseEventKind};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Paragraph, Wrap};

use super::common::{LoadState, fit, spinner_line, tone_style};
use crate::apps::Tone;
use crate::db::{self, Database as Row, Health, LOAD_WARN, Metrics};
use crate::findings::{self, Finding, Level as Severity};
use crate::gcp::sql::{self, Instance};
use crate::gcp::{self, console_url};
use crate::tui::chart::{Agg, Chart};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::{ellipsize, fmt_ago, now_secs};

/// Width of a line's key.
const KEY: usize = 12;
/// Columns of a sparkline.
const SPARK: usize = 24;
/// Rows of the charts.
const CONNECTION_ROWS: usize = 4;
const CPU_ROWS: usize = 3;

/// Each message carries the number of the read it belongs to: a later read wins over one still coming.
enum Msg {
    Instances(u64, gcp::Result<Vec<Instance>>),
    Health(u64, Box<Health>),
}

/// An instance, and what the last read said of it.
struct Entry {
    instance: Instance,
    health: Option<Health>,
    found: Vec<Finding>,
    reading: bool,
}

pub struct Database {
    entries: Vec<Entry>,
    load: LoadState,
    /// Reads started, the last one current.
    reads: u64,
    current: usize,
    scroll: u16,
    /// Where the instance is drawn, for page keys.
    body: Rect,
    project: String,
}

impl Database {
    pub fn new() -> Database {
        Database {
            entries: Vec::new(),
            load: LoadState::Idle,
            reads: 0,
            current: 0,
            scroll: 0,
            body: Rect::default(),
            project: String::new(),
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        self.reads += 1;
        self.load = LoadState::Loading;
        let (n, gcp) = (self.reads, ctx.env().gcp.clone());
        ctx.spawn(move || Msg::Instances(n, sql::list(&gcp)));
    }

    /// The list came: what was read of each instance stays on screen while it is read again.
    fn on_instances(&mut self, n: u64, list: Vec<Instance>, ctx: &mut Ctx) {
        let keep = self.entries.get(self.current).map(|e| e.instance.name.clone());
        let mut old = std::mem::take(&mut self.entries);
        for instance in list {
            let was = old.iter().position(|e| e.instance.name == instance.name).map(|i| old.swap_remove(i));
            let (health, found) = was.map_or((None, Vec::new()), |e| (e.health, e.found));
            let (gcp, one) = (ctx.env().gcp.clone(), instance.clone());
            ctx.spawn(move || Msg::Health(n, Box::new(db::read(&gcp, one, now_secs()))));
            self.entries.push(Entry { instance, health, found, reading: true });
        }
        self.current = keep.and_then(|k| self.entries.iter().position(|e| e.instance.name == k)).unwrap_or(0);
    }

    fn entry(&self) -> Option<&Entry> {
        self.entries.get(self.current)
    }

    fn select(&mut self, index: usize) {
        if index < self.entries.len() && index != self.current {
            self.current = index;
            self.scroll = 0;
        }
    }

    fn scroll_by(&mut self, delta: i32) {
        self.scroll = self.scroll.saturating_add_signed(delta.clamp(-1000, 1000) as i16);
    }

    fn page(&self) -> i32 {
        i32::from(self.body.height.saturating_sub(1).max(1))
    }

    fn bad_count(&self) -> usize {
        self.entries.iter().flat_map(|e| &e.found).filter(|f| f.level == Severity::Bad).count()
    }

    /// The instances, the selected one marked: only when there are several.
    fn tabs(&self) -> Option<Line<'static>> {
        if self.entries.len() < 2 {
            return None;
        }
        let mut spans = Vec::new();
        for (i, e) in self.entries.iter().enumerate() {
            let bad = e.found.iter().any(|f| f.level == Severity::Bad);
            let style = match (i == self.current, bad) {
                (true, _) => theme::accent().bold(),
                (false, true) => Style::new().fg(theme::ERR),
                (false, false) => theme::muted(),
            };
            spans.push(Span::styled(format!("{}  ", e.instance.name), style));
        }
        spans.push(Span::styled("←→", theme::muted()));
        Some(Line::from(spans))
    }

    fn draw(&mut self, f: &mut Frame, area: Rect, view: &View) {
        let Some(e) = self.entry() else {
            let text = match &self.load {
                LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
                LoadState::Loading | LoadState::Idle => spinner_line(view.tick, "reading the instances…"),
                LoadState::Loaded => {
                    Line::styled(format!("No Cloud SQL instances in {}", self.project), theme::muted())
                }
            };
            f.render_widget(widgets::placeholder(text).block(widgets::panel(" Database ", true)), area);
            return;
        };
        let i = &e.instance;
        let mut title = vec![Span::raw(format!(" {} ", i.name)).bold()];
        if let LoadState::Failed(e) = &self.load {
            title.push(Span::styled(format!("✗ {} ", e.short()), Style::new().fg(theme::ERR)));
        } else if let Some(h) = &e.health {
            title.push(Span::styled(format!("read {} ", fmt_ago(h.read_at)), theme::muted()));
        }
        let mut block = widgets::panel(Line::from(title), true);
        let state = if i.stopped() { "stopped" } else { i.state.as_str() };
        let healthy = i.is_runnable() && !i.stopped();
        block = block.title_top(
            Line::styled(format!(" ● {state} "), Style::new().fg(if healthy { theme::OK } else { theme::ERR }).bold())
                .right_aligned(),
        );
        if e.reading {
            block = block.title_top(spinner_line(view.tick, "reading ").right_aligned());
        }
        let inner = block.inner(area);
        f.render_widget(block, area);
        let tabs = self.tabs();
        let [tabs_area, body] =
            inner.layout(&Layout::vertical([Constraint::Length(u16::from(tabs.is_some())), Constraint::Fill(1)]));
        if let Some(line) = tabs {
            f.render_widget(Paragraph::new(line), tabs_area);
        }
        let lines = self.entry().map_or_else(Vec::new, |e| instance_lines(e, body.width as usize, view));
        // Scrolling stops at the end.
        let height = widgets::wrapped_height(&lines, body.width) as usize;
        let max = height.saturating_sub(body.height as usize) as u16;
        self.scroll = self.scroll.min(max);
        self.body = body;
        f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).scroll((self.scroll, 0)), body);
    }
}

fn finding_lines(found: &[Finding]) -> Vec<Line<'static>> {
    let mut lines = vec![widgets::section("Needs attention")];
    for f in found {
        let (mark, color) = match f.level {
            Severity::Bad => ("✗", theme::ERR),
            Severity::Warn => ("!", theme::WARN),
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::new().fg(color).bold()),
            Span::styled(f.what.clone(), Style::new().fg(color)),
        ]));
        if let Some(hint) = &f.hint {
            lines.push(Line::styled(format!("  {hint}"), theme::muted()));
        }
    }
    lines.push(Line::raw(""));
    lines
}

fn fmt_percent(v: f64) -> String {
    format!("{v:.0}%")
}

fn chart_lines(h: &Health, m: &Metrics, width: usize) -> Vec<Line<'static>> {
    let gauges = db::gauges(h, SPARK);
    let summary = |key: &str| -> Vec<Span<'static>> {
        gauges
            .iter()
            .find(|g| g.key == key)
            .map(|g| {
                let style = if matches!(g.tone, Tone::Plain) { theme::muted() } else { tone_style(g.tone) };
                vec![Span::styled(g.text.clone(), style)]
            })
            .unwrap_or_default()
    };
    let mut lines = Vec::new();
    if h.instance.is_postgres() {
        let mut chart = Chart::new("Connections", &m.connections, m.window);
        chart.agg = Agg::Max;
        chart.rows = CONNECTION_ROWS;
        chart.threshold = h.instance.max_connections();
        chart.summary = summary("Connections");
        lines.extend(chart.lines(width));
    }
    let cpu: Vec<(f64, f64)> = m.cpu.iter().map(|(t, v)| (*t, v * 100.0)).collect();
    let mut chart = Chart::new("CPU", &cpu, m.window);
    chart.agg = Agg::Mean;
    chart.rows = CPU_ROWS;
    chart.threshold = Some(LOAD_WARN * 100.0);
    chart.fmt = fmt_percent;
    chart.summary = summary("CPU");
    lines.extend(chart.lines(width));
    lines.push(Line::raw(""));
    for g in gauges.into_iter().filter(|g| g.key != "Connections" && g.key != "CPU") {
        let mut spans =
            vec![Span::styled(format!("{:<KEY$} ", g.key), theme::muted()), Span::styled(g.text, tone_style(g.tone))];
        if let Some(spark) = g.spark {
            spans.push(Span::styled(format!("   {spark}"), theme::accent()));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// Connections per database: the name gives way first on narrow terminals, then the extra columns.
fn table_lines(rows: &[Row], known: bool, width: usize) -> Vec<Line<'static>> {
    let cols = db::columns();
    let table: Vec<Vec<(String, Tone)>> = rows.iter().map(|d| db::cells(d, known)).collect();
    let mut widths: Vec<usize> = (0..cols.len())
        .map(|i| table.iter().map(|r| r[i].0.chars().count()).chain([cols[i].head.len()]).max().unwrap_or(0))
        .collect();
    widths[0] = widths[0].min(40);
    let drops: Vec<u8> = cols.iter().map(|c| c.drop).collect();
    let keep = fit(&widths, &drops, width);
    let others: usize = (1..cols.len()).filter(|&i| keep[i]).map(|i| widths[i] + 2).sum();
    widths[0] = widths[0].min(width.saturating_sub(others)).max(8);
    let draw = |row: Vec<(String, Tone)>, head: bool| -> Line<'static> {
        let mut spans = Vec::new();
        for (i, (text, tone)) in row.into_iter().enumerate().filter(|(i, _)| keep[*i]) {
            let w = widths[i];
            let text = ellipsize(&text, w);
            let cell = if cols[i].right { format!("{text:>w$}") } else { format!("{text:<w$}") };
            let style = if head { theme::muted().bold() } else { tone_style(tone) };
            spans.push(Span::styled(cell, style));
            spans.push(Span::raw("  "));
        }
        Line::from(spans)
    };
    let mut lines = vec![draw(cols.iter().map(|c| (c.head.to_string(), Tone::Plain)).collect(), true)];
    lines.extend(table.into_iter().map(|row| draw(row, false)));
    lines
}

fn instance_lines(e: &Entry, width: usize, view: &View) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !e.found.is_empty() {
        lines.extend(finding_lines(&e.found));
    }
    match &e.health {
        None => {
            lines.push(spinner_line(view.tick, "reading the metrics and the backups…"));
            lines.push(Line::raw(""));
        }
        Some(h) => {
            for line in h.unread().into_iter().chain(h.notes()) {
                lines.push(Line::styled(format!("! {line}"), Style::new().fg(theme::WARN)));
            }
            if let Some(Ok(m)) = &h.metrics {
                lines.extend(chart_lines(h, m, width));
                lines.push(Line::raw(""));
                if !m.databases.is_empty() {
                    lines.push(widgets::section("Connections by database"));
                    let (busy, quiet) = m.busy_and_quiet();
                    lines.extend(table_lines(busy, m.states_at.is_some(), width));
                    if let Some(line) = db::quiet_line(quiet) {
                        lines.push(Line::styled(line, theme::muted()));
                    }
                    lines.push(Line::styled(db::DATABASES_EXPLAINED, theme::muted()));
                    lines.push(Line::raw(""));
                }
            } else {
                // No metrics (a stopped instance, or none could be read): the rest of what is known.
                for g in db::gauges(h, SPARK) {
                    lines.push(widgets::kv(g.key, Span::styled(g.text, tone_style(g.tone)), KEY));
                }
                lines.push(Line::raw(""));
            }
        }
    }
    lines.push(widgets::section("Instance"));
    for fact in db::facts(&e.instance) {
        lines.push(widgets::kv(fact.key, Span::styled(fact.text, tone_style(fact.tone)), KEY));
    }
    lines
}

fn copy(ctx: &mut Ctx, text: &str, done: &str) {
    let ok = crate::platform::copy_to_clipboard(text);
    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { done } else { "Clipboard not available" });
}

impl Module for Database {
    fn title(&self) -> &'static str {
        "Database"
    }

    fn description(&self) -> &'static str {
        "Cloud SQL · connections against the limit, per database, disk, backups"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Database::new();
        self.project = ctx.env().project.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.scroll_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll_by(1),
            KeyCode::PageUp => self.scroll_by(-self.page()),
            KeyCode::PageDown => self.scroll_by(self.page()),
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = u16::MAX,
            KeyCode::Left => self.select(self.current.saturating_sub(1)),
            KeyCode::Right => self.select(self.current + 1),
            KeyCode::Char('y') => {
                if let Some(name) = self.entry().map(|e| e.instance.connection_name.clone()) {
                    copy(ctx, &name, "Connection name copied");
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.entry().map(|e| console_url(&self.project, &e.instance.console_path()))
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
        match event.kind {
            MouseEventKind::ScrollUp => self.scroll_by(-3),
            MouseEventKind::ScrollDown => self.scroll_by(3),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Instances(n, _) | Msg::Health(n, _) if n != self.reads => {}
            Msg::Instances(n, Ok(list)) => {
                self.load = LoadState::Loaded;
                self.on_instances(n, list, ctx);
            }
            Msg::Instances(_, Err(e)) => self.load = LoadState::Failed(e),
            Msg::Health(_, health) => {
                if let Some(e) = self.entries.iter_mut().find(|e| e.instance.name == health.instance.name) {
                    e.found = health.findings();
                    findings::sort(&mut e.found);
                    e.instance = health.instance.clone();
                    e.health = Some(*health);
                    e.reading = false;
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.draw(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let mut hints = vec![("↑↓", "scroll")];
        if self.entries.len() > 1 {
            hints.push(("←→", "instance"));
        }
        hints.extend([("y", "copy connection name"), ("o", "console"), ("r", "reload")]);
        hints
    }

    fn badge(&self) -> Option<(String, Color)> {
        let bad = self.bad_count();
        (bad > 0).then(|| (format!("{bad} bad"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading) || self.entries.iter().any(|e| e.reading)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Environment, Loaded, Source};
    use crate::db::tests::Scene;
    use crate::gcp::Gcp;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::tui::module::{AppEvent, Env, Shared};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    fn screen(m: &mut Database, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            m.render(f, area, &View { tick: 0, now: crate::db::tests::now() });
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The module in a shell of its own, against a fake Google Cloud.
    struct Harness {
        module: Database,
        shared: Shared,
        rx: Receiver<AppEvent>,
    }

    impl Harness {
        fn new(fake: &MockServer) -> Harness {
            let env = Environment {
                project: "my-platform-123456".into(),
                region: "us-east1".into(),
                ..Environment::default()
            };
            let mut config = Config::default();
            config.environments.insert("platform".into(), env.clone());
            let gcp =
                Gcp::with("my-platform-123456", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()));
            let (tx, rx) = mpsc::channel();
            let shared = Shared {
                env: Arc::new(Env {
                    name: "platform".into(),
                    project: env.project.clone(),
                    region: env.region.clone(),
                    production: false,
                    config: env,
                    gcp,
                }),
                loaded: Loaded { config, source: Source::Gcloud("test".into()) },
                tx,
                generation: 0,
                toasts: Vec::new(),
                jump: None,
                switch_env: None,
                login: false,
            };
            let mut h = Harness { module: Database::new(), shared, rx };
            let mut ctx = Ctx { shared: &mut h.shared, module: 0 };
            h.module.reset(&mut ctx);
            h
        }

        fn activate(&mut self) {
            let mut ctx = Ctx { shared: &mut self.shared, module: 0 };
            self.module.activate(&mut ctx);
        }

        fn key(&mut self, code: KeyCode) {
            let mut ctx = Ctx { shared: &mut self.shared, module: 0 };
            self.module.key(KeyEvent::new(code, KeyModifiers::NONE), &mut ctx);
        }

        /// Delivers what the background reads send, until `done` holds.
        fn wait(&mut self, done: impl Fn(&Database) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done(&self.module) {
                assert!(std::time::Instant::now() < deadline, "the reads never finished");
                if let Ok(AppEvent::Task { payload, .. }) = self.rx.recv_timeout(Duration::from_millis(100)) {
                    let mut ctx = Ctx { shared: &mut self.shared, module: 0 };
                    self.module.task(payload, &mut ctx);
                }
            }
        }
    }

    fn read(m: &Database) -> bool {
        !m.animating() && !matches!(m.load, LoadState::Idle)
    }

    #[test]
    fn an_instance_on_screen_with_its_charts_and_databases() {
        let scene = Scene::live();
        let fake = scene.serve();
        let mut h = Harness::new(&fake);
        assert!(!h.module.animating());
        h.activate();
        assert!(h.module.animating(), "reading in the background");
        h.wait(read);
        assert_eq!(h.module.badge(), None);
        let text = screen(&mut h.module, 150, 60);
        for expected in [
            "shared-db",
            "● RUNNABLE",
            "Connections",
            "60 of 500 now (12%), peak 120 (24%)",
            "CPU",
            "22% over the last hour",
            "Memory",
            "Disk",
            "Txn ids",
            "Last backup",
            "CONNECTIONS BY DATABASE",
            "orders",
            "ACTIVE",
            "IDLE",
            "PEAK 24H",
            "+ 2 databases with no connections now",
            "INSTANCE",
            "POSTGRES_15",
            "max_connections 500",
            "Mon 03:00 UTC",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        assert!(!text.contains("NEEDS ATTENTION"), "{text}");
        assert!(fake.requests().iter().all(|r| r.method == "GET"), "it only reads");
        // A later read goes out; older answers would be dropped.
        let before = fake.requests().len();
        h.key(KeyCode::Char('r'));
        assert!(h.module.animating());
        h.wait(read);
        assert_eq!(fake.requests().len(), before * 2);
    }

    #[test]
    fn a_bad_finding_shows_in_the_badge_and_first_on_screen() {
        let mut scene = Scene::live();
        scene.connections = vec![(60.0, 490.0)];
        scene.disk = 0.90;
        let fake = scene.serve();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        assert_eq!(h.module.badge(), Some(("1 bad".to_string(), theme::ERR)));
        let text = screen(&mut h.module, 150, 60);
        let (first, second) = (text.find("✗ connections: 490 of 500").unwrap(), text.find("! disk 90% full").unwrap());
        assert!(text.find("NEEDS ATTENTION").unwrap() < first && first < second, "the worst first:\n{text}");
        assert!(text.contains("Most are in orders"), "{text}");
    }

    #[test]
    fn several_instances_take_turns() {
        let scene = Scene::live();
        let other = json!({ "name": "archive", "state": "FAILED", "databaseVersion": "POSTGRES_15",
                            "connectionName": "my-platform-123456:us-east1:archive",
                            "settings": { "activationPolicy": "ALWAYS", "backupConfiguration": { "enabled": true } } });
        let fake = MockServer::start(vec![]);
        let list = json!({ "items": [other, scene.instance] }).to_string();
        fake.route("/instances?", Reply::json(200, &list));
        scene.route_on(&fake);
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        assert_eq!(h.module.entries.len(), 2);
        assert_eq!(h.module.badge(), Some(("1 bad".to_string(), theme::ERR)));
        assert_eq!(h.module.hints()[1], ("←→", "instance"));
        let text = screen(&mut h.module, 150, 60);
        assert!(text.contains("archive") && text.contains("● FAILED"), "{text}");
        assert!(text.contains("state is FAILED"), "{text}");
        h.key(KeyCode::Down);
        h.key(KeyCode::Right);
        assert_eq!(h.module.scroll, 0, "a new instance starts at the top");
        let text = screen(&mut h.module, 150, 60);
        assert!(text.contains("● RUNNABLE") && text.contains("archive  shared-db"), "{text}");
        h.key(KeyCode::Right);
        assert_eq!(h.module.current, 1, "no further");
        h.key(KeyCode::Left);
        assert_eq!(h.module.current, 0);
        // Reading again stays on the instance selected, and keeps what was read until the new answer.
        h.key(KeyCode::Right);
        h.key(KeyCode::Char('r'));
        h.wait(|m| m.entries.iter().all(|e| e.reading));
        assert!(h.module.entries.iter().all(|e| e.health.is_some()));
        assert_eq!(h.module.current, 1);
        h.wait(read);
    }

    #[test]
    fn scrolling_stops_at_the_end_and_page_keys_move() {
        let fake = Scene::live().serve();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        screen(&mut h.module, 100, 12);
        h.key(KeyCode::Down);
        assert_eq!(h.module.scroll, 1);
        h.key(KeyCode::PageDown);
        assert!(h.module.scroll > 1);
        h.key(KeyCode::End);
        let text = screen(&mut h.module, 100, 12);
        assert!(text.contains("deletion protection on"), "the last line:\n{text}");
        assert!(h.module.scroll < u16::MAX);
        h.key(KeyCode::Home);
        assert_eq!(h.module.scroll, 0);
    }

    #[test]
    fn what_it_could_not_read_is_said() {
        let fake = MockServer::start(vec![]);
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Cloud SQL Admin API is off"}}"#;
        fake.route("/instances", Reply::json(403, denied));
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        let text = screen(&mut h.module, 120, 20);
        assert!(text.contains("Cloud SQL Admin API is off"), "{text}");
        assert_eq!(h.module.badge(), None);

        // The instance reads, the metrics do not.
        let fake = MockServer::start(vec![]);
        fake.route(
            "timeSeries",
            Reply::json(403, r#"{"error":{"status":"PERMISSION_DENIED","message":"no metrics"}}"#),
        );
        Scene::live().route_on(&fake);
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        let text = screen(&mut h.module, 150, 40);
        assert!(text.contains("metrics: PERMISSION_DENIED: no metrics"), "{text}");
        assert!(text.contains("Last backup") && text.contains("POSTGRES_15"), "the rest still shows:\n{text}");
    }

    #[test]
    fn an_empty_project_a_narrow_terminal_and_a_stopped_instance_draw() {
        let fake = MockServer::start(vec![]);
        fake.route("/instances", Reply::json(200, "{}"));
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        assert!(screen(&mut h.module, 100, 10).contains("No Cloud SQL instances in my-platform-123456"));

        let fake = Scene::live().serve();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        screen(&mut h.module, 40, 10);
        let text = screen(&mut h.module, 60, 50);
        assert!(text.contains("orders") && !text.contains("DEADLOCKS"), "the extra columns give way:\n{text}");

        let mut scene = Scene::live();
        scene.instance["settings"]["activationPolicy"] = json!("NEVER");
        let fake = scene.serve();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(read);
        let text = screen(&mut h.module, 120, 30);
        assert!(text.contains("● stopped") && text.contains("✗ stopped"), "{text}");
        assert!(text.contains("POSTGRES_15"), "{text}");
    }
}
