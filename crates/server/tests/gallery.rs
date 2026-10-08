//! Offline rendering-quality tests on the gallery pages (recorded with the real page agent by
//! `cargo run -p glyph-server --example record_gallery`). Beyond snapshots these assert the
//! specific defects that were found by looking at real renderings.

use std::path::PathBuf;

use glyph_proto::{Grid, Rgb};
use glyph_server::{
    pixmap::Pixmap,
    render::{render, Metrics, Rendered},
    snapshot::SnapshotResult,
};

const M: Metrics = Metrics {
    cols: 100,
    rows: 30,
    cw: 8.0,
    ch: 16.0,
};
const BLOCKS: &str = "▘▝▖▗▀▄▌▐▚▞▛▜▙▟█";

fn load(name: &str) -> Rendered {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recorded/gallery");
    let snap: SnapshotResult =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.snapshot.json"))).unwrap())
            .unwrap();
    let pix =
        Pixmap::decode_jpeg(&std::fs::read(dir.join(format!("{name}.jpg"))).unwrap()).unwrap();
    render(&snap, Some(&pix), &M)
}

fn block_cells(g: &Grid) -> Vec<(u16, u16)> {
    let mut v = vec![];
    for y in 0..g.rows() {
        for x in 0..g.cols() {
            if !g.cell(x, y).is_cont() && BLOCKS.contains(g.cell(x, y).g.as_str()) {
                v.push((x, y));
            }
        }
    }
    v
}

/// Position of `needle` in the grid text as (col, row) of its first cell.
fn find(g: &Grid, needle: &str) -> (u16, u16) {
    for y in 0..g.rows() {
        let row: String = (0..g.cols()).map(|x| g.cell(x, y).g.to_string()).collect();
        if let Some(i) = row.find(needle) {
            return (row[..i].chars().count() as u16, y);
        }
    }
    panic!("{needle:?} not found in\n{}", g.dump_text());
}

fn bgs(g: &Grid, needle: &str) -> Vec<Rgb> {
    let (x, y) = find(g, needle);
    (x..x + needle.chars().count() as u16)
        .map(|c| g.cell(c, y).style.bg)
        .collect()
}

/// Cells with text whose background differs sharply from *both* neighbours that agree with each
/// other: an isolated colour blob inside a text run.
fn blobs(g: &Grid) -> Vec<(u16, u16)> {
    let mut v = vec![];
    for y in 0..g.rows() {
        for x in 1..g.cols() - 1 {
            let c = g.cell(x, y);
            let is_text = |c: &glyph_proto::Cell| {
                c.g != " " && !c.is_cont() && !BLOCKS.contains(c.g.as_str())
            };
            if !is_text(c) {
                continue;
            }
            // inside a run of letters: both neighbours carry text too
            let (lc, rc) = (g.cell(x - 1, y), g.cell(x + 1, y));
            if !is_text(lc) || !is_text(rc) {
                continue;
            }
            let (l, r) = (lc.style.bg, rc.style.bg);
            if l.dist2(r) < 600 && c.style.bg.dist2(l) > 3000 {
                v.push((x, y));
            }
        }
    }
    v
}

macro_rules! text_pages {
    ($($name:ident),*) => {$(
        #[test]
        fn $name() {
            let r = load(stringify!($name));
            r.grid.check_invariants().unwrap();
            insta::assert_snapshot!(concat!("gallery_", stringify!($name)), r.grid.dump_text());
        }
    )*};
}
text_pages!(
    article, cards, dark, forms, gradients, motion, photo, sticky, typography, unicode, video
);

#[test]
fn text_only_pages_have_no_pixel_blocks_and_no_blobs() {
    for name in ["typography", "dark", "unicode"] {
        let g = load(name).grid;
        // the only pixel detail allowed is the rounded right end of dark.html's green pill
        // unicode.html prints real block characters, so only its blobs are checked
        let blocks: Vec<_> = block_cells(&g)
            .into_iter()
            .filter(|&c| name != "unicode" && !(name == "dark" && c == (12, 7)))
            .collect();
        assert!(
            blocks.is_empty(),
            "{name}: stray block characters at {blocks:?}\n{}",
            g.dump_text()
        );
        let b = blobs(&g);
        assert!(
            b.is_empty(),
            "{name}: isolated background blobs inside text at {b:?}"
        );
    }
}

#[test]
fn no_background_blobs_inside_text_on_busy_pages() {
    for name in ["cards", "gradients", "sticky", "forms", "motion"] {
        let g = load(name).grid;
        let b = blobs(&g);
        assert!(
            b.is_empty(),
            "{name}: isolated background blobs inside text at {b:?}"
        );
    }
}

#[test]
fn a_button_label_sits_on_one_background() {
    let g = load("cards").grid;
    for label in ["Open", "Details", "Restart"] {
        let b = bgs(&g, label);
        let first = b[0];
        assert!(
            b.iter().all(|c| c.dist2(first) < 600),
            "{label}: background varies across the label: {b:?}"
        );
        if label == "Open" {
            assert!(
                first.2 > first.0 + 100,
                "{label}: expected the blue button face, got {first:?}"
            );
        }
    }
    // white button on the blue header
    let b = bgs(&g, "Sign in");
    assert!(
        b.iter().all(|c| c.dist2(b[0]) < 600 && c.0 > 220),
        "Sign in: {b:?}"
    );
}

#[test]
fn text_over_a_gradient_never_gets_a_white_hole() {
    let g = load("gradients").grid;
    for (label, hue) in [("Linear sunset", "red"), ("Linear ocean", "blue")] {
        for c in bgs(&g, label) {
            let ok = if hue == "red" {
                c.0 > c.2 + 40
            } else {
                c.2 > c.0 + 80
            };
            assert!(
                ok,
                "{label}: a cell behind the text has bg {c:?} (white hole / wrong colour)"
            );
        }
    }
}

#[test]
fn light_text_on_a_dark_page_keeps_the_dark_background() {
    let g = load("dark").grid;
    for label in ["Dark page", "Fix the renderer", "Muted text"] {
        for c in bgs(&g, label) {
            assert!(
                c.0 < 70 && c.1 < 70 && c.2 < 80,
                "{label}: bg {c:?} is not dark"
            );
        }
    }
}

#[test]
fn photos_are_drawn_with_finer_than_half_block_detail_and_without_text_noise() {
    let r = load("photo");
    let blocks = block_cells(&r.grid);
    // a 640x360 photo is 80x22 cells: most of them carry texture
    assert!(
        blocks.len() > 400,
        "photo drawn with only {} block cells",
        blocks.len()
    );
    // quadrant characters (not just ▀/▄) appear: that is the extra resolution
    let quads = blocks
        .iter()
        .filter(|&&(x, y)| "▘▝▖▗▌▐▚▞▛▜▙▟".contains(r.grid.cell(x, y).g.as_str()))
        .count();
    assert!(quads > 20, "no quadrant characters: {quads}");
    assert!(!r.images.is_empty());
}

#[test]
fn links_and_regions_survive_the_new_colour_stage() {
    let r = load("article");
    assert!(r.regions.len() >= 6, "{} regions", r.regions.len());
    for reg in &r.regions {
        let c = &reg.rects[0];
        assert_eq!(r.grid.cell(c.x, c.y).style.link, reg.id);
    }
}

#[test]
fn terminal_style_puts_wrapped_lines_on_consecutive_rows() {
    let g = load("typography").grid;
    // the "Small print" paragraph wraps onto a second line directly below the first
    let (_, y1) = find(&g, "Small print at 12px");
    let (_, y2) = find(&g, "in a naive renderer");
    assert_eq!(
        y2,
        y1 + 1,
        "a wrapped paragraph must not skip rows:\n{}",
        g.dump_text()
    );
    // and inline spans keep their blanks
    let t = g.dump_text();
    assert!(t.contains("red green blue muted grey"), "{t}");
}
