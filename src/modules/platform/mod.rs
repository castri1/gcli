//! Platform: what the apps of a project share, in five tabs. Limits (what is counted against a
//! quota), Orphans (pieces nothing leads to, or that lead nowhere), Exposure (who can reach each
//! service, and whether its `run.app` URL is open), Certificates and Images. It only reads, and
//! offers nothing that changes or removes anything.

mod view;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use super::common::matches_filter;
use crate::findings::{Finding, Level as FindingLevel};
use crate::infra::{self, Part, Row, Section, Snapshot, Table, Want};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{InputEvent, TextInput};
use crate::util::now_secs;

/// A part of the platform, with the number of the read it belongs to: a read that started later
/// wins over one still coming.
struct Msg(u64, Part);

/// What a tab's label says.
#[derive(Default, Clone)]
struct TabState {
    /// Rows worth a look.
    flagged: usize,
    /// What is wrong in it, the worst first.
    found: Vec<Finding>,
}

pub struct Platform {
    snapshot: Snapshot,
    started: bool,
    /// Reads started, the last one current.
    reads: u64,
    tab: Section,
    table: Table,
    tabs: [TabState; 5],
    filter: TextInput,
    filtering: bool,
    /// Only the rows worth a look.
    flagged_only: bool,
    /// Indexes in `table.rows` shown.
    visible: Vec<usize>,
    list_state: ListState,
    list_area: Rect,
    /// The row of the first entry on screen.
    rows_top: u16,
}

impl Platform {
    pub fn new() -> Platform {
        Platform {
            snapshot: Snapshot::default(),
            started: false,
            reads: 0,
            tab: Section::Limits,
            table: Table { cols: Vec::new(), rows: Vec::new() },
            tabs: Default::default(),
            filter: TextInput::default(),
            filtering: false,
            flagged_only: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            list_area: Rect::default(),
            rows_top: 0,
        }
    }

    fn load(&mut self, ctx: &mut Ctx) {
        let env = ctx.env();
        self.reads += 1;
        self.started = true;
        self.snapshot = Snapshot::new(&env.gcp, &env.config.platform.exposed);
        self.rebuild();
        let (n, gcp) = (self.reads, env.gcp.clone());
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            infra::read(&gcp, Want::ALL, |part| {
                em.emit(Msg(n, part));
            });
        });
    }

    /// Everything shown is made again from what has been read.
    fn rebuild(&mut self) {
        let now = now_secs();
        for (i, section) in Section::ALL.iter().enumerate() {
            let rows = self.snapshot.table(*section, now).rows;
            self.tabs[i] = TabState {
                flagged: rows.iter().filter(|r| r.flagged).count(),
                found: infra::section_findings(&self.snapshot, *section, now),
            };
        }
        self.table = self.snapshot.table(self.tab, now);
        self.refilter();
    }

    fn tab_index(&self) -> usize {
        Section::ALL.iter().position(|s| *s == self.tab).unwrap_or(0)
    }

    fn switch(&mut self, section: Section) {
        self.tab = section;
        self.list_state = ListState::default();
        self.table = self.snapshot.table(section, now_secs());
        self.refilter();
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|r| r.name.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.table.rows.len())
            .filter(|&i| {
                let row = &self.table.rows[i];
                let text: Vec<&str> = row.cells.iter().map(|c| c.0.as_str()).collect();
                (!self.flagged_only || row.flagged) && matches_filter(&text.join(" "), &query)
            })
            .collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.table.rows[i].name == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Row> {
        self.table.rows.get(*self.visible.get(self.list_state.selected()?)?)
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

    fn reading(&self) -> bool {
        self.started && Section::ALL.iter().any(|s| self.snapshot.reading(*s))
    }

    fn bad(&self) -> usize {
        self.tabs.iter().flat_map(|t| &t.found).filter(|f| f.level == FindingLevel::Bad).count()
    }
}

impl Module for Platform {
    fn title(&self) -> &'static str {
        "Platform"
    }

    fn description(&self) -> &'static str {
        "Load balancer · limits, orphans, exposed apps, certificates, image registry"
    }

    fn reset(&mut self, _ctx: &mut Ctx) {
        *self = Platform::new();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if !self.started {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let selected = self.list_state.selected().unwrap_or(0);
        if self.filtering {
            match self.filter.handle(key) {
                InputEvent::Changed => {
                    self.refilter();
                    self.select(0);
                }
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
        let i = self.tab_index();
        let count = Section::ALL.len();
        match key.code {
            KeyCode::Right | KeyCode::Char('l') => self.switch(Section::ALL[(i + 1) % count]),
            KeyCode::Left | KeyCode::Char('h') => self.switch(Section::ALL[(i + count - 1) % count]),
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
            KeyCode::Esc if self.flagged_only => {
                self.flagged_only = false;
                self.refilter();
            }
            KeyCode::Char('f') => {
                self.flagged_only = !self.flagged_only;
                self.refilter();
            }
            KeyCode::Char('y') => {
                if let Some(name) = self.selected().map(|r| r.name.clone()) {
                    let ok = crate::platform::copy_to_clipboard(&name);
                    let (level, text) = if ok {
                        (Level::Success, format!("Copied {name}"))
                    } else {
                        (Level::Warn, "Clipboard not available".to_string())
                    };
                    ctx.toast(level, text);
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.selected().map(|r| r.console.clone())
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
            MouseEventKind::Down(MouseButton::Left) if event.row >= self.rows_top => {
                self.select(self.list_state.offset() + (event.row - self.rows_top) as usize);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, _ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        let Msg(n, part) = *msg;
        if n == self.reads {
            self.snapshot.add(part);
            self.rebuild();
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        self.render_all(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        vec![
            ("←→", "tab"),
            ("↑↓", "move"),
            ("/", "search"),
            ("f", "what needs a look"),
            ("y", "copy name"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let bad = self.bad();
        (bad > 0).then(|| (format!("{bad} bad"), theme::ERR))
    }

    fn animating(&self) -> bool {
        self.reading()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Environment, Loaded, PlatformConfig, Source};
    use crate::gcp::Gcp;
    use crate::gcp::mock::MockServer;
    use crate::infra::tests::{denied, platform_fake};
    use crate::tui::module::{AppEvent, Env, Shared};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    fn screen(m: &mut Platform, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            m.render(f, area, &View { tick: 0, now: now_secs() });
        })
        .unwrap();
        let buf = t.backend().buffer().clone();
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    struct Harness {
        module: Platform,
        shared: Shared,
        rx: Receiver<AppEvent>,
    }

    impl Harness {
        fn new(fake: &MockServer) -> Harness {
            let env = Environment {
                project: "my-platform-123456".into(),
                region: "us-east1".into(),
                platform: PlatformConfig { exposed: vec!["webhooks".into(), "retired".into()] },
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
            let mut h = Harness { module: Platform::new(), shared, rx };
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

        fn wait(&mut self, done: impl Fn(&Platform) -> bool) {
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

    #[test]
    fn every_tab_shows_what_was_read_and_nothing_changes_anything() {
        let fake = platform_fake();
        let mut h = Harness::new(&fake);
        h.activate();
        assert!(h.module.animating());
        h.wait(|m| !m.reading());
        assert!(!h.module.animating());
        assert_eq!(h.module.badge(), Some(("3 bad".to_string(), theme::ERR)));

        let text = screen(&mut h.module, 180, 30);
        for expected in [
            "Limits",
            "Orphans 4",
            "Exposure 3",
            "Backend services",
            "GLOBAL_EXTERNAL_MANAGED_BACKEND_SERVICES",
            "Host rules in shop-map",
            "no limit in the API",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }

        h.key(KeyCode::Right);
        let text = screen(&mut h.module, 180, 30);
        for expected in ["gone-backend", "no URL map uses it", "gone-neg", "gone.example.com"] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }

        h.key(KeyCode::Right);
        let text = screen(&mut h.module, 180, 30);
        for expected in
            ["orders", "allUsers", "yes, listed", "no, listed", "iap + public", "internal-and-cloud-load-balancing"]
        {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        // The worst comes first, and the banner says it.
        assert_eq!(h.module.selected().map(|r| r.name.as_str()), Some("billing"));
        assert!(text.contains("reachable directly from the internet"), "{text}");

        h.key(KeyCode::Right);
        let text = screen(&mut h.module, 180, 30);
        for expected in ["wild", "ACTIVE", "*.example.com", "old-cert", "PROVISIONING_FAILED"] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }

        h.key(KeyCode::Right);
        let text = screen(&mut h.module, 180, 30);
        for expected in ["images", "1.5 GB", "none"] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }

        h.key(KeyCode::Right);
        assert_eq!(h.module.tab, Section::Limits, "the tabs go round");
        h.key(KeyCode::Left);
        assert_eq!(h.module.tab, Section::Images);

        assert!(fake.requests().iter().all(|r| r.method == "GET"), "it only reads");
    }

    #[test]
    fn search_and_the_flagged_filter_stay_local() {
        let fake = platform_fake();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| !m.reading());
        h.key(KeyCode::Right);
        h.key(KeyCode::Right);
        let all = h.module.visible.len();
        let reads = fake.requests().len();

        h.key(KeyCode::Char('f'));
        assert!(h.module.visible.len() < all);
        assert!(h.module.visible.iter().all(|&i| h.module.table.rows[i].flagged));
        assert!(screen(&mut h.module, 180, 30).contains("needs a look"));
        h.key(KeyCode::Esc);
        assert_eq!(h.module.visible.len(), all);

        h.key(KeyCode::Char('/'));
        assert!(h.module.capturing());
        for c in "orders".chars() {
            h.key(KeyCode::Char(c));
        }
        h.key(KeyCode::Enter);
        assert_eq!(h.module.visible.len(), 1);
        assert!(!h.module.capturing());
        h.key(KeyCode::Esc);
        assert_eq!(h.module.visible.len(), all);
        assert_eq!(fake.requests().len(), reads, "nothing read again");

        h.key(KeyCode::Home);
        h.key(KeyCode::Down);
        assert_eq!(h.module.list_state.selected(), Some(1));
        h.key(KeyCode::End);
        assert_eq!(h.module.list_state.selected(), Some(all - 1));
        h.key(KeyCode::Home);
        assert_eq!(h.module.list_state.selected(), Some(0));

        // A read again starts afresh.
        h.key(KeyCode::Char('r'));
        assert!(h.module.reading());
        h.wait(|m| !m.reading());
        assert!(fake.requests().len() > reads);
    }

    #[test]
    fn what_it_could_not_read_is_said() {
        let fake = MockServer::start(vec![]);
        denied(&fake);
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| !m.reading());
        assert_eq!(h.module.badge(), None);
        for _ in 0..Section::ALL.len() {
            let text = screen(&mut h.module, 120, 20);
            assert!(text.contains("PERMISSION_DENIED"), "{text}");
            h.key(KeyCode::Right);
        }
        assert!(!h.module.animating());
    }

    #[test]
    fn a_narrow_terminal_and_nothing_read_draw() {
        let mut m = Platform::new();
        assert!(screen(&mut m, 80, 20).contains("Limits"));
        let fake = platform_fake();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| !m.reading());
        for _ in 0..Section::ALL.len() {
            screen(&mut h.module, 40, 10);
            screen(&mut h.module, 70, 12);
            screen(&mut h.module, 20, 4);
            h.key(KeyCode::Right);
        }
    }
}
