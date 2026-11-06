//! The module contract. Each screen (Apps, Costs, Builds, …) is a `Module`;
//! the app owns the terminal, the active environment and the event loop, and hands
//! each module a `Ctx` to start background work and talk back to the user.
//!
//! Background work runs on plain threads. A job returns (or emits) a value of the
//! module's own message type; the app routes it back to that module's `task`, where
//! it is recovered with `downcast`. Results from a previous environment are dropped.

use std::any::Any;
use std::marker::PhantomData;
use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::Instant;

use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyEvent, MouseEvent};
use ratatui::layout::Rect;
use ratatui::style::Color;

use crate::config::{Config, Environment, Loaded};
use crate::gcp::Gcp;

pub type Payload = Box<dyn Any + Send>;

pub enum AppEvent {
    Input(Event),
    Task { module: usize, generation: u64, payload: Payload },
}

/// The environment every module works against.
pub struct Env {
    pub name: String,
    pub project: String,
    /// Empty: every region.
    pub region: String,
    /// Changes ask for the environment's name to be typed.
    pub production: bool,
    /// The environment as configured (billing, apps convention…).
    pub config: Environment,
    pub gcp: Arc<Gcp>,
}

impl Env {
    pub fn new(name: &str, config: &Environment) -> Env {
        Env {
            name: name.to_string(),
            project: config.project.clone(),
            region: config.region.clone(),
            production: config.is_production(name),
            config: config.clone(),
            gcp: Gcp::new(&config.project, &config.region),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Info,
    Success,
    Warn,
    Error,
}

pub struct Toast {
    pub level: Level,
    pub text: String,
    pub at: Instant,
}

/// App state modules may touch, separate from the module list so both can be borrowed.
pub struct Shared {
    pub env: Arc<Env>,
    pub loaded: Loaded,
    pub tx: Sender<AppEvent>,
    pub generation: u64,
    pub toasts: Vec<Toast>,
    /// A module asked to show another one: (module title, what to open there).
    pub jump: Option<(&'static str, String)>,
    /// A module asked to switch to this environment (before the jump, if any).
    pub switch_env: Option<String>,
    /// `L` asked for a sign-in (`gcloud auth login`); the run loop suspends the UI for it.
    pub login: bool,
}

/// Per-frame values for rendering.
pub struct View {
    /// Animation counter, +1 every 100 ms.
    pub tick: usize,
    /// Unix seconds.
    pub now: f64,
}

pub enum Handled {
    Yes,
    No,
}

pub struct Ctx<'a> {
    pub shared: &'a mut Shared,
    pub module: usize,
}

/// Sends several messages from one background job (progressive loading, streams).
pub struct Emitter<M> {
    tx: Sender<AppEvent>,
    module: usize,
    generation: u64,
    _msg: PhantomData<fn(M)>,
}

impl<M: Send + 'static> Emitter<M> {
    /// Returns false once the app is gone, so long jobs can stop.
    pub fn emit(&self, msg: M) -> bool {
        self.tx
            .send(AppEvent::Task { module: self.module, generation: self.generation, payload: Box::new(msg) })
            .is_ok()
    }
}

impl Ctx<'_> {
    pub fn env(&self) -> Arc<Env> {
        self.shared.env.clone()
    }

    pub fn config(&self) -> &Config {
        &self.shared.loaded.config
    }

    fn emitter<M: Send + 'static>(&self) -> Emitter<M> {
        Emitter {
            tx: self.shared.tx.clone(),
            module: self.module,
            generation: self.shared.generation,
            _msg: PhantomData,
        }
    }

    /// Runs `job` on a background thread and delivers its result to this module.
    #[allow(dead_code)] // single answers (an app's detail, an action), from M2
    pub fn spawn<M: Send + 'static>(&self, job: impl FnOnce() -> M + Send + 'static) {
        let emitter = self.emitter::<M>();
        std::thread::spawn(move || {
            emitter.emit(job());
        });
    }

    /// Like `spawn`, for jobs that report several times.
    pub fn spawn_stream<M: Send + 'static>(&self, job: impl FnOnce(Emitter<M>) + Send + 'static) {
        let emitter = self.emitter::<M>();
        std::thread::spawn(move || job(emitter));
    }

    pub fn toast(&mut self, level: Level, text: impl Into<String>) {
        let text = crate::util::sanitize(&text.into());
        self.shared.toasts.push(Toast { level, text, at: Instant::now() });
    }

    /// Switches to the module titled `module` and asks it to open `target`
    /// (an app → its builds in Builds, for example).
    pub fn jump(&mut self, module: &'static str, target: impl Into<String>) {
        self.shared.jump = Some((module, target.into()));
    }

    /// Switches the whole app to environment `name` (a resource of another project, say).
    #[allow(dead_code)] // for modules that span environments (Costs)
    pub fn switch_env(&mut self, name: impl Into<String>) {
        self.shared.switch_env = Some(name.into());
    }

    /// Toast plus a desktop notification when enabled in the config.
    pub fn notify(&mut self, level: Level, title: &str, body: &str) {
        if self.config().defaults.notifications {
            crate::platform::notify(title, body);
        }
        self.toast(level, format!("{title} — {body}"));
    }
}

pub trait Module {
    /// Name in the header and the module palette.
    fn title(&self) -> &'static str;

    /// One line for the module palette: the Google Cloud service and what you can do.
    fn description(&self) -> &'static str {
        ""
    }

    /// The environment changed (also called once at startup): drop old data.
    fn reset(&mut self, ctx: &mut Ctx);

    /// The module became visible. Load lazily here.
    fn activate(&mut self, _ctx: &mut Ctx) {}

    fn key(&mut self, key: KeyEvent, ctx: &mut Ctx) -> Handled;

    fn mouse(&mut self, _event: MouseEvent, _ctx: &mut Ctx) -> Handled {
        Handled::No
    }

    /// A background job of this module finished or reported progress.
    fn task(&mut self, payload: Payload, ctx: &mut Ctx);

    /// Called on every loop iteration; `active` says whether the module is visible.
    fn tick(&mut self, _ctx: &mut Ctx, _active: bool) {}

    fn render(&mut self, f: &mut Frame, area: Rect, view: &View);

    /// Key hints for the footer and the help screen.
    fn hints(&self) -> Vec<(&'static str, &'static str)>;

    /// A short count for the header (apps whose cost spiked, failed builds) and its colour. It
    /// shows next to the module's name even while another module is open.
    fn badge(&self) -> Option<(String, Color)> {
        None
    }

    /// True while something animates, so the app redraws at 10 fps instead of 1.
    fn animating(&self) -> bool {
        false
    }

    /// True while a text field or dialog has focus; global shortcuts are then disabled.
    fn capturing(&self) -> bool {
        false
    }

    /// False while the module wants the terminal to handle the mouse (text selection).
    fn wants_mouse(&self) -> bool {
        true
    }

    /// Text pasted into the terminal (bracketed paste), for the focused field.
    fn paste(&mut self, _text: &str, _ctx: &mut Ctx) {}

    /// Another module sent the user here to see `target` (see [`Ctx::jump`]).
    fn open(&mut self, _target: &str, _ctx: &mut Ctx) {}
}
