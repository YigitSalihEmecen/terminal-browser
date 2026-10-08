//! The cell model and the grid that holds it.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

use crate::width::{cluster_width, clusters};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug, Serialize, Deserialize)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub const BLACK: Rgb = Rgb(0, 0, 0);
    pub const WHITE: Rgb = Rgb(255, 255, 255);

    /// Straight alpha blend of `self` (alpha `a`, 0..=255) over `under`.
    pub fn over(self, a: u8, under: Rgb) -> Rgb {
        let mix =
            |f: u8, b: u8| ((f as u32 * a as u32 + b as u32 * (255 - a as u32) + 127) / 255) as u8;
        Rgb(
            mix(self.0, under.0),
            mix(self.1, under.1),
            mix(self.2, under.2),
        )
    }

    /// Perceptual-ish distance (weighted RGB, squared).
    pub fn dist2(self, o: Rgb) -> u32 {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2) as u32;
        2 * d(self.0, o.0) + 4 * d(self.1, o.1) + 3 * d(self.2, o.2)
    }
}

/// Text attribute bits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug, Serialize, Deserialize)]
pub struct Attrs(pub u8);

impl Attrs {
    pub const BOLD: Attrs = Attrs(1);
    pub const ITALIC: Attrs = Attrs(2);
    pub const UNDERLINE: Attrs = Attrs(4);
    pub const STRIKE: Attrs = Attrs(8);
    pub const DIM: Attrs = Attrs(16);
    pub const REVERSE: Attrs = Attrs(32);

    pub fn contains(self, o: Attrs) -> bool {
        self.0 & o.0 == o.0
    }
    pub fn with(self, o: Attrs) -> Attrs {
        Attrs(self.0 | o.0)
    }
    pub fn without(self, o: Attrs) -> Attrs {
        Attrs(self.0 & !o.0)
    }
}

/// Colours, attributes and the interaction-region id (0 = none) of a cell.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, Debug, Serialize, Deserialize)]
pub struct Style {
    pub fg: Rgb,
    pub bg: Rgb,
    pub attrs: Attrs,
    pub link: u32,
}

impl Style {
    pub fn new(fg: Rgb, bg: Rgb) -> Self {
        Self {
            fg,
            bg,
            attrs: Attrs::default(),
            link: 0,
        }
    }
}

/// One terminal cell. `g` is a grapheme cluster; the empty string marks the continuation half
/// of a double-width cluster in the cell to its left.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cell {
    pub g: SmolStr,
    pub style: Style,
}

impl Cell {
    pub fn blank(style: Style) -> Self {
        Self {
            g: SmolStr::new_static(" "),
            style,
        }
    }
    pub fn cont(style: Style) -> Self {
        Self {
            g: SmolStr::default(),
            style,
        }
    }
    pub fn is_cont(&self) -> bool {
        self.g.is_empty()
    }
    pub fn is_wide_head(&self, next: Option<&Cell>) -> bool {
        !self.g.is_empty() && next.is_some_and(Cell::is_cont)
    }
}

/// Invariant (checked by property tests): a continuation cell is always immediately preceded by
/// a non-continuation cell whose cluster has width 2, and every width-2 head is followed by a
/// continuation cell in the same row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Grid {
    cols: u16,
    rows: u16,
    cells: Vec<Cell>,
}

impl Grid {
    pub fn new(cols: u16, rows: u16, fill: Style) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::blank(fill); cols as usize * rows as usize],
        }
    }
    pub fn cols(&self) -> u16 {
        self.cols
    }
    pub fn rows(&self) -> u16 {
        self.rows
    }
    pub fn cell(&self, x: u16, y: u16) -> &Cell {
        &self.cells[y as usize * self.cols as usize + x as usize]
    }
    pub fn row(&self, y: u16) -> &[Cell] {
        let w = self.cols as usize;
        &self.cells[y as usize * w..(y as usize + 1) * w]
    }
    pub fn row_mut(&mut self, y: u16) -> &mut [Cell] {
        let w = self.cols as usize;
        &mut self.cells[y as usize * w..(y as usize + 1) * w]
    }
    pub fn in_bounds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.cols as i32 && y < self.rows as i32
    }

    /// Replace the style of a cell without touching its text (keeps wide pairs consistent).
    pub fn restyle(&mut self, x: u16, y: u16, f: impl Fn(&mut Style)) {
        let w = self.cols as usize;
        let i = y as usize * w + x as usize;
        f(&mut self.cells[i].style);
    }

    /// Blank `x,y` (and the other half of a wide pair it belongs to), keeping `style`.
    pub fn clear(&mut self, x: u16, y: u16, style: Style) {
        self.break_pair(x, y, style);
        let w = self.cols as usize;
        self.cells[y as usize * w + x as usize] = Cell::blank(style);
    }

    /// If `x,y` is half of a wide pair, blank the *other* half so no orphan remains.
    fn break_pair(&mut self, x: u16, y: u16, style: Style) {
        let w = self.cols as usize;
        let i = y as usize * w + x as usize;
        if self.cells[i].is_cont() {
            if x > 0 {
                let mut s = self.cells[i - 1].style;
                s.bg = style.bg;
                self.cells[i - 1] = Cell::blank(s);
            }
        } else if x + 1 < self.cols && self.cells[i + 1].is_cont() {
            let s = self.cells[i + 1].style;
            self.cells[i + 1] = Cell::blank(s);
        }
    }

    /// Place one grapheme cluster with its top-left at `x,y`. Returns the cells consumed
    /// (0 if it cannot be drawn or is off-grid). A double-width cluster that would straddle the
    /// right edge is replaced by a space.
    pub fn put(&mut self, x: u16, y: u16, g: &str, style: Style) -> u16 {
        if x >= self.cols || y >= self.rows {
            return 0;
        }
        let mut w = cluster_width(g);
        if w == 0 {
            return 0;
        }
        let g = if w == 2 && x + 1 >= self.cols {
            w = 1;
            " "
        } else {
            g
        };
        let cols = self.cols as usize;
        let i = y as usize * cols + x as usize;
        self.break_pair(x, y, style);
        if w == 2 {
            self.break_pair(x + 1, y, style);
            self.cells[i] = Cell { g: g.into(), style };
            self.cells[i + 1] = Cell::cont(style);
        } else {
            self.cells[i] = Cell { g: g.into(), style };
        }
        w as u16
    }

    /// Draw `s` starting at `x`, stopping before `max_x` (exclusive). Returns the next free x.
    pub fn put_str(&mut self, mut x: u16, y: u16, s: &str, style: Style, max_x: u16) -> u16 {
        let max_x = max_x.min(self.cols);
        for g in clusters(s) {
            let g = if g.chars().next().is_some_and(char::is_whitespace) {
                " "
            } else {
                g
            };
            let w = cluster_width(g) as u16;
            if w == 0 {
                continue;
            }
            if x + w > max_x {
                break;
            }
            x += self.put(x, y, g, style);
        }
        x
    }

    pub fn set_row_cells(&mut self, y: u16, from: usize, cells: impl IntoIterator<Item = Cell>) {
        let row = self.row_mut(y);
        for (slot, c) in row[from..].iter_mut().zip(cells) {
            *slot = c;
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16, fill: Style) {
        let mut g = Grid::new(cols, rows, fill);
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                g.cells[y as usize * cols as usize + x as usize] = self.cell(x, y).clone();
            }
            // A wide head cut off at the new right edge would be orphaned.
            if cols < self.cols && cols > 0 {
                let last = g.cell(cols - 1, y).clone();
                if !last.g.is_empty() && self.cell(cols, y).is_cont() {
                    g.cells[y as usize * cols as usize + cols as usize - 1] =
                        Cell::blank(last.style);
                }
            }
        }
        *self = g;
    }

    /// Check the wide-pair invariant. Used by tests and debug assertions.
    pub fn check_invariants(&self) -> Result<(), String> {
        for y in 0..self.rows {
            let row = self.row(y);
            for (x, c) in row.iter().enumerate() {
                if c.is_cont() {
                    let ok = x > 0 && !row[x - 1].is_cont() && cluster_width(&row[x - 1].g) == 2;
                    if !ok {
                        return Err(format!("orphan continuation at {x},{y}"));
                    }
                } else if cluster_width(&c.g) == 2 && !row.get(x + 1).is_some_and(Cell::is_cont) {
                    return Err(format!("wide head without continuation at {x},{y}"));
                }
            }
        }
        Ok(())
    }

    /// Plain text of the grid, trailing blanks trimmed. For tests and `glyph render`.
    pub fn dump_text(&self) -> String {
        let mut out = String::new();
        for y in 0..self.rows {
            let mut line = String::new();
            for c in self.row(y) {
                line.push_str(&c.g);
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }
}

impl Grid {
    /// Truecolor ANSI rendering (one string, `\n` separated). For `glyph render` and debugging.
    pub fn to_ansi(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for y in 0..self.rows {
            let mut cur: Option<Style> = None;
            for c in self.row(y) {
                if c.is_cont() {
                    continue;
                }
                if cur != Some(c.style) {
                    let s = c.style;
                    let _ = write!(
                        out,
                        "\x1b[0;38;2;{};{};{};48;2;{};{};{}m",
                        s.fg.0, s.fg.1, s.fg.2, s.bg.0, s.bg.1, s.bg.2
                    );
                    if s.attrs.contains(Attrs::BOLD) {
                        out.push_str("\x1b[1m");
                    }
                    if s.attrs.contains(Attrs::ITALIC) {
                        out.push_str("\x1b[3m");
                    }
                    if s.attrs.contains(Attrs::UNDERLINE) {
                        out.push_str("\x1b[4m");
                    }
                    if s.attrs.contains(Attrs::STRIKE) {
                        out.push_str("\x1b[9m");
                    }
                    cur = Some(s);
                }
                out.push_str(&c.g);
            }
            out.push_str("\x1b[0m\n");
        }
        out
    }
}
