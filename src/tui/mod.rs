//! Interactive terminal UI.

mod app;
#[allow(dead_code)] // diagrams (load balancer → backend → service), from M2
pub mod canvas;
#[allow(dead_code)] // the Apps health charts (M2) and Costs by day (M1)
pub mod chart;
#[allow(dead_code)] // actions that ask for values (min instances), from M2
pub mod form;
#[allow(dead_code)] // build logs and Cloud Logging (M3)
pub mod logview;
pub mod module;
pub mod picker;
pub mod theme;
pub mod widgets;

use std::io::stdout;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui::crossterm::terminal::{Clear, ClearType, EnterAlternateScreen, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};

use crate::config::Loaded;
use module::AppEvent;

/// The keyboard reader's switch: while paused it reads nothing, so another program (gcloud
/// during a sign-in) gets every key typed.
#[derive(Clone, Default)]
struct Input {
    paused: Arc<AtomicBool>,
    /// Set by the reader once it has seen `paused` and stopped polling.
    idle: Arc<AtomicBool>,
}

impl Input {
    /// Reads terminal events into `tx` on a thread, in short polls so a pause takes effect
    /// within one.
    fn start(&self, tx: mpsc::Sender<AppEvent>) {
        let input = self.clone();
        std::thread::spawn(move || {
            loop {
                input.idle.store(false, Ordering::SeqCst);
                if input.paused.load(Ordering::SeqCst) {
                    input.idle.store(true, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                match event::poll(Duration::from_millis(50)) {
                    Ok(true) => match event::read() {
                        Ok(ev) => {
                            if tx.send(AppEvent::Input(ev)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    },
                    Ok(false) => {}
                    Err(_) => break,
                }
            }
        });
    }

    /// Stops reading and waits (briefly) until the reader is out of its poll.
    fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
        let since = Instant::now();
        while !self.idle.load(Ordering::SeqCst) && since.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
}

/// Opens the UI in `start_env`, or in the current environment (see `use`).
pub fn run(loaded: Loaded, start_env: Option<&str>) -> Result<()> {
    let env_name = match start_env {
        Some(name) => {
            loaded.config.env(name)?;
            name.to_string()
        }
        None => loaded.config.default_env().map(str::to_string).ok_or_else(|| {
            anyhow!("no environments: create ~/.gcli/config.yaml, or set a project with `gcloud config set project ID`")
        })?,
    };

    let (tx, rx) = mpsc::channel::<AppEvent>();
    let input = Input::default();
    input.start(tx.clone());

    let mut terminal = ratatui::init();
    let _ = execute!(stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut mouse_on = true;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        previous_hook(info);
    }));

    let mut app = app::App::new(loaded, &env_name, tx, crate::modules::all());
    let result = (|| -> Result<()> {
        loop {
            terminal.draw(|f| app.render(f))?;
            let wait = if app.animating() { Duration::from_millis(100) } else { Duration::from_secs(1) };
            match rx.recv_timeout(wait) {
                Ok(ev) => {
                    app.handle(ev);
                    while let Ok(ev) = rx.try_recv() {
                        app.handle(ev);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            app.tick();
            if app.quit {
                break;
            }
            if app.take_login() {
                let result = suspended(&input, crate::gcp::gcloud::login);
                // Back in a blank alternate screen: forget what was drawn so the next frame is
                // drawn whole. (`Terminal::clear` would too, but it asks the terminal where the
                // cursor is and fails when no answer comes.)
                let _ = execute!(stdout(), Clear(ClearType::All));
                terminal.swap_buffers();
                mouse_on = false;
                app.login_finished(result);
            }
            // Full-screen logs leave the mouse to the terminal, so text can be selected.
            let wanted = app.mouse_wanted();
            if wanted != mouse_on {
                let _ = if wanted {
                    execute!(stdout(), EnableMouseCapture)
                } else {
                    execute!(stdout(), DisableMouseCapture)
                };
                mouse_on = wanted;
            }
        }
        Ok(())
    })();

    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    result
}

/// Leaves the UI for the plain terminal (cooked mode, the normal screen, no mouse capture),
/// runs `job` there with the keyboard, and comes back. When `job` fails its output stays on
/// screen until Enter, so it can be read.
fn suspended(input: &Input, job: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    input.pause();
    let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    let _ = execute!(stdout(), cursor::Show);
    println!("Signing in to Google Cloud with gcloud; gcli comes back when it is done.\n");
    let result = job();
    if let Err(e) = &result {
        println!("\n✗ {}\n\nPress Enter to go back to gcli.", crate::util::sanitize(e));
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
    let _ = enable_raw_mode();
    let _ = execute!(stdout(), EnterAlternateScreen, EnableBracketedPaste);
    input.resume();
    result
}
