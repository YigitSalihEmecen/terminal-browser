//! Row-run diffs between grids.
//!
//! A [`Run`] is a horizontal stretch of one row encoded like terminal output: styled spans of
//! text. Continuation cells are implicit (a double-width cluster implies its right neighbour),
//! so a run never starts or ends in the middle of a wide pair. Unchanged cells are never sent;
//! runs separated by at most [`MERGE_GAP`] unchanged cells are merged because that is cheaper
//! than another run header.

use serde::{Deserialize, Serialize};

use crate::{
    cell::{Cell, Grid, Style},
    width::{cluster_width, clusters, fuses},
};

pub const MERGE_GAP: usize = 2;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Span {
    pub style: Style,
    pub text: String,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Run {
    pub y: u16,
    pub x: u16,
    pub spans: Vec<Span>,
}

fn encode_range(row: &[Cell], y: u16, from: usize, to: usize) -> Run {
    let mut spans: Vec<Span> = Vec::new();
    for c in &row[from..to] {
        if c.is_cont() {
            continue;
        }
        match spans.last_mut() {
            Some(s) if s.style == c.style && !fuses(&s.text, &c.g) => s.text.push_str(&c.g),
            _ => spans.push(Span {
                style: c.style,
                text: c.g.to_string(),
            }),
        }
    }
    Run {
        y,
        x: from as u16,
        spans,
    }
}

/// Every row as one run (a full frame).
pub fn full_runs(g: &Grid) -> Vec<Run> {
    (0..g.rows())
        .map(|y| encode_range(g.row(y), y, 0, g.cols() as usize))
        .collect()
}

/// Runs that turn `old` into `new`. Both grids must have the same dimensions.
pub fn diff_runs(old: &Grid, new: &Grid) -> Vec<Run> {
    assert_eq!(
        (old.cols(), old.rows()),
        (new.cols(), new.rows()),
        "diff needs equal dimensions"
    );
    let mut runs = Vec::new();
    let w = new.cols() as usize;
    for y in 0..new.rows() {
        let (a, b) = (old.row(y), new.row(y));
        let mut changed = vec![false; w];
        let mut any = false;
        for x in 0..w {
            if a[x] != b[x] {
                changed[x] = true;
                any = true;
            }
        }
        if !any {
            continue;
        }
        // A changed continuation drags its head in; a changed wide head drags its continuation.
        for x in 0..w {
            if changed[x] {
                if b[x].is_cont() && x > 0 {
                    changed[x - 1] = true;
                }
                if x + 1 < w && b[x + 1].is_cont() {
                    changed[x + 1] = true;
                }
            }
        }
        let mut x = 0;
        while x < w {
            if !changed[x] {
                x += 1;
                continue;
            }
            let mut start = x;
            let mut end = x + 1; // exclusive
            let mut scan = end;
            while scan < w && scan <= end + MERGE_GAP {
                if changed[scan] {
                    end = scan + 1;
                }
                scan += 1;
            }
            while start > 0 && b[start].is_cont() {
                start -= 1;
            }
            while end < w && b[end].is_cont() {
                end += 1;
            }
            runs.push(encode_range(b, y, start, end));
            x = end;
        }
    }
    runs
}

/// Apply runs onto `grid`. Out-of-range content is clipped, never panics.
pub fn apply_runs(grid: &mut Grid, runs: &[Run]) {
    for r in runs {
        if r.y >= grid.rows() {
            continue;
        }
        let mut x = r.x;
        for s in &r.spans {
            for g in clusters(&s.text) {
                if cluster_width(g) == 0 {
                    continue;
                }
                x = x.saturating_add(grid.put(x, r.y, g, s.style));
            }
        }
    }
}
