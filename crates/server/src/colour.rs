//! The colour stage: turn pixels into cell colours.
//!
//! Two kinds of cell, handled differently because they fail differently:
//!
//! * **Text runs** own a rectangle of cells. Their background is estimated *once per run* from every
//!   pixel in the rectangle except ink (pixels near the known text colour), by majority bucket.
//!   Sparse per-cell sampling lets a large or bold glyph out-vote its own background; a run-wide vote
//!   cannot be swung by a few cells. A run whose background genuinely changes along its length
//!   (gradient, highlight) is split into chunks.
//! * **Everything else** is read as four sub-cell colours (a 2×2 "quadrant" of 4×8 px each). If the
//!   four are alike the cell is a flat colour; otherwise the best two-colour split is drawn with a
//!   quadrant block character, which is what gives edges, icons and video twice the horizontal
//!   resolution of half-blocks. (The quadrant idea is the one Carbonyl uses.)

use glyph_proto::{Attrs, Grid, Rgb, Style};

use crate::{
    pixmap::{CellGeom, Pixmap},
    render::Metrics,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Plain,
    Glyph,
    Image,
}

/// A text run's cell rectangle and what we know about it from the DOM.
pub(crate) struct BoxRec {
    pub c0: i32,
    pub r0: i32,
    pub c1: i32,
    pub r1: i32,
    pub fg: Rgb,
    /// Effective CSS background (ancestors composited); the fallback when pixels cannot tell.
    pub css_bg: Rgb,
}

/// Quadrants closer than this (weighted squared RGB distance) are one colour. ≈ 11 levels per
/// channel: above JPEG noise at screencast quality, below a white card on a #f4f5f7 page (834).
const FLAT: u32 = 1200;
/// Next to a drawn border only strong structure (a button face) may show; the thin pixel line the
/// border glyphs already represent must not be drawn again.
const FLAT_HALO: u32 = 40000;
/// Photos and video: show finer texture.
const FLAT_IMAGE: u32 = 350;
/// Per-cell mean colour change (squared distance) that counts as "the page changed".
const CHANGED: u32 = 100;
/// Flat neighbours this close share a colour (keeps spans long, hides JPEG noise).
const SNAP: u32 = 150;
/// A pixel this close to the text colour is ink, not background.
const INK: u32 = 3000;
/// Chunk width (cells) for run backgrounds that change along the run.
const CHUNK: i32 = 8;

fn mean4(a: [Rgb; 4], mask: u8) -> Rgb {
    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
    for (i, c) in a.iter().enumerate() {
        if mask & (1 << i) != 0 {
            r += c.0 as u32;
            g += c.1 as u32;
            b += c.2 as u32;
            n += 1;
        }
    }
    let n = n.max(1);
    Rgb((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

fn sse(a: [Rgb; 4], mask: u8, mean: Rgb) -> u32 {
    (0..4)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| a[i].dist2(mean))
        .sum()
}

/// Quadrant block character whose filled region is `mask` (bit0 UL, bit1 UR, bit2 LL, bit3 LR).
fn quad_char(mask: u8) -> &'static str {
    const T: [&str; 16] = [
        " ", "▘", "▝", "▀", "▖", "▌", "▞", "▛", "▗", "▚", "▐", "▜", "▄", "▙", "▟", "█",
    ];
    T[(mask & 15) as usize]
}

/// How a cell with sub-colours `q` should look: `(glyph, fg, bg)`.
pub(crate) fn cell_look(q: [Rgb; 4], flat_below: u32) -> (&'static str, Rgb, Rgb) {
    let spread = (0..4)
        .flat_map(|i| (i + 1..4).map(move |j| (i, j)))
        .map(|(i, j)| q[i].dist2(q[j]))
        .max()
        .unwrap_or(0);
    let all = mean4(q, 15);
    if spread < flat_below {
        return (" ", all, all);
    }
    // the seven ways to split four quadrants in two; the part *without* the upper-left quadrant is
    // the foreground, so the same picture always gets the same glyph
    let mut best = (u32::MAX, 0u8);
    for fg_mask in [0b0010u8, 0b0100, 0b1000, 0b0110, 0b1010, 0b1100, 0b1110] {
        let bg_mask = !fg_mask & 15;
        let cost = sse(q, fg_mask, mean4(q, fg_mask)) + sse(q, bg_mask, mean4(q, bg_mask));
        if cost < best.0 {
            best = (cost, fg_mask);
        }
    }
    let (fg, bg) = (mean4(q, best.1), mean4(q, !best.1 & 15));
    if fg.dist2(bg) < flat_below / 2 {
        return (" ", all, all);
    }
    (quad_char(best.1), fg, bg)
}

/// Fast path for live pixels (video, canvas): recolour only the cells of `rect` from a fresh
/// screenshot, leaving text drawn over them alone. Same look as the image cells of a full render.
pub fn recolour_live(grid: &mut Grid, rect: glyph_proto::CellRect, p: &Pixmap, m: &Metrics) {
    let geom = CellGeom {
        cw: m.cw,
        ch: m.ch,
        sx: p.w as f64 / m.px_w(),
        sy: p.h as f64 / m.px_h(),
    };
    for y in rect.y..(rect.y + rect.h).min(m.rows) {
        for x in rect.x..(rect.x + rect.w).min(m.cols) {
            let cur = grid.cell(x, y);
            let drawn_text = cur.g != " " && !"▘▝▖▗▀▄▌▐▚▞▛▜▙▟█░▒▓".contains(cur.g.as_str());
            if drawn_text || cur.is_cont() {
                continue;
            }
            let link = cur.style.link;
            let q = p.quadrants(geom.px_rect(x as i32, y as i32, x as i32 + 1, y as i32 + 1));
            let (g, fg, bg) = cell_look(q, FLAT_IMAGE);
            let st = Style {
                fg,
                bg,
                attrs: Attrs::default(),
                link,
            };
            grid.clear(x, y, st);
            if g != " " {
                grid.put(x, y, g, st);
            }
        }
    }
}

/// Did anything outside the live rectangles (and a one-cell margin around them) change between two
/// screenshots? Cheap per-cell mean comparison: the cue that a text label or layout moved, so the
/// live fast path has to hand over to a full refresh.
pub fn changed_outside(
    a: &Pixmap,
    b: &Pixmap,
    live: &[glyph_proto::CellRect],
    m: &Metrics,
) -> bool {
    if (a.w, a.h) != (b.w, b.h) {
        return true;
    }
    let geom = CellGeom {
        cw: m.cw,
        ch: m.ch,
        sx: a.w as f64 / m.px_w(),
        sy: a.h as f64 / m.px_h(),
    };
    let near = |x: u16, y: u16| {
        live.iter()
            .any(|r| x + 1 >= r.x && y + 1 >= r.y && x <= r.x + r.w && y <= r.y + r.h)
    };
    for y in 0..m.rows {
        for x in 0..m.cols {
            if near(x, y) {
                continue;
            }
            let rect = geom.px_rect(x as i32, y as i32, x as i32 + 1, y as i32 + 1);
            if a.mean_rect(rect).dist2(b.mean_rect(rect)) > CHANGED {
                return true;
            }
        }
    }
    false
}

/// Fill in every cell's colours (and, for non-text cells, the block character) from `p`.
pub(crate) fn apply(
    grid: &mut Grid,
    kind: &[Kind],
    owner: &[i32],
    halo: &[bool],
    boxes: &[BoxRec],
    p: &Pixmap,
    m: &Metrics,
) {
    let geom = CellGeom {
        cw: m.cw,
        ch: m.ch,
        sx: p.w as f64 / m.px_w(),
        sy: p.h as f64 / m.px_h(),
    };
    let (cols, rows) = (m.cols as usize, m.rows as usize);

    // 1. one background per text run (per chunk when it varies)
    let mut box_bg: Vec<Option<Rgb>> = vec![None; cols * rows];
    for (bi, b) in boxes.iter().enumerate() {
        let (c0, c1) = (b.c0.max(0), b.c1.min(cols as i32));
        let (r0, r1) = (b.r0.max(0), b.r1.min(rows as i32));
        if c1 <= c0 || r1 <= r0 {
            continue;
        }
        let whole = p
            .mode_excluding(
                geom.px_rect(c0, r0, c1, r1),
                Some(b.fg),
                INK,
                Some(b.css_bg),
            )
            .unwrap_or(b.css_bg);
        let mut ca = c0;
        while ca < c1 {
            let cb = (ca + CHUNK).min(c1);
            let est = p
                .mode_excluding(
                    geom.px_rect(ca, r0, cb, r1),
                    Some(b.fg),
                    INK,
                    Some(b.css_bg),
                )
                .unwrap_or(whole);
            let use_ = if est.dist2(whole) < 1500 { whole } else { est };
            for y in r0..r1 {
                for x in ca..cb {
                    if owner[y as usize * cols + x as usize] == bi as i32 {
                        box_bg[y as usize * cols + x as usize] = Some(use_);
                    }
                }
            }
            ca = cb;
        }
    }

    // 2. every cell
    let mut up: Vec<Option<Rgb>> = vec![None; cols];
    for y in 0..rows {
        let mut left: Option<Rgb> = None;
        for x in 0..cols {
            let i = y * cols + x;
            let cur = grid.cell(x as u16, y as u16).clone();
            if cur.is_cont() {
                // continuation of a wide glyph: same background as its head
                let head = grid.cell(x as u16 - 1, y as u16).style.bg;
                grid.restyle(x as u16, y as u16, |s| s.bg = head);
                continue;
            }
            let (xu, yu) = (x as u16, y as u16);
            if let Some(bg) = box_bg[i] {
                grid.restyle(xu, yu, |s| s.bg = bg);
                (left, up[x]) = (Some(bg), Some(bg));
                continue;
            }
            let rect = geom.px_rect(x as i32, y as i32, x as i32 + 1, y as i32 + 1);
            let link = cur.style.link;
            let snap = |c: Rgb, left: Option<Rgb>, up: Option<Rgb>| -> Rgb {
                match (left, up) {
                    (Some(l), _) if c.dist2(l) < SNAP => l,
                    (_, Some(u)) if c.dist2(u) < SNAP => u,
                    _ => c,
                }
            };
            match kind[i] {
                Kind::Glyph => {
                    // borders and other glyphs without a run: this cell's own background
                    let bg = p
                        .mode_excluding(rect, Some(cur.style.fg), INK, None)
                        .unwrap_or_else(|| p.mean_rect(rect));
                    let bg = snap(bg, left, up[x]);
                    grid.restyle(xu, yu, |s| s.bg = bg);
                    (left, up[x]) = (Some(bg), Some(bg));
                }
                Kind::Plain | Kind::Image => {
                    let q = p.quadrants(rect);
                    let t = match (kind[i], halo[i]) {
                        (Kind::Image, _) => FLAT_IMAGE,
                        (_, true) => FLAT_HALO,
                        _ => FLAT,
                    };
                    let (g, fg, mut bg) = cell_look(q, t);
                    if g == " " && halo[i] && kind[i] != Kind::Image {
                        // next to a drawn border a thin pixel line must vanish, not tint the cell:
                        // the dominant colour ignores it, the mean would dilute it into a pale band
                        bg = p.mode_excluding(rect, None, 0, None).unwrap_or(bg);
                    }
                    if g == " " {
                        let bg = snap(bg, left, up[x]);
                        grid.clear(
                            xu,
                            yu,
                            Style {
                                fg: cur.style.fg,
                                bg,
                                attrs: Attrs::default(),
                                link,
                            },
                        );
                        (left, up[x]) = (Some(bg), Some(bg));
                    } else {
                        grid.clear(
                            xu,
                            yu,
                            Style {
                                fg,
                                bg,
                                attrs: Attrs::default(),
                                link,
                            },
                        );
                        grid.put(
                            xu,
                            yu,
                            g,
                            Style {
                                fg,
                                bg,
                                attrs: Attrs::default(),
                                link,
                            },
                        );
                        (left, up[x]) = (None, None);
                    }
                }
            }
        }
    }

    // 3. keep text legible where CSS and pixels disagree (text over images)
    for y in 0..m.rows {
        for x in 0..m.cols {
            let c = grid.cell(x, y);
            let is_text = c.g != " " && !c.is_cont() && !"▘▝▖▗▀▄▌▐▚▞▛▜▙▟█".contains(c.g.as_str());
            if is_text && c.style.fg.dist2(c.style.bg) < 1500 {
                let light = (c.style.bg.0 as u32 * 299
                    + c.style.bg.1 as u32 * 587
                    + c.style.bg.2 as u32 * 114)
                    / 1000
                    > 128;
                let fg = if light {
                    Rgb(0x11, 0x11, 0x11)
                } else {
                    Rgb(0xee, 0xee, 0xee)
                };
                grid.restyle(x, y, |s| s.fg = fg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: Rgb = Rgb(255, 255, 255);
    const BLUE: Rgb = Rgb(11, 95, 255);

    #[test]
    fn flat_cells_stay_flat_even_with_jpeg_noise() {
        let noisy = [
            Rgb(250, 250, 250),
            Rgb(253, 252, 254),
            Rgb(251, 253, 250),
            Rgb(255, 255, 255),
        ];
        assert_eq!(cell_look(noisy, FLAT).0, " ");
        // a white card on a near-white page is a *different* flat colour, not a merged one
        let card = [W; 4];
        let page = [Rgb(0xf4, 0xf5, 0xf7); 4];
        assert_ne!(cell_look(card, FLAT).2, cell_look(page, FLAT).2);
    }

    #[test]
    fn edges_pick_the_matching_block_glyph_with_stable_fg_bg() {
        // upper half white, lower half blue: bg is the part containing the upper-left quadrant
        let (g, fg, bg) = cell_look([W, W, BLUE, BLUE], FLAT);
        assert_eq!((g, fg, bg), ("▄", BLUE, W));
        // left half blue, right half white
        let (g, fg, bg) = cell_look([BLUE, W, BLUE, W], FLAT);
        assert_eq!((g, fg, bg), ("▐", W, BLUE));
        // a single odd quadrant
        let (g, fg, bg) = cell_look([W, W, W, BLUE], FLAT);
        assert_eq!((g, fg, bg), ("▗", BLUE, W));
        // diagonal
        let (g, _, _) = cell_look([W, BLUE, BLUE, W], FLAT);
        assert_eq!(g, "▞");
    }

    #[test]
    fn same_picture_same_output_regardless_of_which_colour_is_which() {
        // swapping the two colours must yield the same *shape* with swapped colours
        let a = cell_look([W, W, BLUE, BLUE], FLAT);
        let b = cell_look([BLUE, BLUE, W, W], FLAT);
        assert_eq!(a.0, "▄");
        assert_eq!(b.0, "▄");
        assert_eq!((a.1, a.2), (b.2, b.1));
    }

    #[test]
    fn photo_texture_uses_the_finer_threshold() {
        let subtle = [
            Rgb(100, 100, 100),
            Rgb(108, 108, 108),
            Rgb(100, 100, 100),
            Rgb(108, 108, 108),
        ];
        assert_eq!(cell_look(subtle, FLAT).0, " ");
        assert_ne!(cell_look(subtle, FLAT_IMAGE).0, " ");
    }

    fn solid(w: usize, h: usize, c: Rgb) -> Pixmap {
        Pixmap::from_rgb(w, h, (0..w * h).flat_map(|_| [c.0, c.1, c.2]).collect())
    }

    #[test]
    fn live_recolour_updates_the_rect_and_leaves_text_and_outside_alone() {
        let m = Metrics {
            cols: 4,
            rows: 2,
            cw: 8.0,
            ch: 16.0,
        };
        let mut g = Grid::new(4, 2, Style::new(Rgb(0, 0, 0), Rgb(255, 255, 255)));
        g.put(1, 0, "x", Style::new(Rgb(0, 0, 0), Rgb(255, 255, 255)));
        let red = solid(32, 32, Rgb(200, 0, 0));
        let rect = glyph_proto::CellRect {
            x: 0,
            y: 0,
            w: 2,
            h: 1,
        };
        recolour_live(&mut g, rect, &red, &m);
        assert_eq!(
            g.cell(0, 0).style.bg,
            Rgb(200, 0, 0),
            "live cell takes the new colour"
        );
        assert_eq!(g.cell(1, 0).g, "x", "text over the video stays");
        assert_eq!(
            g.cell(2, 0).style.bg,
            Rgb(255, 255, 255),
            "outside the rect is untouched"
        );
    }

    #[test]
    fn change_detection_ignores_the_live_rect_but_sees_a_label() {
        let m = Metrics {
            cols: 4,
            rows: 2,
            cw: 8.0,
            ch: 16.0,
        };
        let a = solid(32, 32, Rgb(255, 255, 255));
        let mut px = vec![255u8; 32 * 32 * 3];
        let paint = |px: &mut Vec<u8>, x0: usize, x1: usize| {
            for y in 0..16 {
                for x in x0..x1 {
                    px[(y * 32 + x) * 3..][..3].copy_from_slice(&[0, 0, 0]);
                }
            }
        };
        paint(&mut px, 0, 8); // inside the live cell
        let in_live = Pixmap::from_rgb(32, 32, px.clone());
        let live = [glyph_proto::CellRect {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        }];
        assert!(!changed_outside(&a, &in_live, &live, &m));
        paint(&mut px, 24, 32); // far cell: a label changed
        assert!(changed_outside(
            &a,
            &Pixmap::from_rgb(32, 32, px),
            &live,
            &m
        ));
    }
}
