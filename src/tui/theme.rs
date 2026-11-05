//! Colours and symbols. Only the 16 ANSI colours are used so the UI follows the
//! terminal's own palette (light or dark) and works in every terminal.

use ratatui::style::{Color, Modifier, Style};

pub const ACCENT: Color = Color::Cyan;
pub const OK: Color = Color::Green;
pub const ERR: Color = Color::Red;
pub const WARN: Color = Color::Yellow;
pub const MUTED: Color = Color::DarkGray;

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn spinner(tick: usize) -> &'static str {
    SPINNER[tick % SPINNER.len()]
}

pub fn muted() -> Style {
    Style::new().fg(MUTED)
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

pub fn accent() -> Style {
    Style::new().fg(ACCENT)
}

pub fn selected() -> Style {
    Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD)
}

pub fn key() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

/// Production environments are shown in red, the rest in green.
pub fn env_color(production: bool) -> Color {
    if production { ERR } else { OK }
}
