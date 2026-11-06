//! Pieces shared by the modules.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::apps::Tone;
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

pub fn tone_style(tone: Tone) -> Style {
    match tone {
        Tone::Plain => Style::new(),
        Tone::Muted => theme::muted(),
        Tone::Warn => Style::new().fg(theme::WARN),
        Tone::Bad => Style::new().fg(theme::ERR),
    }
}

pub fn spinner_line(tick: usize, text: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(theme::spinner(tick), theme::accent()),
        Span::styled(format!(" {text}"), theme::muted()),
    ])
}

/// Which columns fit in `width`: those that give way first (highest `drop`) go until the rest
/// fit; the last column (notes) takes what is left.
pub fn fit(widths: &[usize], drops: &[u8], width: usize) -> Vec<bool> {
    let mut keep = vec![true; widths.len()];
    let used = |keep: &[bool]| -> usize {
        let n = keep.iter().filter(|k| **k).count();
        // The notes need a little room to say anything.
        widths.iter().zip(keep).take(widths.len() - 1).filter(|(_, k)| **k).map(|(w, _)| *w).sum::<usize>()
            + 2 * n.saturating_sub(1)
            + 6
    };
    while used(&keep) > width {
        let Some(i) = (0..widths.len()).filter(|&i| keep[i] && drops[i] > 0).max_by_key(|&i| (drops[i], i)) else {
            break;
        };
        keep[i] = false;
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_give_way_in_order() {
        // APP, REV, REQ, NOTES; REV gives way first.
        let widths = [10, 9, 7, 20];
        let drops = [0, 6, 0, 0];
        assert_eq!(fit(&widths, &drops, 80), [true, true, true, true]);
        assert_eq!(fit(&widths, &drops, 30), [true, false, true, true]);
        // Nothing else may go: kept, and the name gives way when drawn.
        assert_eq!(fit(&widths, &drops, 10), [true, false, true, true]);
    }
}
