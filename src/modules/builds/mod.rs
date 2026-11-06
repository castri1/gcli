//! Builds: the environment's recent Cloud Build builds with their app, filtered by app (`a`) and
//! by failed (`f`), and which apps' last deploy failed: their latest build failed and no later one
//! succeeded, whatever the deploy command said. Enter opens a build: its step timeline and its
//! log, a failed one at its first error, and a Markdown report for an AI agent (`c`).

mod view;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use super::common::{LoadState, matches_filter};
use crate::builds::{self, Deploys, Detail, Failure, Row};
use crate::config::AppsConvention;
use crate::gcp::build::{self, Build, Log};
use crate::gcp::{self, GcpError};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{InputEvent, TextInput};
use crate::util::now_secs;

/// Builds a list holds at most, and how many come at a time.
const LIST_LIMIT: usize = 200;
const LIST_PAGE: usize = 100;

/// The list's and the last deploys' messages carry the number of the read they belong to: a read
/// that started later wins over one still coming.
enum Msg {
    Page(u64, Vec<Build>),
    Done(u64),
    Failed(u64, GcpError),
    Deploys(u64, gcp::Result<Deploys>),
    /// The open build, read again.
    Build {
        id: String,
        result: Box<gcp::Result<Build>>,
    },
    Log {
        id: String,
        result: gcp::Result<Log>,
    },
    Deploy {
        id: String,
        result: Box<gcp::Result<Deploys>>,
    },
}

/// The build shown full screen, and what has been read of it.
struct Open {
    id: String,
    build: Option<Build>,
    /// Reading the build itself failed.
    error: Option<GcpError>,
    log: Option<gcp::Result<Log>>,
    view: LogView,
    /// Failed builds of an app are read for how its last deploy stands.
    deploy_wanted: bool,
    deploy: Option<gcp::Result<Deploys>>,
}

impl Open {
    fn new(id: &str, build: Option<Build>) -> Open {
        Open {
            id: id.to_string(),
            build,
            error: None,
            log: None,
            view: LogView::default(),
            deploy_wanted: false,
            deploy: None,
        }
    }

    fn loading(&self) -> bool {
        self.log.is_none()
            || (self.build.is_none() && self.error.is_none())
            || (self.deploy.is_none() && self.deploy_wanted)
    }

    /// Why the build failed, once it and its log are read.
    fn failure(&self) -> Option<Failure> {
        match (&self.build, &self.log) {
            (Some(b), Some(Ok(log))) if b.failed() => Some(Failure::of(b, log)),
            _ => None,
        }
    }
}

pub struct Builds {
    builds: Vec<Build>,
    load: LoadState,
    /// Reads of the list started, the last one current.
    loads: u64,
    /// `None` while the last deploys are read.
    deploys: Option<gcp::Result<Deploys>>,
    deploy_reads: u64,
    /// Only this app's builds, as Google filters them: older ones come in.
    app: Option<String>,
    /// Only failed builds, as Google filters them.
    failed_only: bool,
    filter: TextInput,
    filtering: bool,
    /// Indexes in `builds` shown.
    visible: Vec<usize>,
    list_state: ListState,
    list_area: Rect,
    env_name: String,
    project: String,
    region: String,
    convention: AppsConvention,
    open: Option<Open>,
}

impl Builds {
    pub fn new() -> Builds {
        Builds {
            builds: Vec::new(),
            load: LoadState::Idle,
            loads: 0,
            deploys: None,
            deploy_reads: 0,
            app: None,
            failed_only: false,
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

    /// Reads the list and the last deploys.
    fn load(&mut self, ctx: &mut Ctx) {
        self.read_list(ctx);
        self.deploys = None;
        self.deploy_reads += 1;
        let (n, gcp, convention) = (self.deploy_reads, ctx.env().gcp.clone(), self.convention);
        ctx.spawn(move || Msg::Deploys(n, builds::failing_deploys(&gcp, convention)));
    }

    /// Reads the list again, with the filters set.
    fn read_list(&mut self, ctx: &mut Ctx) {
        self.builds.clear();
        self.visible.clear();
        self.list_state.select(None);
        self.loads += 1;
        self.load = LoadState::Loading;
        let n = self.loads;
        let filter = builds::list_filter(self.convention, self.app.as_deref(), self.failed_only);
        let gcp = ctx.env().gcp.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let (mut got, mut gone) = (0, false);
            let result = build::pages(&gcp, &filter, LIST_PAGE, |page| {
                got += page.len();
                gone = !em.emit(Msg::Page(n, page));
                !gone && got < LIST_LIMIT
            });
            if !gone {
                em.emit(match result {
                    Ok(()) => Msg::Done(n),
                    Err(e) => Msg::Failed(n, e),
                });
            }
        });
    }

    fn deploys(&self) -> Option<&Deploys> {
        self.deploys.as_ref().and_then(|d| d.as_ref().ok())
    }

    fn row<'a>(&'a self, b: &'a Build) -> Row<'a> {
        Row::new(b, self.convention, self.deploys())
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|b| b.id.clone());
        let query = self.filter.value.trim().to_lowercase();
        self.visible = (0..self.builds.len())
            .filter(|&i| {
                let b = &self.builds[i];
                let app = self.convention.build_app(b).unwrap_or_default();
                matches_filter(&format!("{app} {} {} {} {}", b.id, b.status, b.trigger(), b.commit()), &query)
            })
            .collect();
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.builds[i].id == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Build> {
        self.builds.get(*self.visible.get(self.list_state.selected()?)?)
    }

    /// The build the keys act on: the one open, else the one selected.
    fn current(&self) -> Option<&Build> {
        match &self.open {
            Some(open) => open.build.as_ref(),
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
        let Some(b) = self.selected().cloned() else { return };
        self.open = Some(Open::new(&b.id, Some(b.clone())));
        self.read_open(&b, ctx);
    }

    /// Reads the open build's log, how its app stands, and the build again (a list can be old).
    fn read_open(&mut self, b: &Build, ctx: &mut Ctx) {
        let id = b.id.clone();
        let gcp = ctx.env().gcp.clone();
        let (i, snapshot) = (id.clone(), b.clone());
        ctx.spawn(move || Msg::Log { id: i, result: build::log(&gcp, &snapshot) });
        let (gcp, i) = (ctx.env().gcp.clone(), id.clone());
        ctx.spawn(move || Msg::Build { id: i, result: Box::new(build::find(&gcp, &id)) });
        self.read_deploy(b, ctx);
    }

    fn read_deploy(&mut self, b: &Build, ctx: &mut Ctx) {
        let convention = self.convention;
        let app = convention.build_app(b).map(str::to_string).filter(|_| b.failed());
        if let Some(open) = &mut self.open {
            open.deploy_wanted = app.is_some();
        }
        let Some(app) = app else { return };
        let (gcp, id) = (ctx.env().gcp.clone(), b.id.clone());
        ctx.spawn(move || Msg::Deploy { id, result: Box::new(builds::app_deploys(&gcp, convention, &app)) });
    }

    /// The open build's log came: it fills the viewer, a failed build at its first error.
    fn on_log(&mut self, result: gcp::Result<Log>) {
        let Some(open) = &mut self.open else { return };
        let zoomed = open.view.zoomed;
        open.view = LogView::default();
        open.view.zoomed = zoomed;
        if let Ok(log) = &result {
            open.view.extend(log.lines.iter().map(|l| l.shown()));
        }
        open.log = Some(result);
        if let Some(at) = open.failure().and_then(|f| f.first_error) {
            open.view.jump_to(at);
        }
    }

    /// Everything the open build's report needs, once it is read.
    fn detail(&self) -> Option<Detail> {
        let open = self.open.as_ref()?;
        let build = open.build.clone()?;
        if (build.failed() && open.log.is_none()) || (open.deploy_wanted && open.deploy.is_none()) {
            return None;
        }
        Some(Detail {
            app: self.convention.build_app(&build).map(str::to_string),
            deploy: open.deploy.clone(),
            log: open.log.clone(),
            build,
        })
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(detail) = self.detail() else {
            return ctx.toast(Level::Info, "Still reading the build: try again in a moment");
        };
        let region = if self.region.is_empty() { "global" } else { self.region.as_str() };
        let md = builds::report::markdown(&self.env_name, &self.project, region, &detail, now_secs());
        copy(ctx, &md, &format!("Copied the report of {}. Paste it into your AI agent.", detail.build.short_id()));
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
                self.refilter();
            }
            KeyCode::Esc if self.app.is_some() => {
                self.app = None;
                self.read_list(ctx);
            }
            KeyCode::Char('f') => {
                self.failed_only = !self.failed_only;
                self.read_list(ctx);
            }
            KeyCode::Char('a') => {
                self.app = match &self.app {
                    Some(_) => None,
                    None => self.selected().and_then(|b| self.convention.build_app(b)).map(str::to_string),
                };
                self.read_list(ctx);
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn detail_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let Some(open) = &mut self.open else { return Handled::No };
        match key.code {
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left => self.open = None,
            KeyCode::Up | KeyCode::Char('k') => open.view.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => open.view.scroll(1),
            KeyCode::PageUp | KeyCode::PageDown | KeyCode::End => {
                open.view.key(key);
            }
            KeyCode::Enter | KeyCode::Char('z') => open.view.zoomed = true,
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('r') => {
                let Some(b) = open.build.clone() else { return Handled::Yes };
                open.log = None;
                open.deploy = None;
                self.read_open(&b, ctx);
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

impl Module for Builds {
    fn title(&self) -> &'static str {
        "Builds"
    }

    fn description(&self) -> &'static str {
        "Cloud Build · builds per app, step timeline, failed logs, last deploy failed"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Builds::new();
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
        // The log full screen has every key but Ctrl+C.
        if let Some(open) = self.open.as_mut().filter(|o| o.view.zoomed) {
            if let LogKey::Copy = open.view.key(key) {
                let text = open.view.lines().join("\n");
                copy(ctx, &text, "Log copied");
            }
            return Handled::Yes;
        }
        if self.filtering {
            let selected = self.list_state.selected().unwrap_or(0);
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
        // Keys for the build at hand, in the list and in its detail.
        match key.code {
            KeyCode::Char('y') => {
                if let Some(id) = self.current().map(|b| b.id.clone()) {
                    copy(ctx, &id, "Build id copied");
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.current().map(|b| b.console_url(&ctx.env().gcp))
                    && !crate::platform::open_url(&url)
                {
                    ctx.toast(Level::Warn, format!("Could not open a browser: {url}"));
                }
            }
            _ if self.open.is_some() => return self.detail_key(key, ctx),
            _ => return self.list_key(key, ctx),
        }
        Handled::Yes
    }

    fn mouse(&mut self, event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        if let Some(open) = &mut self.open {
            match event.kind {
                MouseEventKind::ScrollUp => open.view.scroll(-3),
                MouseEventKind::ScrollDown => open.view.scroll(3),
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
                // What comes before the first build, as the list draws it.
                let row = event.row.saturating_sub(self.list_top()) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, _ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(n, _) | Msg::Done(n) | Msg::Failed(n, _) if n != self.loads => {}
            Msg::Page(_, page) => {
                self.builds.extend(page);
                self.builds.truncate(LIST_LIMIT);
                self.refilter();
            }
            Msg::Done(_) => self.load = LoadState::Loaded,
            Msg::Failed(_, e) => self.load = LoadState::Failed(e),
            Msg::Deploys(n, _) if n != self.deploy_reads => {}
            Msg::Deploys(_, result) => self.deploys = Some(result),
            Msg::Build { id, result } => {
                let Some(open) = self.open.as_mut().filter(|o| o.id == id) else { return };
                match *result {
                    Ok(b) => {
                        open.build = Some(b.clone());
                        open.error = None;
                        // The list can be old: what it said of a build may have changed.
                        if let Some(listed) = self.builds.iter_mut().find(|x| x.id == b.id) {
                            *listed = b;
                        }
                    }
                    Err(e) => open.error = Some(e),
                }
            }
            Msg::Log { id, result } => {
                if self.open.as_ref().is_some_and(|o| o.id == id) {
                    self.on_log(result);
                }
            }
            Msg::Deploy { id, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == id) {
                    open.deploy = Some(*result);
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
        if let Some(open) = &self.open {
            if open.view.zoomed {
                return vec![("Esc", "back")];
            }
            return vec![
                ("↑↓", "scroll the log"),
                ("z", "log full screen"),
                ("c", "copy for AI"),
                ("y", "copy id"),
                ("o", "console"),
                ("r", "refresh"),
                ("Esc", "back"),
            ];
        }
        vec![
            ("↑↓", "move"),
            ("Enter", "open"),
            ("/", "search"),
            ("f", "failed only"),
            ("a", "this app only"),
            ("y", "copy id"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let failing = self.deploys()?.failing.len();
        (failing > 0).then(|| (format!("{failing} failed"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.deploys.is_none() && !matches!(self.load, LoadState::Idle)
            || self.open.as_ref().is_some_and(Open::loading)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.open.as_ref().is_some_and(|o| o.view.zoomed)
    }

    fn wants_mouse(&self) -> bool {
        // Full screen, the terminal keeps the mouse, so text can be selected and copied.
        !self.open.as_ref().is_some_and(|o| o.view.zoomed)
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        if self.filtering {
            self.filter.insert_str(text, false);
            self.refilter();
        }
    }

    /// Another module sent the user here: this app's builds.
    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.open = None;
        self.filter.clear();
        self.filtering = false;
        self.failed_only = false;
        self.app = Some(target.to_string());
        self.read_list(ctx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builds::tests::failed;
    use crate::config::{Config, Environment, Loaded, Source};
    use crate::gcp::Gcp;
    use crate::gcp::build::tests::FAILED_BUILD;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::tui::module::{AppEvent, Env, Shared};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver};
    use std::time::Duration;

    const LOG: &str = r#"{"entries":[
        {"textPayload":"FETCHSOURCE","labels":{"build_step":"MAIN"}},
        {"textPayload":"Step #0 - \"pull\": all good","labels":{"build_step":"Step #0 - \"pull\""}},
        {"textPayload":"Step #1 - \"build-or-fail\": npm ci","labels":{"build_step":"Step #1 - \"build-or-fail\""}},
        {"textPayload":"Step #1 - \"build-or-fail\": npm ERR! boom","labels":{"build_step":"Step #1 - \"build-or-fail\""}},
        {"textPayload":"ERROR: build step 1 failed","labels":{"build_step":"MAIN"}}
    ]}"#;

    fn screen(m: &mut Builds, width: u16, height: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(width, height)).unwrap();
        t.draw(|f| {
            let area = f.area();
            m.render(f, area, &View { tick: 0, now: crate::util::parse_rfc3339("2026-11-05T12:00:00Z").unwrap() });
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
        module: Builds,
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
            let mut h = Harness { module: Builds::new(), shared, rx };
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
        fn wait(&mut self, done: impl Fn(&Builds) -> bool) {
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

    fn fake() -> MockServer {
        let fake = MockServer::start(vec![]);
        let serving = r#"{"name":"projects/p/locations/us-east1/services/shop",
            "latestReadyRevision":"projects/p/locations/us-east1/services/shop/revisions/shop-00012-abc",
            "conditions":[{"type":"ConfigurationsReady","state":"CONDITION_SUCCEEDED","lastTransitionTime":"2026-11-01T00:00:00Z"}]}"#;
        fake.route("entries:list", Reply::json(200, LOG));
        fake.route("/services/shop", Reply::json(200, serving));
        fake.route("/services", Reply::json(200, &format!(r#"{{"services":[{serving}]}}"#)));
        fake.route("create_time", Reply::json(200, "{}"));
        fake.route("/builds/1a2b3c4d", Reply::json(200, FAILED_BUILD));
        let ok = r#"{"id":"22222222-1111-2222-3333-444455556666","status":"SUCCESS","substitutions":{"_SERVICE":"orders"},
                     "startTime":"2026-11-05T09:00:00Z","finishTime":"2026-11-05T09:01:00Z"}"#;
        fake.route("%22orders%22", Reply::json(200, &format!(r#"{{"builds":[{ok}]}}"#)));
        // The list's reads and the failed builds, by what they ask.
        fake.route("status%3D%22FAILURE", Reply::json(200, &format!(r#"{{"builds":[{FAILED_BUILD}]}}"#)));
        fake.route("/builds?", Reply::json(200, &format!(r#"{{"builds":[{ok},{FAILED_BUILD}]}}"#)));
        fake
    }

    #[test]
    fn a_failed_build_opens_its_log_at_the_first_error() {
        let fake = fake();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| matches!(m.load, LoadState::Loaded) && m.deploys.is_some());
        assert_eq!(h.module.badge(), Some(("1 failed".to_string(), theme::ERR)));
        let text = screen(&mut h.module, 180, 30);
        for expected in [
            "Builds · 2",
            "✗ failure",
            "✓ success",
            "shop",
            "orders",
            "1a2b3c4d",
            "0123456",
            "3m 00s",
            "shop-main-deploy",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        assert!(text.contains("last deploy failed: still running shop-00012-abc from"), "{text}");
        assert!(
            text.contains("LAST DEPLOY FAILED") && text.contains("1 app"),
            "the banner, for those the list may not reach:\n{text}"
        );

        // The second row is the failed one.
        h.key(KeyCode::Down);
        assert_eq!(h.module.selected().map(|b| b.short_id()), Some("1a2b3c4d"));
        h.key(KeyCode::Enter);
        h.wait(|m| m.open.as_ref().is_some_and(|o| !o.loading()));
        let open = h.module.open.as_ref().unwrap();
        assert_eq!(open.failure().unwrap().first_error, Some(3));
        let text = screen(&mut h.module, 180, 30);
        for expected in [
            "shop · 1a2b3c4d",
            "✗ failure",
            "0123456789abcdef0123456789abcdef01234567 · main · acme/shop",
            "shop-main-deploy",
            "Build step failure",
            "last deploy failed",
            "build-or-fail",
            "█",
            "exit 1",
            "not run",
            "first error at line 4 of 5",
            "Step #1: npm ERR! boom",
        ] {
            assert!(text.contains(expected), "{expected} in\n{text}");
        }
        assert!(!text.contains("hunter2"), "{text}");
        // The log, full screen, takes the keys.
        h.key(KeyCode::Char('z'));
        assert!(h.module.capturing() && !h.module.wants_mouse());
        let text = screen(&mut h.module, 120, 20);
        assert!(text.contains("npm ERR! boom") && text.contains("line "), "{text}");
        h.key(KeyCode::Esc);
        assert!(!h.module.capturing() && h.module.open.is_some());
        // The report is ready.
        let md = h.module.detail().map(|d| builds::report::markdown("platform", "p", "us-east1", &d, 0.0)).unwrap();
        assert!(md.contains("### Failing step: `build-or-fail`") && md.contains("npm ERR! boom"), "{md}");
        h.key(KeyCode::Esc);
        assert!(h.module.open.is_none());
        // Nothing here changes anything.
        assert!(fake.requests().iter().all(|r| r.method == "GET" || r.path.ends_with("entries:list")));
    }

    #[test]
    fn filters_read_again_and_search_stays_local() {
        let fake = fake();
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| matches!(m.load, LoadState::Loaded) && m.deploys.is_some());
        let lists = |fake: &MockServer| fake.requests().iter().filter(|r| r.path.contains("pageSize=100&")).count();
        let before = lists(&fake);

        h.key(KeyCode::Char('f'));
        assert!(h.module.failed_only);
        h.wait(|m| matches!(m.load, LoadState::Loaded));
        assert_eq!(lists(&fake), before + 1);
        assert_eq!(h.module.builds.len(), 1);
        assert!(fake.requests().last().unwrap().path.contains("status%3D%22FAILURE"));
        assert!(screen(&mut h.module, 180, 30).contains("failed only"));
        h.key(KeyCode::Char('f'));
        h.wait(|m| matches!(m.load, LoadState::Loaded) && m.builds.len() == 2);

        // `a`: the selected build's app, read from Google so older builds come in.
        h.key(KeyCode::Char('a'));
        assert_eq!(h.module.app.as_deref(), Some("orders"));
        h.wait(|m| matches!(m.load, LoadState::Loaded));
        assert_eq!(h.module.builds.len(), 1);
        assert!(screen(&mut h.module, 180, 30).contains("of orders"));
        h.key(KeyCode::Esc);
        assert_eq!(h.module.app, None);
        h.wait(|m| matches!(m.load, LoadState::Loaded) && m.builds.len() == 2);

        // `/` searches what was read.
        let reads = fake.requests().len();
        h.key(KeyCode::Char('/'));
        assert!(h.module.capturing());
        for c in "shop".chars() {
            h.key(KeyCode::Char(c));
        }
        h.key(KeyCode::Enter);
        assert_eq!(h.module.visible.len(), 1);
        assert_eq!(fake.requests().len(), reads);
        h.key(KeyCode::Esc);
        assert_eq!(h.module.visible.len(), 2);

        // From another module: that app's builds.
        let mut ctx = Ctx { shared: &mut h.shared, module: 0 };
        h.module.open("shop", &mut ctx);
        assert_eq!(h.module.app.as_deref(), Some("shop"));
    }

    #[test]
    fn what_it_could_not_read_is_said() {
        let fake = MockServer::start(vec![]);
        let denied = r#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"Cloud Build is off"}}"#;
        fake.route("/builds", Reply::json(403, denied));
        fake.route("/services", Reply::json(200, "{}"));
        let mut h = Harness::new(&fake);
        h.activate();
        h.wait(|m| matches!(m.load, LoadState::Failed(_)) && m.deploys.is_some());
        assert_eq!(h.module.badge(), None);
        let text = screen(&mut h.module, 120, 20);
        assert!(text.contains("PERMISSION_DENIED"), "{text}");
        assert!(!h.module.animating());
    }

    #[test]
    fn a_narrow_terminal_and_an_empty_list_draw() {
        let mut m = Builds::new();
        m.env_name = "platform".into();
        m.project = "p".into();
        m.load = LoadState::Loaded;
        assert!(screen(&mut m, 80, 20).contains("No builds in p"));
        m.builds = vec![failed()];
        m.refilter();
        screen(&mut m, 40, 10);
        let text = screen(&mut m, 70, 12);
        assert!(text.contains("shop"), "{text}");
        m.open = Some(Open::new("1a2b3c4d", Some(failed())));
        let text = screen(&mut m, 60, 14);
        assert!(text.contains("reading"), "{text}");
        m.open.as_mut().unwrap().log = Some(Ok(Log::default()));
        let text = screen(&mut m, 120, 30);
        assert!(text.contains("Cloud Logging holds no line"), "{text}");
    }
}
