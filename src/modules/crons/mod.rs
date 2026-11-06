//! Crons: the environment's Cloud Scheduler jobs, the ones in trouble first, filtered by failing
//! (`f`) and by app (`a`). Enter opens a job: what is wrong with it and its recent attempts, and a
//! Markdown report for an AI agent (`c`); `l` goes to its app's logs. Read-only: nothing here runs,
//! pauses, resumes, changes or deletes a job.

mod view;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use super::common::{LoadState, matches_filter};
use crate::config::AppsConvention;
use crate::crons::history::{self, History};
use crate::crons::{Detail, Listing, Row, report};
use crate::findings::Level as Found;
use crate::gcp::{self, GcpError};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{InputEvent, TextInput};
use crate::util::now_secs;

/// Messages carry the number of the read they belong to: a read that started later wins.
enum Msg {
    Listed(u64, Box<gcp::Result<Listing>>),
    History(u64, gcp::Result<History>),
    Detail { job: String, result: Box<gcp::Result<Detail>> },
}

/// The job shown full screen.
struct Open {
    job: String,
    /// `None` while it is read.
    detail: Option<gcp::Result<Detail>>,
    /// Lines the attempts are scrolled.
    scroll: u16,
    /// Lines of attempts there are to scroll, as last drawn.
    lines: u16,
}

pub struct Crons {
    listing: Listing,
    load: LoadState,
    loads: u64,
    /// Only the jobs failing.
    failed_only: bool,
    /// Only this app's jobs.
    app: Option<String>,
    filter: TextInput,
    filtering: bool,
    /// Indexes in `listing.rows` shown.
    visible: Vec<usize>,
    list_state: ListState,
    list_area: Rect,
    env_name: String,
    project: String,
    region: String,
    convention: AppsConvention,
    open: Option<Open>,
}

impl Crons {
    pub fn new() -> Crons {
        Crons {
            listing: Listing::default(),
            load: LoadState::Idle,
            loads: 0,
            failed_only: false,
            app: None,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            list_area: Rect::default(),
            env_name: String::new(),
            project: String::new(),
            region: String::new(),
            convention: AppsConvention::Run,
            open: None,
        }
    }

    /// Reads the jobs and the services; the attempts of those failing follow.
    fn load(&mut self, ctx: &mut Ctx) {
        self.loads += 1;
        self.load = LoadState::Loading;
        let (n, gcp, convention) = (self.loads, ctx.env().gcp.clone(), self.convention);
        ctx.spawn(move || Msg::Listed(n, Box::new(Listing::read(&gcp, convention, now_secs()))));
    }

    fn on_listed(&mut self, n: u64, listing: Listing, ctx: &mut Ctx) {
        let in_question = listing.in_question();
        let keep = self.selected().map(|r| r.job.id().to_string());
        self.listing = listing;
        self.load = LoadState::Loaded;
        self.refilter(keep);
        let gcp = ctx.env().gcp.clone();
        ctx.spawn(move || Msg::History(n, history::read(&gcp, &in_question, now_secs())));
    }

    fn refilter(&mut self, keep: Option<String>) {
        let query = self.filter.value.trim().to_lowercase();
        let now = now_secs();
        let rows = &self.listing.rows;
        self.visible = (0..rows.len())
            .filter(|&i| {
                let r = &rows[i];
                let app = r.app().unwrap_or_default();
                (!self.failed_only || r.level(now) == Some(Found::Bad))
                    && self.app.as_deref().is_none_or(|a| a == app)
                    && matches_filter(
                        &format!("{} {app} {} {}", r.job.id(), r.job.schedule, r.job.outcome().text()),
                        &query,
                    )
            })
            .collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| rows[i].job.id() == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    /// Filters changed: the same job stays selected when it can.
    fn filters_changed(&mut self) {
        let keep = self.selected().map(|r| r.job.id().to_string());
        self.refilter(keep);
    }

    fn selected(&self) -> Option<&Row> {
        self.listing.rows.get(*self.visible.get(self.list_state.selected()?)?)
    }

    /// The job the keys act on: the one open, else the one selected.
    fn current(&self) -> Option<&Row> {
        match &self.open {
            Some(Open { detail: Some(Ok(d)), .. }) => Some(&d.row),
            Some(open) => self.listing.rows.iter().find(|r| r.job.id() == open.job),
            None => self.selected(),
        }
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

    fn open_selected(&mut self, ctx: &mut Ctx) {
        let Some(id) = self.selected().map(|r| r.job.id().to_string()) else { return };
        self.open = Some(Open { job: id.clone(), detail: None, scroll: 0, lines: 0 });
        self.read_open(id, ctx);
    }

    fn read_open(&mut self, job: String, ctx: &mut Ctx) {
        let (gcp, convention) = (ctx.env().gcp.clone(), self.convention);
        ctx.spawn(move || {
            let result = Detail::load(&gcp, convention, &job, now_secs());
            Msg::Detail { job, result: Box::new(result) }
        });
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(Open { detail: Some(Ok(detail)), .. }) = &self.open else {
            return ctx.toast(Level::Info, "Still reading the job: try again in a moment");
        };
        let md = report::markdown(&self.env_name, &self.project, detail, now_secs());
        let done = format!("Copied the report of {}. Paste it into your AI agent.", detail.row.job.id());
        copy(ctx, &md, &done);
    }

    fn list_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let selected = self.list_state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(selected.saturating_sub(1)),
            KeyCode::Down | KeyCode::Char('j') => self.select(selected + 1),
            KeyCode::PageUp => self.select(selected.saturating_sub(self.page())),
            KeyCode::PageDown => self.select(selected + self.page()),
            KeyCode::Home => self.select(0),
            KeyCode::End => self.select(usize::MAX),
            KeyCode::Enter | KeyCode::Right => self.open_selected(ctx),
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc if !self.filter.value.is_empty() => {
                self.filter.clear();
                self.filters_changed();
            }
            KeyCode::Esc if self.app.is_some() => {
                self.app = None;
                self.filters_changed();
            }
            KeyCode::Char('f') => {
                self.failed_only = !self.failed_only;
                self.filters_changed();
            }
            KeyCode::Char('a') => {
                self.app = match &self.app {
                    Some(_) => None,
                    None => self.selected().and_then(Row::app).map(str::to_string),
                };
                self.filters_changed();
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn detail_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let Some(open) = &mut self.open else { return Handled::No };
        let last = open.lines.saturating_sub(1);
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => self.open = None,
            KeyCode::Up | KeyCode::Char('k') => open.scroll = open.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => open.scroll = open.scroll.saturating_add(1).min(last),
            KeyCode::PageUp => open.scroll = open.scroll.saturating_sub(10),
            KeyCode::PageDown => open.scroll = open.scroll.saturating_add(10).min(last),
            KeyCode::Home => open.scroll = 0,
            KeyCode::End => open.scroll = last,
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('r') => {
                open.detail = None;
                let job = open.job.clone();
                self.read_open(job, ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }
}

fn copy(ctx: &mut Ctx, text: &str, done: &str) {
    let ok = crate::platform::copy_to_clipboard(text);
    ctx.toast(if ok { Level::Success } else { Level::Warn }, if ok { done } else { "Clipboard not available" });
}

impl Module for Crons {
    fn title(&self) -> &'static str {
        "Crons"
    }

    fn description(&self) -> &'static str {
        "Cloud Scheduler · jobs failing, since when and why, attempts, orphans (read-only)"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Crons::new();
        let env = ctx.env();
        self.env_name = env.name.clone();
        self.project = env.project.clone();
        self.region = env.region.clone();
        self.convention = env.config.apps;
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if self.filtering {
            let selected = self.list_state.selected().unwrap_or(0);
            match self.filter.handle(key) {
                InputEvent::Changed => {
                    self.filters_changed();
                    self.select(0);
                }
                InputEvent::Submit => self.filtering = false,
                InputEvent::Cancel => {
                    self.filtering = false;
                    self.filter.clear();
                    self.filters_changed();
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
        // Keys for the job at hand, in the list and in its detail.
        match key.code {
            KeyCode::Char('y') => {
                if let Some(id) = self.current().map(|r| r.job.id().to_string()) {
                    copy(ctx, &id, "Job name copied");
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.current().map(|r| r.job.console_url(&self.project))
                    && !crate::platform::open_url(&url)
                {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            KeyCode::Char('l') => match self.current().and_then(Row::live_app).map(str::to_string) {
                Some(app) => ctx.jump("Logs", app),
                None => ctx.toast(Level::Info, "This job has no app whose logs to read"),
            },
            _ if self.open.is_some() => return self.detail_key(key, ctx),
            _ => return self.list_key(key, ctx),
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if let Some(open) = &mut self.open {
            match event.kind {
                MouseEventKind::ScrollUp => open.scroll = open.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => {
                    open.scroll = open.scroll.saturating_add(3).min(open.lines.saturating_sub(1));
                }
                _ => return Handled::No,
            }
            return Handled::Yes;
        }
        if !self.list_area.contains((event.column, event.row).into()) {
            return Handled::No;
        }
        let selected = self.list_state.selected().unwrap_or(0);
        match event.kind {
            MouseEventKind::ScrollUp => self.select(selected.saturating_sub(1)),
            MouseEventKind::ScrollDown => self.select(selected + 1),
            MouseEventKind::Down(MouseButton::Left) => {
                let row = event.row.saturating_sub(self.list_top()) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Listed(n, _) | Msg::History(n, _) if n != self.loads => {}
            Msg::Listed(n, result) => match *result {
                Ok(listing) => self.on_listed(n, listing, ctx),
                Err(e) => self.load = LoadState::Failed(e),
            },
            Msg::History(_, result) => self.listing.apply_history(result),
            Msg::Detail { job, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.job == job) {
                    open.detail = Some(*result);
                }
            }
        }
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.open.is_some() {
            self.render_detail(f, area, view);
        } else {
            self.render_list(f, area, view);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.open.is_some() {
            return vec![
                ("↑↓", "scroll the attempts"),
                ("l", "its logs"),
                ("c", "copy for AI"),
                ("y", "copy name"),
                ("o", "console"),
                ("r", "refresh"),
                ("Esc", "back"),
            ];
        }
        vec![
            ("↑↓", "move"),
            ("Enter", "open"),
            ("/", "search"),
            ("f", "failing only"),
            ("a", "this app only"),
            ("l", "its logs"),
            ("y", "copy name"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let failing = self.listing.failing(now_secs());
        (failing > 0).then(|| (format!("{failing} failing"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.reading_history()
            || self.open.as_ref().is_some_and(|o| o.detail.is_none())
    }

    fn capturing(&self) -> bool {
        self.filtering
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.filtering {
            self.filter.insert_str(text, false);
            self.filters_changed();
        }
    }

    /// Another module sent the user here: this app's jobs.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.open = None;
        self.filter.clear();
        self.filtering = false;
        self.failed_only = false;
        self.app = Some(target.to_string()).filter(|t| !t.is_empty());
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
        self.filters_changed();
    }
}

impl Crons {
    fn reading_history(&self) -> bool {
        matches!(self.load, LoadState::Loaded) && !self.listing.history_read
    }

    /// What it could not read about the job's environment, for the list's title.
    fn unread(&self) -> Option<&GcpError> {
        self.listing.services_error.as_ref().or(self.listing.history_error.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Environment, Loaded, Source};
    use crate::crons::history::tests::{finished, started};
    use crate::crons::tests::{job, service};
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

    const DIRECT: &str = "https://shop-123456789.us-east1.run.app/cron/x";

    fn screen(m: &mut Crons, width: u16, height: u16) -> String {
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

    /// The module in a shell of its own, against a fake Google Cloud.
    struct Harness {
        module: Crons,
        shared: Shared,
        rx: Receiver<AppEvent>,
    }

    impl Harness {
        fn new(fake: &MockServer) -> Harness {
            let env = Environment {
                project: "my-platform-123456".into(),
                region: "us-east1".into(),
                apps: AppsConvention::Whiteghost,
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
            let mut h = Harness { module: Crons::new(), shared, rx };
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
        fn wait(&mut self, done: impl Fn(&Crons) -> bool) {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !done(&self.module) {
                assert!(std::time::Instant::now() < deadline, "the reads never finished");
                if let Ok(AppEvent::Task { payload, .. }) = self.rx.recv_timeout(Duration::from_millis(100)) {
                    let mut ctx = Ctx { shared: &mut self.shared, module: 0 };
                    self.module.task(payload, &mut ctx);
                }
            }
        }

        fn wait_loaded(&mut self) {
            self.wait(|m| matches!(m.load, LoadState::Loaded) && m.listing.history_read);
        }
    }

    /// Times around now, for a job's fields and its logs.
    fn ago(secs: f64) -> String {
        crate::gcp::monitoring::rfc3339(now_secs() - secs)
    }

    fn fake() -> MockServer {
        let fake = MockServer::start(vec![]);
        let times = format!(r#","lastAttemptTime":"{}","scheduleTime":"{}""#, ago(900.0), ago(-930.0));
        let failing = job("shop-nightly", DIRECT, &format!(r#"{times},"status":{{"code":14}}"#));
        let orders = "https://orders-123456789.us-east1.run.app/x";
        let healthy = job("orders-sync", orders, &format!(r#"{times},"status":{{}}"#));
        let paused = job("orders-off", orders, &format!(r#"{times},"state":"PAUSED""#));
        fake.route("/jobs/shop-nightly", Reply::json(200, &failing));
        fake.route("/jobs/orders-sync", Reply::json(200, &healthy));
        fake.route("/jobs", Reply::json(200, &format!(r#"{{"jobs":[{healthy},{failing},{paused}]}}"#)));
        fake.route(
            "/services",
            Reply::json(200, &format!(r#"{{"services":[{},{}]}}"#, service("shop", "300s"), service("orders", "300s"))),
        );
        fake.route(
            "entries:list",
            Reply::json(
                200,
                &json!({ "entries": [
                    finished("shop-nightly", &ago(895.0), 503),
                    started("shop-nightly", &ago(900.0)),
                    finished("shop-nightly", &ago(1795.0), 503),
                    started("shop-nightly", &ago(1800.0)),
                ] })
                .to_string(),
            ),
        );
        fake
    }

    #[test]
    fn the_jobs_in_trouble_come_first_and_a_job_opens_on_its_attempts() {
        let fake = fake();
        let mut h = Harness::new(&fake);
        assert_eq!(h.module.badge(), None);
        h.activate();
        h.wait_loaded();
        assert_eq!(h.module.badge(), Some(("1 failing".to_string(), theme::ERR)));
        assert!(!h.module.animating());
        let text = screen(&mut h.module, 180, 30);
        for expected in [
            "Crons · 3",
            "✗ UNAVAILABLE",
            "shop-nightly",
            "orders-sync",
            "· paused",
            "✓ ok",
            "*/15 * * * *",
            "15m ago",
            "in 15m",
            "HTTP 503",
            "2+ in a row",
            "at least 2 in a row",
            "FAILING · 1 job: shop-nightly",
            "did not answer",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        assert_eq!(h.module.selected().map(|r| r.job.id()), Some("shop-nightly"), "the failing one first");

        // Its logs are a jump away.
        h.key(KeyCode::Char('l'));
        assert_eq!(h.shared.jump.take(), Some(("Logs", "shop".to_string())));

        h.key(KeyCode::Enter);
        h.wait(|m| m.open.as_ref().is_some_and(|o| o.detail.is_some()));
        let text = screen(&mut h.module, 180, 30);
        for expected in [
            "shop-nightly",
            "UNAVAILABLE",
            "POST https://shop-123456789.us-east1.run.app/cron/x",
            "token: OIDC",
            "*/15 * * * *",
            "300s",
            "did not answer",
            "Attempts · last 24 h · 2",
            "HTTP 503",
            "took 5s",
            "URL_UNREACHABLE",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        assert!(!text.contains("hunter2"), "{text}");
        h.key(KeyCode::Down);
        assert_eq!(h.module.open.as_ref().unwrap().scroll, 1);
        h.key(KeyCode::Down);
        assert_eq!(h.module.open.as_ref().unwrap().scroll, 1, "no further than the last line");
        h.key(KeyCode::Char('l'));
        assert_eq!(h.shared.jump.take(), Some(("Logs", "shop".to_string())));
        h.key(KeyCode::Char('r'));
        assert!(h.module.open.as_ref().unwrap().detail.is_none());
        assert!(screen(&mut h.module, 100, 20).contains("reading the job"));
        h.wait(|m| m.open.as_ref().is_some_and(|o| o.detail.is_some()));
        h.key(KeyCode::Esc);
        assert!(h.module.open.is_none());
        // Nothing here changes anything, or sees a job's headers and body.
        for r in fake.requests() {
            assert!(r.method == "GET" || r.path.ends_with("entries:list"), "{} {}", r.method, r.path);
        }
    }

    #[test]
    fn filters_stay_local_and_keep_the_selection() {
        let fake = fake();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait_loaded();
        let reads = fake.requests().len();
        assert_eq!(h.module.visible.len(), 3);

        h.key(KeyCode::Char('f'));
        assert_eq!(h.module.visible.len(), 1);
        assert!(screen(&mut h.module, 180, 30).contains("failing only"));
        h.key(KeyCode::Char('f'));
        assert_eq!(h.module.visible.len(), 3);

        // `a`: the selected job's app.
        h.key(KeyCode::Down);
        assert_eq!(h.module.selected().map(|r| r.job.id()), Some("orders-off"));
        h.key(KeyCode::Char('a'));
        assert_eq!(h.module.app.as_deref(), Some("orders"));
        assert_eq!(h.module.visible.len(), 2);
        assert_eq!(h.module.selected().map(|r| r.job.id()), Some("orders-off"), "the same job stays selected");
        assert!(screen(&mut h.module, 180, 30).contains("of orders"));
        h.key(KeyCode::Esc);
        assert_eq!((h.module.app.as_deref(), h.module.visible.len()), (None, 3));

        // `/` searches what was read.
        h.key(KeyCode::Char('/'));
        assert!(h.module.capturing());
        for c in "nightly".chars() {
            h.key(KeyCode::Char(c));
        }
        h.key(KeyCode::Enter);
        assert_eq!(h.module.visible.len(), 1);
        h.key(KeyCode::Esc);
        assert_eq!(h.module.visible.len(), 3);
        assert_eq!(fake.requests().len(), reads, "none of these reads again");

        // From another module: that app's jobs.
        let mut ctx = Ctx { shared: &mut h.shared, module: 0 };
        h.module.open("shop", &mut ctx);
        assert_eq!(h.module.visible.len(), 1);
    }

    #[test]
    fn what_it_could_not_read_is_said() {
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Cloud Scheduler is off"}}"#;
        let fake = MockServer::start(vec![]);
        fake.route("/jobs", Reply::json(403, denied));
        fake.route("/services", Reply::json(200, "{}"));
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| matches!(m.load, LoadState::Failed(_)));
        assert_eq!(h.module.badge(), None);
        assert!(!h.module.animating());
        assert!(screen(&mut h.module, 120, 20).contains("PERMISSION_DENIED"));

        // The logs denied: the jobs still say what they say.
        let fake = MockServer::start(vec![]);
        let failing = job("shop-nightly", DIRECT, r#","status":{"code":14}"#);
        fake.route("/jobs", Reply::json(200, &format!(r#"{{"jobs":[{failing}]}}"#)));
        fake.route("/services", Reply::json(200, &format!(r#"{{"services":[{}]}}"#, service("shop", "300s"))));
        fake.route("entries:list", Reply::json(403, denied));
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait_loaded();
        assert_eq!(h.module.badge(), Some(("1 failing".to_string(), theme::ERR)));
        let text = screen(&mut h.module, 180, 30);
        assert!(text.contains("✗ UNAVAILABLE") && text.contains("PERMISSION_DENIED"), "{text}");
        h.key(KeyCode::Enter);
        h.wait(|m| m.open.as_ref().is_some_and(|o| o.detail.is_some()));
        assert!(screen(&mut h.module, 180, 30).contains("PERMISSION_DENIED"));
    }

    #[test]
    fn a_narrow_terminal_and_an_empty_list_draw() {
        let mut m = Crons::new();
        m.project = "p".into();
        m.load = LoadState::Loaded;
        m.listing.history_read = true;
        assert!(screen(&mut m, 80, 20).contains("No jobs in p"));
        m.listing.rows = vec![Row {
            job: serde_json::from_str(&job("shop-nightly", DIRECT, r#","status":{"code":14}"#)).unwrap(),
            target: crate::crons::Target::App("shop".into()),
            timeout: Some(300.0),
            streak: None,
        }];
        m.refilter(None);
        screen(&mut m, 40, 10);
        assert!(screen(&mut m, 70, 12).contains("shop-nightly"));
        m.open = Some(Open { job: "shop-nightly".into(), detail: None, scroll: 0, lines: 0 });
        assert!(screen(&mut m, 60, 14).contains("reading the job"));
    }
}
