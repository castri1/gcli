//! Apps: the environment's apps (Cloud Run services) with their last day (requests, errors,
//! latency, billed instance time, held requests) and, once Costs has read the bill, their
//! cost. Enter opens an app: health charts, revisions and secrets, a Markdown report for an
//! AI agent (`c`), and the two changes, restart (`R`) and min instances (`m`), each confirmed
//! (in production by typing the app's name).

mod view;

use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::widgets::ListState;

use super::common::{LoadState, matches_filter};
use crate::apps::costs::{self, CostFile};
use crate::apps::health::{self, Charts, Health, Range};
use crate::apps::secrets::{self, Secret};
use crate::apps::{Detail, REVISIONS, Row, Sort, cmd, report};
use crate::builds::{self, Failing};
use crate::config::AppsConvention;
use crate::gcp::run::{self, Revision, Service};
use crate::gcp::{self, GcpError};
use crate::tui::form::{Field, Form, FormEvent};
use crate::tui::module::{Ctx, Emitter, Handled, Level, Module, Payload, View};
use crate::tui::theme;
use crate::tui::widgets::{InputEvent, TextInput};
use crate::util::now_secs;

/// The list's messages carry the number of the load they belong to: a reload (`r`) while
/// pages still come drops the earlier ones.
enum Msg {
    Page(u64, Vec<Service>),
    Done(u64),
    Failed(u64, GcpError),
    Health(u64, gcp::Result<HashMap<String, Health>>),
    /// One service read again (after a change, or `r` in its detail).
    Service(Box<gcp::Result<Service>>),
    Revisions {
        app: String,
        result: gcp::Result<Vec<Revision>>,
    },
    Secrets {
        app: String,
        result: gcp::Result<Vec<Secret>>,
    },
    Charts {
        app: String,
        range: Range,
        result: gcp::Result<Charts>,
    },
    Changed {
        app: String,
        action: Action,
        result: gcp::Result<String>,
    },
    Deploy {
        app: String,
        result: Box<gcp::Result<Option<Failing>>>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Health,
    Revisions,
    Secrets,
}

const TABS: [Tab; 3] = [Tab::Health, Tab::Revisions, Tab::Secrets];

impl Tab {
    fn label(self) -> &'static str {
        match self {
            Tab::Health => "Health",
            Tab::Revisions => "Revisions",
            Tab::Secrets => "Secrets",
        }
    }
}

/// The app shown full screen, and what has been read of it.
struct Open {
    id: String,
    tab: Tab,
    range: Range,
    scroll: u16,
    revisions: Option<gcp::Result<Vec<Revision>>>,
    secrets: Option<gcp::Result<Vec<Secret>>>,
    charts: HashMap<Range, gcp::Result<Charts>>,
    charts_loading: HashSet<Range>,
    /// Its latest build, when it failed and no later one succeeded.
    deploy: Option<gcp::Result<Option<Failing>>>,
}

impl Open {
    fn new(id: &str) -> Open {
        Open {
            id: id.to_string(),
            tab: Tab::Health,
            range: Range::Day,
            scroll: 0,
            revisions: None,
            secrets: None,
            charts: HashMap::new(),
            charts_loading: HashSet::new(),
            deploy: None,
        }
    }

    fn loading(&self) -> bool {
        self.revisions.is_none() || self.secrets.is_none() || self.deploy.is_none() || !self.charts_loading.is_empty()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Restart,
    Min(u32),
}

/// A change being confirmed, then applied.
struct Dialog {
    action: Action,
    app: String,
    /// The service's resource name.
    service: String,
    form: Form,
    busy: bool,
}

pub struct Apps {
    services: Vec<Service>,
    load: LoadState,
    /// Loads of the list started, the last one current.
    loads: u64,
    /// `None` while the metrics are read.
    health: Option<gcp::Result<HashMap<String, Health>>>,
    costs: Result<Option<CostFile>, String>,
    sort: Sort,
    filter: TextInput,
    filtering: bool,
    /// Indexes in `services` shown, filtered and sorted.
    visible: Vec<usize>,
    list_state: ListState,
    /// The selection was moved by hand: pages that arrive later keep it.
    moved: bool,
    list_area: Rect,
    detail_area: Rect,
    env_name: String,
    project: String,
    /// Empty: every region.
    region: String,
    production: bool,
    convention: AppsConvention,
    open: Option<Open>,
    dialog: Option<Dialog>,
}

impl Apps {
    pub fn new() -> Apps {
        Apps {
            services: Vec::new(),
            load: LoadState::Idle,
            loads: 0,
            health: None,
            costs: Ok(None),
            sort: Sort::Name,
            filter: TextInput::default(),
            filtering: false,
            visible: Vec::new(),
            list_state: ListState::default(),
            moved: false,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            env_name: String::new(),
            project: String::new(),
            region: String::new(),
            production: false,
            convention: AppsConvention::Run,
            open: None,
            dialog: None,
        }
    }

    /// Reads the list again: the services page by page, the last day of all of them at the
    /// same time, and the costs file.
    fn load(&mut self, ctx: &mut Ctx) {
        self.services.clear();
        self.visible.clear();
        self.moved = false;
        self.loads += 1;
        let n = self.loads;
        self.load = LoadState::Loading;
        self.health = None;
        self.costs = costs::read(&self.env_name);
        let gcp = ctx.env().gcp.clone();
        ctx.spawn_stream(move |em: Emitter<Msg>| {
            let mut gone = false;
            let result = run::list_services(&gcp, |page| {
                gone = !em.emit(Msg::Page(n, page));
                !gone
            });
            if !gone {
                em.emit(match result {
                    Ok(()) => Msg::Done(n),
                    Err(e) => Msg::Failed(n, e),
                });
            }
        });
        let gcp = ctx.env().gcp.clone();
        ctx.spawn(move || Msg::Health(n, health::last_day(&gcp, now_secs())));
    }

    fn cost_file(&self) -> Option<&CostFile> {
        self.costs.as_ref().ok().and_then(Option::as_ref)
    }

    fn row<'a>(&'a self, s: &'a Service) -> Row<'a> {
        let health = self.health.as_ref().and_then(|h| h.as_ref().ok());
        Row::new(s, health, self.cost_file())
    }

    fn refilter(&mut self) {
        let keep = self.selected().map(|s| s.name.clone());
        let query = self.filter.value.trim().to_lowercase();
        let mut visible: Vec<usize> = (0..self.services.len())
            .filter(|&i| {
                let s = &self.services[i];
                matches_filter(&format!("{} {} {}", s.id(), s.revision(), s.last_modifier), &query)
            })
            .collect();
        crate::apps::sort(&mut visible, self.sort, |&i| self.row(&self.services[i]));
        self.visible = visible;
        let index = keep.and_then(|k| self.visible.iter().position(|&i| self.services[i].name == k)).unwrap_or(0);
        self.list_state.select(if self.visible.is_empty() { None } else { Some(index) });
    }

    fn selected(&self) -> Option<&Service> {
        self.services.get(*self.visible.get(self.list_state.selected()?)?)
    }

    fn service(&self, id: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.id() == id)
    }

    /// The app the keys act on: the one open, else the one selected.
    fn current(&self) -> Option<&Service> {
        match &self.open {
            Some(open) => self.service(&open.id),
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
        let Some(service) = self.selected().cloned() else { return };
        self.open = Some(Open::new(service.id()));
        self.fetch_detail(&service, ctx);
    }

    /// Reads the open app's revisions, secrets and charts (for its range).
    fn fetch_detail(&mut self, service: &Service, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        open.revisions = None;
        open.secrets = None;
        open.deploy = None;
        open.charts.clear();
        let (app, name) = (service.id().to_string(), service.name.clone());
        let gcp = ctx.env().gcp.clone();
        ctx.spawn(move || Msg::Revisions { result: run::recent_revisions(&gcp, &name, REVISIONS), app });
        let (gcp, convention, svc) = (ctx.env().gcp.clone(), self.convention, service.clone());
        ctx.spawn(move || Msg::Secrets { app: svc.id().to_string(), result: secrets::load(&gcp, convention, &svc) });
        let (gcp, svc) = (ctx.env().gcp.clone(), service.clone());
        ctx.spawn(move || Msg::Deploy {
            app: svc.id().to_string(),
            result: Box::new(builds::last_deploy(&gcp, convention, &svc)),
        });
        self.fetch_charts(ctx);
    }

    fn fetch_charts(&mut self, ctx: &mut Ctx) {
        let Some(open) = &mut self.open else { return };
        let range = open.range;
        if open.charts.contains_key(&range) || !open.charts_loading.insert(range) {
            return;
        }
        let (gcp, app) = (ctx.env().gcp.clone(), open.id.clone());
        ctx.spawn(move || Msg::Charts { result: health::charts(&gcp, &app, range, now_secs()), app, range });
    }

    /// Reads one app again: its service and, when it is open, its detail.
    fn refresh_app(&mut self, id: &str, ctx: &mut Ctx) {
        let (gcp, app) = (ctx.env().gcp.clone(), id.to_string());
        ctx.spawn(move || Msg::Service(Box::new(run::find_service(&gcp, &app))));
        if let Some(service) = self.service(id).cloned()
            && self.open.as_ref().is_some_and(|o| o.id == id)
        {
            self.fetch_detail(&service, ctx);
        }
    }

    /// Everything the open app's report needs, once it is all read: the 24 h charts when
    /// read, else the range shown.
    fn detail(&self) -> Option<Detail> {
        let open = self.open.as_ref()?;
        let charts = open.charts.get(&Range::Day).or_else(|| open.charts.get(&open.range))?;
        Some(Detail {
            service: self.service(&open.id)?.clone(),
            revisions: open.revisions.clone()?,
            secrets: open.secrets.clone()?,
            charts: charts.clone(),
            last_deploy: open.deploy.clone()?,
        })
    }

    fn copy_report(&mut self, ctx: &mut Ctx) {
        let Some(detail) = self.detail() else {
            return ctx.toast(Level::Info, "Still reading the app: try again in a moment");
        };
        let md = report::markdown(&self.env_name, &self.project, &detail, now_secs());
        let id = detail.service.id().to_string();
        copy(ctx, &md, &format!("Copied the report of {id}. Paste it into your AI agent."));
    }

    fn open_dialog(&mut self, restart: bool) {
        let Some(s) = self.current().cloned() else { return };
        let app = s.id().to_string();
        let mut form = if restart {
            let traffic = s.traffic_split().unwrap_or_else(|| "100% latest".into());
            let mut note = format!(
                "Makes a new revision with the same configuration (template label {}). Traffic that follows the latest revision moves to it once it is ready, and its instances start fresh, re-reading secrets bound as `latest`.",
                run::RESTART_LABEL
            );
            if s.pinned_percent() > 0 {
                note.push_str(&format!(
                    " {}% of the traffic is pinned to older revisions and stays there: those instances are not restarted.",
                    s.pinned_percent()
                ));
            }
            let waiting = self.waiting_secrets();
            if !waiting.is_empty() {
                note.push_str(&format!(" New secret versions it will load: {}.", waiting.join(", ")));
            }
            let fields = vec![
                Field::fixed("App", app.clone()),
                Field::fixed("Serving", format!("{} · traffic {traffic}", s.revision())),
            ];
            let mut form = Form::new(format!("Restart {app}"), fields).submit_label("restart").enter_submits();
            form.note = Some(note);
            form
        } else {
            let now = format!(
                "service {} · revision {} · max {}",
                s.service_min(),
                s.revision_min(),
                s.max_instances().map_or("default".into(), |m| m.to_string())
            );
            let fields = vec![
                Field::fixed("App", app.clone()),
                Field::fixed("Now", now),
                Field::text("Minimum", s.service_min().to_string()).hint("0 lets it scale to zero"),
            ];
            let mut form = Form::new(format!("Minimum instances of {app}"), fields)
                .submit_label("set")
                .enter_submits()
                .focus_on("Minimum");
            let mut note = "The service-level minimum: it applies at once, without a new revision. Warm instances are billed even without requests.".to_string();
            if s.revision_min() > 0 {
                note.push_str(&format!(
                    " The revision's own minimum ({}) still applies when it is higher: change it with a deploy.",
                    s.revision_min()
                ));
            }
            form.note = Some(note);
            form
        };
        if self.production {
            form = form.confirm_with(&app);
            if restart {
                // Nothing to edit: straight to typing the name.
                form.handle(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
            }
        }
        let action = if restart { Action::Restart } else { Action::Min(0) };
        self.dialog = Some(Dialog { action, app, service: s.name.clone(), form, busy: false });
    }

    /// Variables of the open app waiting for a restart to read a new secret version.
    fn waiting_secrets(&self) -> Vec<String> {
        self.findings()
            .into_iter()
            .filter_map(|f| match f {
                secrets::Finding::NotLoaded { target, .. } => Some(target),
                _ => None,
            })
            .collect()
    }

    /// What the open app's secrets need, once they are read.
    fn findings(&self) -> Vec<secrets::Finding> {
        let Some(open) = &self.open else { return Vec::new() };
        let (Some(Ok(list)), Some(s)) = (&open.secrets, self.service(&open.id)) else { return Vec::new() };
        let created = crate::apps::revision_created(s, open.revisions.as_ref().and_then(|r| r.as_deref().ok()));
        secrets::states(&self.project, list, &s.secret_refs(), created).into_iter().flat_map(|st| st.findings).collect()
    }

    fn submit_dialog(&mut self, ctx: &mut Ctx) {
        let Some(dialog) = &mut self.dialog else { return };
        let action = match dialog.action {
            Action::Restart => Action::Restart,
            Action::Min(_) => {
                let text = dialog.form.get("Minimum").trim().to_string();
                let Ok(n) = text.parse::<u32>() else {
                    return dialog.form.fail(format!("'{text}' is not a whole number"));
                };
                let Some(service) = self.services.iter().find(|s| s.name == dialog.service) else { return };
                if let Err(e) = cmd::check_min(service, n) {
                    return dialog.form.fail(e);
                }
                Action::Min(n)
            }
        };
        dialog.action = action;
        dialog.busy = true;
        let (gcp, app, service) = (ctx.env().gcp.clone(), dialog.app.clone(), dialog.service.clone());
        ctx.spawn(move || {
            let result = match action {
                Action::Restart => run::restart(&gcp, &service, now_secs() as u64),
                Action::Min(n) => run::set_min_instances(&gcp, &service, n).map(|s| s.service_min().to_string()),
            };
            Msg::Changed { app, action, result }
        });
    }

    fn on_changed(&mut self, app: String, action: Action, result: gcp::Result<String>, ctx: &mut Ctx) {
        match result {
            Ok(value) => {
                self.dialog = None;
                let text = match action {
                    Action::Restart => format!("Restarted {app}: {value} is the new revision"),
                    Action::Min(_) => format!("{app}: service-level minimum {value}"),
                };
                ctx.toast(Level::Success, text);
                self.refresh_app(&app, ctx);
            }
            Err(e) => match &mut self.dialog {
                Some(d) if d.app == app => {
                    d.busy = false;
                    d.form.fail(e.short());
                }
                _ => ctx.toast(Level::Error, format!("{app}: {}", e.short())),
            },
        }
    }

    fn step_tab(&mut self, forward: bool) {
        let Some(open) = &mut self.open else { return };
        let at = TABS.iter().position(|t| *t == open.tab).unwrap_or(0);
        open.tab = TABS[if forward { (at + 1) % TABS.len() } else { (at + TABS.len() - 1) % TABS.len() }];
        open.scroll = 0;
    }

    fn scroll(&mut self, delta: i32) {
        if let Some(open) = &mut self.open {
            open.scroll = (open.scroll as i32 + delta).max(0) as u16;
        }
    }

    fn list_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        let selected = self.list_state.selected().unwrap_or(0);
        self.moved = true;
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
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                if self.sort == Sort::Cost && self.cost_file().is_none() {
                    // Nothing to sort by without the costs file.
                    self.sort = self.sort.next();
                }
                self.refilter();
                self.select(0);
            }
            KeyCode::Char('r') => self.load(ctx),
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn detail_key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        match key.code {
            KeyCode::Esc | KeyCode::Backspace => self.open = None,
            KeyCode::Right => self.step_tab(true),
            KeyCode::Left => self.step_tab(false),
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::PageUp => self.scroll(-(self.detail_area.height.max(2) as i32 - 2)),
            KeyCode::PageDown => self.scroll(self.detail_area.height.max(2) as i32 - 2),
            KeyCode::Home => self.scroll(-(u16::MAX as i32)),
            KeyCode::Char('w') => {
                if let Some(open) = &mut self.open {
                    open.range = open.range.next();
                }
                self.fetch_charts(ctx);
            }
            KeyCode::Char('c') => self.copy_report(ctx),
            KeyCode::Char('r') => {
                if let Some(id) = self.open.as_ref().map(|o| o.id.clone()) {
                    self.refresh_app(&id, ctx);
                }
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

impl Module for Apps {
    fn title(&self) -> &'static str {
        "Apps"
    }

    fn description(&self) -> &'static str {
        "Cloud Run · health, held requests, revisions, secrets, restart, min instances"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Apps::new();
        let env = ctx.env();
        self.env_name = env.name.clone();
        self.project = env.project.clone();
        self.region = env.region.clone();
        self.production = env.production;
        self.convention = env.config.apps;
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if matches!(self.load, LoadState::Idle) {
            self.load(ctx);
        }
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(dialog) = &mut self.dialog {
            if dialog.busy {
                // A restart can take minutes: it goes on, and a toast tells how it ended.
                if key.code == KeyCode::Esc {
                    ctx.toast(Level::Info, format!("{}: the change goes on; a toast tells how it ends", dialog.app));
                    self.dialog = None;
                }
                return Handled::Yes;
            }
            match dialog.form.handle(key) {
                FormEvent::Submit => self.submit_dialog(ctx),
                FormEvent::Cancel => self.dialog = None,
                FormEvent::Pending => {}
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
        // Keys for the app at hand, in the list and in its detail.
        match key.code {
            KeyCode::Char('R') => self.open_dialog(true),
            KeyCode::Char('m') => self.open_dialog(false),
            KeyCode::Char('y') => {
                if let Some(uri) = self.current().map(|s| s.uri.clone()).filter(|u| !u.is_empty()) {
                    copy(ctx, &uri, "URL copied");
                }
            }
            KeyCode::Char('b') => {
                if let Some(id) = self.current().map(|s| s.id().to_string()) {
                    ctx.jump("Builds", id);
                }
            }
            KeyCode::Char('o') => {
                if let Some(url) = self.current().map(|s| s.console_url(&self.project))
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
        if self.dialog.is_some() {
            return Handled::No;
        }
        if self.open.is_some() {
            match event.kind {
                MouseEventKind::ScrollUp => self.scroll(-3),
                MouseEventKind::ScrollDown => self.scroll(3),
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
                // Filter bar, top border and the header row come before the first app.
                let row = event.row.saturating_sub(self.list_area.y + 3) as usize;
                self.select(self.list_state.offset() + row);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Page(n, _) | Msg::Done(n) | Msg::Failed(n, _) | Msg::Health(n, _) if n != self.loads => {}
            Msg::Page(_, page) => {
                self.services.extend(page);
                self.refilter();
                // Pages come in Google's order, the list in its own: the top row until moved.
                if !self.moved {
                    self.select(0);
                }
            }
            Msg::Done(_) => self.load = LoadState::Loaded,
            Msg::Failed(_, e) => self.load = LoadState::Failed(e),
            Msg::Health(_, result) => {
                self.health = Some(result);
                self.refilter();
                if !self.moved {
                    self.select(0);
                }
            }
            Msg::Service(result) => match *result {
                Ok(service) => {
                    match self.services.iter_mut().find(|s| s.name == service.name) {
                        Some(s) => *s = service,
                        None => self.services.push(service),
                    }
                    self.refilter();
                }
                Err(e) => ctx.toast(Level::Warn, format!("Could not read the app again: {}", e.short())),
            },
            Msg::Revisions { app, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == app) {
                    open.revisions = Some(result);
                }
            }
            Msg::Secrets { app, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == app) {
                    open.secrets = Some(result);
                }
            }
            Msg::Charts { app, range, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == app) {
                    open.charts_loading.remove(&range);
                    open.charts.insert(range, result);
                }
            }
            Msg::Changed { app, action, result } => self.on_changed(app, action, result, ctx),
            Msg::Deploy { app, result } => {
                if let Some(open) = self.open.as_mut().filter(|o| o.id == app) {
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
        self.render_dialog(f, area, view);
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if let Some(d) = &self.dialog {
            return if d.busy {
                vec![("Esc", "hide (it goes on)")]
            } else {
                vec![("Enter", "confirm"), ("Tab", "next field"), ("Esc", "cancel")]
            };
        }
        if self.filtering {
            return vec![("Enter", "keep filter"), ("Esc", "clear"), ("↑↓", "move")];
        }
        if self.open.is_some() {
            return vec![
                ("←→", "tab"),
                ("↑↓", "scroll"),
                ("w", "24h/6h"),
                ("R", "restart"),
                ("m", "min instances"),
                ("c", "copy for AI"),
                ("b", "builds"),
                ("y", "copy URL"),
                ("o", "console"),
                ("r", "refresh"),
                ("Esc", "back"),
            ];
        }
        vec![
            ("↑↓", "move"),
            ("Enter", "open"),
            ("/", "search"),
            ("s", "sort"),
            ("R", "restart"),
            ("m", "min instances"),
            ("b", "builds"),
            ("y", "copy URL"),
            ("o", "console"),
            ("r", "reload"),
        ]
    }

    fn badge(&self) -> Option<(String, Color)> {
        let held = self.services.iter().filter(|s| self.row(s).held()).count();
        (held > 0).then(|| (format!("{held} held"), theme::ERR))
    }

    fn animating(&self) -> bool {
        matches!(self.load, LoadState::Loading)
            || self.health.is_none()
            || self.open.as_ref().is_some_and(Open::loading)
            || self.dialog.as_ref().is_some_and(|d| d.busy)
    }

    fn capturing(&self) -> bool {
        self.filtering || self.dialog.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.dialog {
            Some(d) if !d.busy => d.form.paste(text),
            Some(_) => {}
            None if self.filtering => {
                self.filter.insert_str(text, false);
                self.refilter();
            }
            None => {}
        }
    }
}
