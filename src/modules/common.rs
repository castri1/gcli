//! Pieces shared by the modules.

use ratatui::text::{Line, Span};

use crate::gcp::GcpError;
use crate::tui::theme;
use crate::tui::widgets::TextInput;

pub enum LoadState {
    Idle,
    Loading,
    Loaded,
    Failed(GcpError),
}

pub use crate::util::matches_filter;

/// The search line above a list. Returns the cursor column while focused.
pub fn filter_bar<'a>(input: &'a TextInput, focused: bool, placeholder: &'a str) -> (Line<'a>, Option<u16>) {
    if focused {
        let (line, col) = input.line(" / ", placeholder);
        return (line, Some(col));
    }
    if input.value.is_empty() {
        return (Line::styled(format!(" / {placeholder}"), theme::muted()), None);
    }
    (
        Line::from(vec![
            Span::styled(" / ", theme::accent()),
            Span::raw(input.value.clone()),
            Span::styled("   Esc clears", theme::muted()),
        ]),
        None,
    )
}
