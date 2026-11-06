//! Logs: choose an app and see what it wrote, full screen in the log viewer: newest first, or
//! oldest first while following. `s` severity, `w` time window, `t` the requests log and `r` read
//! again; `f` follows every few seconds, only while the module is on screen. Everything reads on
//! background threads, and messages of a read that was replaced are dropped.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use super::common::LoadState;
use crate::gcp;
use crate::gcp::logging::{self, Entries, Entry};
use crate::gcp::run;
use crate::logs::{self, CONTINUED, Follower, POLL_SECS, Query};
use crate::tui::logview::{LogKey, LogView};
use crate::tui::module::{Ctx, Handled, Level, Module, Payload, View};
use crate::tui::picker::{Item, Outcome, Picker};
use crate::tui::theme;
use crate::tui::widgets;
use crate::util::now_secs;

/// Entries read at a time: one request for most apps.
const READ: usize = 1000;
/// Entries kept while following; the oldest go.
const KEEP: usize = 5000;
/// Longest wait between polls after failures.
const MAX_BACKOFF_SECS: u64 = 60;

/// `s` goes through these: this severity or worse, or all of them.
const SEVERITIES: [Option<&str>; 4] = [None, Some("INFO"), Some("WARNING"), Some("ERROR")];
/// `w` goes through these.
const WINDOWS: [f64; 6] = [900.0, 3600.0, 6.0 * 3600.0, 86_400.0, 3.0 * 86_400.0, 7.0 * 86_400.0];
/// One hour.
const DEFAULT_WINDOW: usize = 1;

enum Msg {
    /// The apps: (name, what the picker says of it).
    Apps(gcp::Result<Vec<(String, String)>>),
    Read {
        load: u64,
        result: gcp::Result<Entries>,
    },
    /// The follower comes back with what it found.
    Polled {
        follow: u64,
        follower: Follower,
        result: gcp::Result<Vec<Entry>>,
    },
}

pub struct Logs {
    env_name: String,
    apps: LoadState,
    names: Vec<(String, String)>,
    picker: Option<Picker>,
    /// Open the picker once the apps are read.
    want_picker: bool,
    app: Option<String>,
    /// Came here from the Apps module: back goes there.
    from_apps: bool,
    severity: usize,
    window: usize,
    requests: bool,
    /// Newest first.
    entries: Vec<Entry>,
    more: bool,
    read: LoadState,
    /// Reads started, the last one current.
    loads: u64,
    log: LogView,
    following: bool,
    /// Follow runs started or stopped, the last one current.
    follows: u64,
    /// With no poll running, the follower; a running poll has it.
    follower: Option<Follower>,
    next_poll: Instant,
    /// Seconds to the next poll: more after a failure.
    pause: u64,
}

/// An entry as the viewer's lines. CRITICAL and worse get a mark the viewer takes for an error
/// (its colour, `e`), as it does ERROR by its name.
fn entry_lines(e: &Entry) -> Vec<String> {
    let severity = logs::severity(e);
    let mark = if matches!(severity, "CRITICAL" | "ALERT" | "EMERGENCY") { "✖ " } else { "" };
    let mut message = logs::message_lines(e).into_iter();
    let first = message.next().unwrap_or_default();
    let mut lines = vec![format!("{}  {severity:<8}  {mark}{first}", logs::time_of(e))];
    lines.extend(message.map(|l| format!("{CONTINUED}{l}")));
    lines
}

fn app_items(names: &[(String, String)], current: Option<&str>) -> Vec<Item> {
    names.iter().map(|(name, about)| Item::new(name, about).current(current == Some(name.as_str()))).collect()
}

impl Logs {
    pub fn new() -> Logs {
        let mut log = LogView::default();
        log.zoomed = true;
        Logs {
            env_name: String::new(),
            apps: LoadState::Idle,
            names: Vec::new(),
            picker: None,
            want_picker: false,
            app: None,
            from_apps: false,
            severity: 0,
            window: DEFAULT_WINDOW,
            requests: false,
            entries: Vec::new(),
            more: false,
            read: LoadState::Idle,
            loads: 0,
            log,
            following: false,
            follows: 0,
            follower: None,
            next_poll: Instant::now(),
            pause: POLL_SECS,
        }
    }

    fn load_apps(&mut self, ctx: &mut Ctx) {
        self.apps = LoadState::Loading;
        let gcp = ctx.env().gcp.clone();
        ctx.spawn(move || {
            Msg::Apps(run::all_services(&gcp).map(|services| {
                services.iter().map(|s| (s.id().to_string(), format!("{} · {}", s.region(), s.revision()))).collect()
            }))
        });
    }

    /// The picker, now or once the apps are read.
    fn open_picker(&mut self, ctx: &mut Ctx) {
        match self.apps {
            LoadState::Loaded => self.show_picker(),
            LoadState::Loading => self.want_picker = true,
            LoadState::Idle | LoadState::Failed(_) => {
                self.want_picker = true;
                self.load_apps(ctx);
            }
        }
    }

    fn show_picker(&mut self) {
        self.want_picker = false;
        let current = self.app.as_deref();
        let cursor = self.names.iter().position(|(n, _)| Some(n.as_str()) == current).unwrap_or(0);
        let items = app_items(&self.names, current);
        self.picker = Some(Picker::new("Logs of which app?", "open", items, false).with_cursor(cursor));
    }

    fn query(&self) -> Option<Query> {
        let mut query = Query::app(self.app.as_deref()?, WINDOWS[self.window]);
        query.severity = SEVERITIES[self.severity];
        query.requests = self.requests;
        Some(query)
    }

    /// Reads the app's entries again, as the keys now ask; what is shown goes.
    fn read(&mut self, ctx: &mut Ctx) {
        let Some(query) = self.query() else { return };
        self.loads += 1;
        self.stop_polling();
        self.read = LoadState::Loading;
        self.entries.clear();
        self.more = false;
        self.log.reset();
        let (load, gcp) = (self.loads, ctx.env().gcp.clone());
        ctx.spawn(move || {
            let result = logging::entries(&gcp, &query.filter_at(&gcp, now_secs()), true, READ);
            Msg::Read { load, result }
        });
    }

    fn open_app(&mut self, app: &str, ctx: &mut Ctx) {
        self.app = Some(app.to_string());
        self.picker = None;
        self.want_picker = false;
        self.read(ctx);
    }

    /// What was read, in the order the mode asks: newest first, or oldest first and at the end
    /// while following.
    fn rebuild(&mut self) {
        self.log.reset();
        if self.following {
            self.log.extend(self.entries.iter().rev().flat_map(entry_lines));
        } else {
            self.log.extend(self.entries.iter().flat_map(entry_lines));
            self.log.show_start();
        }
    }

    fn stop_polling(&mut self) {
        self.follows += 1;
        self.follower = None;
        self.pause = POLL_SECS;
    }

    /// Follows on from what was read.
    fn start_polling(&mut self, ctx: &mut Ctx) {
        let Some(query) = self.query() else { return };
        self.stop_polling();
        let now = now_secs();
        let conditions = query.conditions(&ctx.env().gcp);
        let since_start = query.since.map(|s| now - s);
        self.follower = Some(Follower::after(conditions, &self.entries, self.more, since_start, now));
        self.next_poll = Instant::now() + Duration::from_secs(POLL_SECS);
    }

    fn toggle_follow(&mut self, ctx: &mut Ctx) {
        self.following = !self.following;
        self.rebuild();
        self.stop_polling();
        if self.following && matches!(self.read, LoadState::Loaded) {
            self.start_polling(ctx);
        }
    }

    fn polled(&mut self, follower: Follower, result: gcp::Result<Vec<Entry>>, ctx: &mut Ctx) {
        self.follower = Some(follower);
        match result {
            Ok(new) => {
                self.pause = POLL_SECS;
                self.log.extend(new.iter().flat_map(entry_lines));
                self.entries.splice(0..0, new.into_iter().rev());
                self.entries.truncate(KEEP);
            }
            Err(e) if e.is_retryable() => {
                if self.pause == POLL_SECS {
                    ctx.toast(Level::Warn, format!("Could not read new entries; trying again: {}", e.short()));
                }
                self.pause = (self.pause * 2).min(MAX_BACKOFF_SECS);
            }
            Err(e) => {
                self.following = false;
                self.stop_polling();
                ctx.toast(Level::Error, format!("Stopped following: {}", e.short()));
            }
        }
        self.next_poll = Instant::now() + Duration::from_secs(self.pause);
    }

    /// Back from the viewer: to Apps when it came from there, else to choose another app.
    fn leave(&mut self, ctx: &mut Ctx) {
        self.log.zoomed = true;
        self.app = None;
        self.following = false;
        self.stop_polling();
        self.read = LoadState::Idle;
        self.loads += 1;
        self.entries.clear();
        self.log.reset();
        if std::mem::take(&mut self.from_apps) {
            ctx.jump("Apps", "");
        } else {
            self.open_picker(ctx);
        }
    }

    fn viewer_key(&mut self, key: KeyEvent, ctx: &mut Ctx) {
        if !self.log.searching() {
            match key.code {
                KeyCode::Char('s') => {
                    self.severity = (self.severity + 1) % SEVERITIES.len();
                    return self.read(ctx);
                }
                KeyCode::Char('w') => {
                    self.window = (self.window + 1) % WINDOWS.len();
                    return self.read(ctx);
                }
                KeyCode::Char('t') => {
                    self.requests = !self.requests;
                    return self.read(ctx);
                }
                KeyCode::Char('f') => return self.toggle_follow(ctx),
                KeyCode::Char('a') => return self.open_picker(ctx),
                KeyCode::Char('r') => return self.read(ctx),
                _ => {}
            }
        }
        match self.log.key(key) {
            LogKey::Copy => {
                let ok = crate::platform::copy_to_clipboard(&self.log.lines().join("\n"));
                let done = format!("Copied {} lines", self.log.lines().len());
                ctx.toast(
                    if ok { Level::Success } else { Level::Warn },
                    if ok { &done } else { "Clipboard not available" },
                );
            }
            _ if !self.log.zoomed => self.leave(ctx),
            _ => {}
        }
    }

    /// `orders · ● following · WARNING+ · last 1h · requests log`, for the viewer's status line:
    /// what matters most first, as a narrow terminal cuts the end.
    fn title(&self, tick: usize) -> String {
        let mut parts = vec![self.app.clone().unwrap_or_default()];
        match self.read {
            LoadState::Loading => parts.push(format!("{} reading", theme::spinner(tick))),
            LoadState::Loaded if self.following => parts.push("● following".into()),
            _ => {}
        }
        parts.push(SEVERITIES[self.severity].map_or("all severities".to_string(), |s| format!("{s}+")));
        parts.push(format!("last {}", logs::fmt_window(WINDOWS[self.window])));
        if self.requests {
            parts.push("requests log".into());
        }
        let problems = self.entries.iter().filter(|e| e.is_problem()).count();
        if problems > 0 {
            parts.push(format!("{problems} warnings or worse"));
        }
        if self.more {
            parts.push(format!("newest {}", self.entries.len()));
        }
        parts.join(" · ")
    }

    /// What an empty log has to say.
    fn empty_text(&self) -> Option<Line<'static>> {
        if !self.log.lines().is_empty() {
            return None;
        }
        match &self.read {
            LoadState::Idle => None,
            LoadState::Loading => Some(Line::styled("Reading…", theme::muted())),
            LoadState::Failed(e) => Some(Line::styled(e.short(), Style::new().fg(theme::ERR))),
            LoadState::Loaded => {
                let text = self.query().map(|q| q.nothing_found(&self.env_name, self.more)).unwrap_or_default();
                Some(Line::styled(text, theme::muted()))
            }
        }
    }

    fn render_idle(&self, f: &mut Frame, area: Rect, view: &View) {
        let text = match &self.apps {
            LoadState::Failed(e) => Line::styled(e.short(), Style::new().fg(theme::ERR)),
            LoadState::Loading | LoadState::Idle => {
                Line::styled(format!("{} Reading the apps…", theme::spinner(view.tick)), theme::muted())
            }
            LoadState::Loaded if self.names.is_empty() => {
                Line::styled(format!("No Cloud Run services in {}", self.env_name), theme::muted())
            }
            LoadState::Loaded => Line::styled("Press a to choose an app", theme::muted()),
        };
        f.render_widget(widgets::placeholder(text).block(widgets::panel(" Logs ", true)), area);
    }
}

impl Module for Logs {
    fn title(&self) -> &'static str {
        "Logs"
    }

    fn description(&self) -> &'static str {
        "Cloud Logging · what an app wrote, severity, search, errors, follow"
    }

    fn reset(&mut self, ctx: &mut Ctx) {
        *self = Logs::new();
        self.env_name = ctx.env().name.clone();
    }

    fn activate(&mut self, ctx: &mut Ctx) {
        if self.app.is_none() && self.picker.is_none() {
            self.open_picker(ctx);
        }
    }

    fn open(&mut self, target: &str, ctx: &mut Ctx) {
        self.from_apps = true;
        self.following = false;
        self.open_app(target, ctx);
    }

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled {
        if let Some(picker) = &mut self.picker {
            match picker.handle(key) {
                Outcome::Pending => {}
                Outcome::Cancel => self.picker = None,
                Outcome::Done(chosen) => {
                    if let Some((name, _)) = chosen.first().and_then(|&i| self.names.get(i)).cloned() {
                        self.from_apps = false;
                        self.open_app(&name, ctx);
                    }
                }
            }
            return Handled::Yes;
        }
        if self.app.is_some() {
            self.viewer_key(key, ctx);
            return Handled::Yes;
        }
        match key.code {
            KeyCode::Char('a') | KeyCode::Enter => self.open_picker(ctx),
            KeyCode::Char('r') => {
                self.want_picker = true;
                self.load_apps(ctx);
            }
            _ => return Handled::No,
        }
        Handled::Yes
    }

    fn task(&mut self, payload: Payload, ctx: &mut Ctx) {
        let Ok(msg) = payload.downcast::<Msg>() else { return };
        match *msg {
            Msg::Apps(Ok(names)) => {
                self.names = names;
                self.apps = LoadState::Loaded;
                if self.want_picker {
                    self.show_picker();
                }
            }
            Msg::Apps(Err(e)) => {
                self.want_picker = false;
                if self.app.is_some() {
                    ctx.toast(Level::Error, e.short());
                }
                self.apps = LoadState::Failed(e);
            }
            Msg::Read { load, .. } if load != self.loads => {}
            Msg::Read { result, .. } => match result {
                Ok(read) => {
                    self.entries = read.list;
                    self.more = read.more;
                    self.read = LoadState::Loaded;
                    self.rebuild();
                    if self.following {
                        self.start_polling(ctx);
                    }
                }
                Err(e) => self.read = LoadState::Failed(e),
            },
            Msg::Polled { follow, .. } if follow != self.follows => {}
            Msg::Polled { follower, result, .. } => self.polled(follower, result, ctx),
        }
    }

    /// Polls only while on screen; what arrives meanwhile is read on coming back.
    fn tick(&mut self, ctx: &mut Ctx, active: bool) {
        if !active || !self.following || Instant::now() < self.next_poll {
            return;
        }
        let Some(mut follower) = self.follower.take() else { return };
        let (follow, gcp) = (self.follows, ctx.env().gcp.clone());
        ctx.spawn(move || {
            let result = follower.poll(&gcp);
            Msg::Polled { follow, follower, result }
        });
    }

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View) {
        if self.app.is_some() {
            let title = self.title(view.tick);
            self.log.render_zoomed(f, area, &title);
            if let Some(text) = self.empty_text() {
                let body = Rect { height: area.height.saturating_sub(1), ..area };
                f.render_widget(widgets::placeholder(text), body);
            }
        } else {
            self.render_idle(f, area, view);
        }
        if let Some(picker) = &mut self.picker {
            let rect = widgets::popup_area(area, 84, picker.height(area.height.saturating_sub(4)));
            picker.render(f, rect, theme::ACCENT);
        }
    }

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        if self.picker.is_some() {
            return vec![("↑↓", "move"), ("Enter", "open"), ("Esc", "cancel")];
        }
        if self.log.searching() {
            return vec![("Enter", "search"), ("Esc", "cancel")];
        }
        if self.app.is_some() {
            return vec![
                ("s", "severity"),
                ("w", "time window"),
                ("f", "follow"),
                ("t", "requests log"),
                ("a", "another app"),
                ("r", "read again"),
                ("/", "search"),
                ("n/N", "next/prev"),
                ("e/E", "error"),
                ("y", "copy"),
                ("Esc", "back"),
            ];
        }
        vec![("a", "choose an app"), ("r", "read the apps again")]
    }

    fn animating(&self) -> bool {
        matches!(self.read, LoadState::Loading)
            || (self.app.is_none() && self.picker.is_none() && matches!(self.apps, LoadState::Loading))
    }

    fn capturing(&self) -> bool {
        self.picker.is_some() || (self.app.is_some() && self.log.searching())
    }

    /// In the viewer `q` goes back and `e` jumps to an error; the other global keys still work.
    fn takes_key(&self, key: &KeyEvent) -> bool {
        self.capturing() || (self.app.is_some() && matches!(key.code, KeyCode::Char('q' | 'e')))
    }

    /// The viewer leaves the mouse to the terminal, so text can be selected and copied.
    fn wants_mouse(&self) -> bool {
        self.app.is_none() || self.picker.is_some()
    }

    fn paste(&mut self, text: &str, _ctx: &mut Ctx) {
        match &mut self.picker {
            Some(picker) => picker.paste(text),
            None => self.log.paste(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Environment, Loaded, Source};
    use crate::gcp::GcpError;
    use crate::gcp::mock::{MockServer, Reply};
    use crate::tui::module::{AppEvent, Env, Shared};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyModifiers;
    use std::sync::Arc;
    use std::sync::mpsc::{Receiver, channel};

    /// A module with its shell's shared state, against a fake Google Cloud.
    struct Rig {
        m: Logs,
        shared: Shared,
        rx: Receiver<AppEvent>,
        fake: MockServer,
    }

    const SERVICES: &str = r#"{"services":[
        {"name":"projects/acme/locations/us-east1/services/orders","latestReadyRevision":"x/revisions/orders-00001-abc"},
        {"name":"projects/acme/locations/us-east1/services/shop","latestReadyRevision":"x/revisions/shop-00002-def"}]}"#;

    const PAGE: &str = r#"{"entries":[
        {"insertId":"3","timestamp":"2026-11-05T10:00:03Z","severity":"CRITICAL","textPayload":"on fire"},
        {"insertId":"2","timestamp":"2026-11-05T10:00:02Z","severity":"ERROR","textPayload":"boom\n\tat main"},
        {"insertId":"1","timestamp":"2026-11-05T10:00:01Z","textPayload":"hello"}]}"#;

    fn rig() -> Rig {
        let fake = MockServer::start(vec![]);
        fake.route("/services", Reply::json(200, SERVICES));
        let (tx, rx) = channel();
        let config = Environment { project: "acme".into(), region: "us-east1".into(), ..Default::default() };
        let mut env = Env::new("dev", &config);
        env.gcp = crate::gcp::Gcp::with("acme", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()));
        let shared = Shared {
            env: Arc::new(env),
            loaded: Loaded { config: Config::default(), source: Source::Gcloud("x".into()) },
            tx,
            generation: 1,
            toasts: Vec::new(),
            jump: None,
            switch_env: None,
            login: false,
        };
        let mut rig = Rig { m: Logs::new(), shared, rx, fake };
        rig.with(|m, ctx| m.reset(ctx));
        rig
    }

    impl Rig {
        fn with<R>(&mut self, f: impl FnOnce(&mut Logs, &mut Ctx) -> R) -> R {
            let mut ctx = Ctx { shared: &mut self.shared, module: 0 };
            f(&mut self.m, &mut ctx)
        }

        /// Hands the module what one background job reports.
        fn settle(&mut self) {
            let event = self.rx.recv_timeout(Duration::from_secs(5)).expect("a job reports");
            if let AppEvent::Task { payload, .. } = event {
                self.with(|m, ctx| m.task(payload, ctx));
            }
        }

        fn press(&mut self, code: KeyCode) {
            let key = KeyEvent::new(code, KeyModifiers::NONE);
            self.with(|m, ctx| m.key(key, ctx));
        }

        fn char(&mut self, c: char) {
            self.press(KeyCode::Char(c));
        }

        /// Opens `shop` as the Apps module's `l` does.
        fn jump_to_shop(&mut self) {
            self.fake.push(Reply::json(200, PAGE));
            self.with(|m, ctx| m.open("shop", ctx));
            self.settle();
        }

        fn filter(&self, i: usize) -> String {
            let reads: Vec<_> = self.fake.requests().into_iter().filter(|r| r.path.contains("entries:list")).collect();
            let body: serde_json::Value = serde_json::from_str(&reads[i].body).unwrap();
            body["filter"].as_str().unwrap().to_string()
        }

        fn screen(&mut self) -> String {
            let mut t = Terminal::new(TestBackend::new(100, 20)).unwrap();
            let m = &mut self.m;
            t.draw(|f| {
                let area = f.area();
                m.render(f, area, &View { tick: 0, now: 0.0 });
            })
            .unwrap();
            let buf = t.backend().buffer().clone();
            (0..20)
                .map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        }
    }

    #[test]
    fn chooses_an_app_then_shows_its_entries_newest_first() {
        let mut r = rig();
        r.with(|m, ctx| m.activate(ctx));
        assert!(r.screen().contains("Reading the apps"));
        assert!(!r.m.capturing(), "global keys work while the apps are read");
        r.settle();
        assert!(r.m.capturing(), "the picker has the keys");
        let picker = r.screen();
        assert!(
            picker.contains("Logs of which app?") && picker.contains("orders") && picker.contains("shop"),
            "{picker}"
        );
        for c in "sho".chars() {
            r.char(c);
        }
        r.fake.push(Reply::json(200, PAGE));
        r.press(KeyCode::Enter);
        assert!(r.m.picker.is_none());
        assert!(r.screen().contains("Reading…"));
        r.settle();
        assert!(r.filter(0).contains("service_name=\"shop\""));
        assert!(r.filter(0).contains("logName!="), "what the app wrote: {}", r.filter(0));
        assert_eq!(
            r.m.log.lines(),
            [
                "2026-11-05 10:00:03 UTC  CRITICAL  ✖ on fire",
                "2026-11-05 10:00:02 UTC  ERROR     boom",
                "        at main",
                "2026-11-05 10:00:01 UTC  DEFAULT   hello",
            ]
        );
        let screen = r.screen();
        assert!(screen.contains("on fire") && screen.contains("shop · all severities · last 1h"), "{screen}");
        assert!(screen.contains("2 warnings or worse"), "{screen}");
    }

    #[test]
    fn keys_ask_for_other_entries() {
        let mut r = rig();
        r.jump_to_shop();
        r.fake.push(Reply::json(200, PAGE));
        r.char('s');
        r.settle();
        assert!(!r.filter(0).contains("severity>="));
        assert!(r.filter(1).contains("severity>=INFO"), "{}", r.filter(1));
        r.fake.push(Reply::json(200, PAGE));
        r.char('w');
        r.settle();
        assert!(r.screen().contains("INFO+ · last 6h"));
        r.fake.push(Reply::json(200, PAGE));
        r.char('t');
        r.settle();
        assert!(
            r.filter(3).contains("logName=\"projects/acme/logs/run.googleapis.com%2Frequests\""),
            "{}",
            r.filter(3)
        );
        assert!(r.screen().contains("requests log"));
        r.fake.push(Reply::json(200, "{}"));
        r.char('r');
        r.settle();
        let screen = r.screen();
        assert!(screen.contains("No entries for shop"), "{screen}");
    }

    #[test]
    fn the_viewers_keys_and_a_failed_read() {
        let mut r = rig();
        r.jump_to_shop();
        assert!(r.m.takes_key(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE)));
        assert!(r.m.takes_key(&KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
        assert!(!r.m.takes_key(&KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)), "the palette still opens");
        assert!(!r.m.takes_key(&KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE)), "and help");
        assert!(!r.m.wants_mouse(), "text can be selected");
        // While searching, s is a letter, not a key.
        r.char('/');
        assert!(r.m.capturing());
        assert!(r.m.takes_key(&KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE)));
        r.with(|m, ctx| m.paste("he", ctx));
        r.char('s');
        r.press(KeyCode::Enter);
        assert_eq!(r.m.severity, 0);
        assert!(!r.m.log.searching());
        r.m.read = LoadState::Failed(GcpError::LoginRequired { detail: "x".into() });
        r.m.log.reset();
        assert!(r.screen().contains("sign in again"));
    }

    #[test]
    fn follows_oldest_first_without_repeats_then_stops() {
        let mut r = rig();
        r.jump_to_shop();
        r.char('f');
        assert_eq!(r.m.log.lines()[0], "2026-11-05 10:00:01 UTC  DEFAULT   hello", "oldest first");
        assert!(r.m.log.following(), "at the end");
        assert!(r.screen().contains("● following"));
        // Not yet: the first poll comes after a pause.
        r.with(|m, ctx| m.tick(ctx, true));
        assert!(r.rx.try_recv().is_err());
        r.m.next_poll = Instant::now();
        // Not on screen: no polling.
        r.with(|m, ctx| m.tick(ctx, false));
        assert!(r.m.follower.is_some());
        r.fake.push(Reply::json(
            200,
            r#"{"entries":[{"insertId":"3","timestamp":"2026-11-05T10:00:03Z","textPayload":"on fire"},
                           {"insertId":"4","timestamp":"2026-11-05T10:00:09Z","textPayload":"new"}]}"#,
        ));
        r.with(|m, ctx| m.tick(ctx, true));
        assert!(r.m.follower.is_none(), "one poll at a time");
        r.with(|m, ctx| m.tick(ctx, true));
        r.settle();
        assert_eq!(r.m.log.lines().last().unwrap(), "2026-11-05 10:00:09 UTC  DEFAULT   new");
        assert_eq!(r.m.log.lines().iter().filter(|l| l.contains("on fire")).count(), 1, "never twice");
        assert!(r.m.next_poll > Instant::now(), "the next one waits");
        let reads = r.fake.requests().len();
        // Off: newest first again, without reading, and an answer on its way is dropped.
        r.m.next_poll = Instant::now();
        r.fake.push(Reply::json(200, "{}"));
        r.with(|m, ctx| m.tick(ctx, true));
        r.char('f');
        r.settle();
        assert!(!r.m.following && r.m.follower.is_none());
        assert_eq!(r.m.log.lines()[0], "2026-11-05 10:00:09 UTC  DEFAULT   new");
        assert_eq!(r.fake.requests().len(), reads + 1, "only the poll that was running");
    }

    #[test]
    fn following_survives_a_passing_failure_and_stops_on_a_lasting_one() {
        let mut r = rig();
        r.jump_to_shop();
        r.char('f');
        r.m.next_poll = Instant::now();
        r.fake.push(Reply::json(400, r#"{"error":{"status":"INVALID_ARGUMENT","message":"bad filter"}}"#));
        r.with(|m, ctx| m.tick(ctx, true));
        r.settle();
        assert!(!r.m.following, "it would fail every time");
        assert!(r.shared.toasts.iter().any(|t| t.text.contains("Stopped following")));

        let (follower, e) = (
            Follower::after("c".into(), &[], false, None, 0.0),
            GcpError::Api { status: 503, code: "UNAVAILABLE".into(), message: "x".into() },
        );
        r.char('f');
        r.with(|m, ctx| m.polled(follower, Err(e), ctx));
        assert!(r.m.following);
        assert_eq!(r.m.pause, 2 * POLL_SECS, "waits longer");
        assert!(r.shared.toasts.iter().any(|t| t.text.contains("trying again")));
    }

    #[test]
    fn back_goes_where_it_came_from_and_answers_of_old_reads_are_dropped() {
        let mut r = rig();
        r.jump_to_shop();
        let replaced = r.m.loads;
        r.fake.push(Reply::json(200, PAGE));
        r.char('s');
        r.settle();
        // The read it replaced answers last.
        let old = Entry { insert_id: "o".into(), text_payload: "old".into(), ..Entry::default() };
        let late = Msg::Read { load: replaced, result: Ok(Entries { list: vec![old], more: false }) };
        r.with(|m, ctx| m.task(Box::new(late), ctx));
        assert!(r.m.log.lines().iter().all(|l| !l.contains("old")), "{:?}", r.m.log.lines());
        r.press(KeyCode::Esc);
        assert_eq!(r.shared.jump.take().map(|j| j.0), Some("Apps"));
        assert!(r.m.app.is_none());
        // Chosen from the picker, back goes to the picker.
        r.with(|m, ctx| m.activate(ctx));
        r.settle();
        r.fake.push(Reply::json(200, PAGE));
        r.press(KeyCode::Enter);
        r.settle();
        r.char('q');
        assert!(r.shared.jump.is_none());
        assert!(r.m.picker.is_some(), "the picker again");
        r.press(KeyCode::Esc);
        assert!(r.m.picker.is_none() && !r.m.capturing());
        assert!(r.screen().contains("Press a to choose an app"));
    }

    #[test]
    fn l_in_apps_asks_for_the_logs_of_the_selected_app() {
        let mut r = rig();
        r.fake.route("timeSeries", Reply::json(200, "{}"));
        let mut apps = crate::modules::apps::Apps::new();
        let mut ctx = Ctx { shared: &mut r.shared, module: 0 };
        apps.reset(&mut ctx);
        apps.activate(&mut ctx);
        // The services, the end of the list and the last day's metrics.
        for _ in 0..3 {
            if let Ok(AppEvent::Task { payload, .. }) = r.rx.recv_timeout(Duration::from_secs(5)) {
                apps.task(payload, &mut ctx);
            }
        }
        apps.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &mut ctx);
        assert_eq!(r.shared.jump, Some(("Logs", "orders".to_string())));
    }

    #[test]
    fn apps_that_cannot_be_read_can_be_read_again() {
        let mut r = rig();
        let fake = MockServer::start(vec![Reply::json(
            403,
            r#"{"error":{"status":"PERMISSION_DENIED","message":"no access"}}"#,
        )]);
        let config = Environment { project: "acme".into(), region: "us-east1".into(), ..Default::default() };
        let mut env = Env::new("dev", &config);
        env.gcp = crate::gcp::Gcp::with("acme", "us-east1", crate::gcp::auth::tests::fixed("t"), Some(fake.url()));
        r.shared.env = Arc::new(env);
        r.with(|m, ctx| m.activate(ctx));
        r.settle();
        assert!(r.screen().contains("PERMISSION_DENIED"));
        fake.push(Reply::json(200, SERVICES));
        r.char('r');
        r.settle();
        assert!(r.m.picker.is_some());
    }
}
