//! A grid of cells to draw diagrams in the terminal: box-drawing lines that join where they
//! meet (crossing lines merge into ┼, ├, ┴…) and text. Diagrams are drawn on a canvas as
//! large as they need and shown through [`Canvas::lines`], a window that can scroll (a load
//! balancer's path to its apps, say).

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::util::ellipsize;

/// The sides a line cell leaves through, combined with `|`: up, down, left, right.
pub const U: u8 = 1;
pub const D: u8 = 2;
pub const L: u8 = 4;
pub const R: u8 = 8;

#[derive(Clone, Copy, PartialEq)]
enum Cell {
    Empty,
    /// Which sides the line leaves through; crossing lines merge into ┼, ├, ┴…
    Line(u8, Style),
    Text(char, Style),
}

/// The character for each combination of sides, indexed by the bits.
const GLYPHS: [char; 16] = [' ', '│', '│', '│', '─', '╯', '╮', '┤', '─', '╰', '╭', '├', '─', '┴', '┬', '┼'];

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    cells: Vec<Cell>,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas { w, h, cells: vec![Cell::Empty; w * h] }
    }

    pub fn line(&mut self, x: usize, y: usize, bits: u8, style: Style) {
        if x < self.w && y < self.h {
            let cell = &mut self.cells[y * self.w + x];
            *cell = match *cell {
                Cell::Line(b, _) => Cell::Line(b | bits, style),
                _ => Cell::Line(bits, style),
            };
        }
    }

    /// A horizontal line whose ends turn into corners or junctions with what they meet.
    pub fn hline(&mut self, x0: usize, x1: usize, y: usize, style: Style) {
        for x in x0..=x1 {
            self.line(x, y, if x > x0 { L } else { 0 } | if x < x1 { R } else { 0 }, style);
        }
    }

    pub fn vline(&mut self, x: usize, y0: usize, y1: usize, style: Style) {
        for y in y0..=y1 {
            self.line(x, y, if y > y0 { U } else { 0 } | if y < y1 { D } else { 0 }, style);
        }
    }

    /// A vertical line that also continues above `y0` and below `y1`.
    pub fn through(&mut self, x: usize, y0: usize, y1: usize, style: Style) {
        for y in y0..=y1 {
            self.line(x, y, U | D, style);
        }
    }

    pub fn frame(&mut self, w: usize, h: usize, style: Style) {
        self.hline(0, w - 1, 0, style);
        self.hline(0, w - 1, h - 1, style);
        self.vline(0, 0, h - 1, style);
        self.vline(w - 1, 0, h - 1, style);
    }

    /// Writes `text` from `x`, at most `max` characters.
    pub fn text(&mut self, x: usize, y: usize, text: &str, max: usize, style: Style) {
        for (i, c) in ellipsize(text, max).chars().enumerate() {
            if x + i < self.w && y < self.h {
                self.cells[y * self.w + x + i] = Cell::Text(c, style);
            }
        }
    }

    pub fn blit(&mut self, src: &Canvas, dx: usize, dy: usize) {
        for y in 0..src.h {
            for x in 0..src.w {
                match src.cells[y * src.w + x] {
                    Cell::Empty => {}
                    Cell::Line(bits, style) => self.line(dx + x, dy + y, bits, style),
                    cell => {
                        if dx + x < self.w && dy + y < self.h {
                            self.cells[(dy + y) * self.w + dx + x] = cell;
                        }
                    }
                }
            }
        }
    }

    /// The part of the canvas from (`x0`, `y0`), `w` × `h` cells.
    pub fn lines(&self, x0: usize, y0: usize, w: usize, h: usize) -> Vec<Line<'static>> {
        (y0..(y0 + h).min(self.h))
            .map(|y| {
                let mut spans = Vec::new();
                let (mut text, mut style) = (String::new(), Style::new());
                for x in x0..(x0 + w).min(self.w) {
                    let (c, s) = match self.cells[y * self.w + x] {
                        Cell::Empty => (' ', Style::new()),
                        Cell::Line(bits, s) => (GLYPHS[usize::from(bits & 15)], s),
                        Cell::Text(c, s) => (c, s),
                    };
                    if s != style && !text.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut text), style));
                    }
                    style = s;
                    text.push(c);
                }
                spans.push(Span::styled(text, style));
                Line::from(spans)
            })
            .collect()
    }
}
