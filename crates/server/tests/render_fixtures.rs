//! Renders recorded Chromium snapshots (fixtures/recorded) offline and snapshot-tests the grid.
//! Re-record with `cargo run -p glyph-server --example record`.

use std::{fmt::Write, path::PathBuf};

use glyph_proto::{full_runs, Attrs, Grid, RegionKind};
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

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recorded")
}

fn load(name: &str, pixels: bool) -> Rendered {
    let snap: SnapshotResult = serde_json::from_slice(
        &std::fs::read(dir().join(format!("{name}.snapshot.json"))).unwrap(),
    )
    .unwrap();
    let pix = pixels.then(|| {
        Pixmap::decode_jpeg(&std::fs::read(dir().join(format!("{name}.jpg"))).unwrap()).unwrap()
    });
    render(&snap, pix.as_ref(), &M)
}

fn hex(c: glyph_proto::Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Compact style dump: text-bearing rows list their spans; blank rows are summarised.
fn styles(g: &Grid) -> String {
    let mut out = String::new();
    for (y, run) in full_runs(g).iter().enumerate() {
        if run.spans.iter().all(|s| s.text.trim().is_empty()) {
            let _ = writeln!(out, "r{y:02} blank x{}", run.spans.len());
            continue;
        }
        let _ = write!(out, "r{y:02}");
        for s in &run.spans {
            let mut flags = String::new();
            for (a, c) in [
                (Attrs::BOLD, 'B'),
                (Attrs::ITALIC, 'I'),
                (Attrs::UNDERLINE, 'U'),
                (Attrs::STRIKE, 'S'),
            ] {
                if s.style.attrs.contains(a) {
                    flags.push(c);
                }
            }
            let link = if s.style.link != 0 {
                format!(" L{}", s.style.link)
            } else {
                String::new()
            };
            let _ = write!(
                out,
                " [{} {} {flags}{link}]{:?}",
                hex(s.style.fg),
                hex(s.style.bg),
                s.text
            );
        }
        out.push('\n');
    }
    out
}

macro_rules! fixture_snapshots {
    ($($name:ident),*) => {$(
        #[test]
        fn $name() {
            let r = load(stringify!($name), true);
            r.grid.check_invariants().unwrap();
            insta::assert_snapshot!(concat!(stringify!($name), "_text"), r.grid.dump_text());
            insta::assert_snapshot!(concat!(stringify!($name), "_styles"), styles(&r.grid));
            let regions: String = r.regions.iter().map(|g| format!("#{} {:?} {:?} {:?} {:?}\n", g.id, g.kind, g.rects, g.href, g.label)).collect();
            insta::assert_snapshot!(concat!(stringify!($name), "_regions"), regions);
        }
    )*};
}

fixture_snapshots!(basic, unicode, form, overlay, boxes);

// ---- explicit semantics (so a blessed-but-wrong snapshot can't hide a regression)

#[test]
fn links_become_regions_with_absolute_urls() {
    let r = load("basic", false);
    let hrefs: Vec<_> = r.regions.iter().filter_map(|g| g.href.as_deref()).collect();
    assert_eq!(hrefs.len(), 2, "{hrefs:?}");
    assert!(
        hrefs[0].ends_with("/next") && hrefs[0].starts_with("http://"),
        "{hrefs:?}"
    );
    assert_eq!(hrefs[1], "https://example.org/");
    assert!(r.regions.iter().all(|g| g.kind == RegionKind::Link));
    // every region cell carries its id, and ids are ordered top-to-bottom
    let first = &r.regions[0].rects[0];
    assert_eq!(r.grid.cell(first.x, first.y).style.link, r.regions[0].id);
    assert!(r.regions[0].rects[0].y < r.regions[1].rects[0].y);
}

#[test]
fn text_attributes_come_from_computed_style() {
    let r = load("basic", false);
    let text = r.grid.dump_text();
    let (row, line) = text
        .lines()
        .enumerate()
        .find(|(_, l)| l.contains("A paragraph"))
        .unwrap();
    let col = |needle: &str| line.find(needle).unwrap();
    let at = |needle: &str| r.grid.cell(col(needle) as u16, row as u16).style.attrs;
    assert!(at("bold").contains(Attrs::BOLD));
    assert!(at("italic").contains(Attrs::ITALIC));
    assert!(at("underline").contains(Attrs::UNDERLINE));
    assert!(at("struck").contains(Attrs::STRIKE));
    assert!(!at("paragraph").contains(Attrs::BOLD));
}

#[test]
fn opaque_overlay_hides_text_beneath_and_clip_hides_collapsed() {
    let t = load("overlay", false).grid.dump_text();
    assert!(t.contains("Modal dialog"));
    assert!(
        !t.contains("COLLAPSED"),
        "overflow:hidden content leaked:\n{t}"
    );
    // the modal covers rows 3..10, cols 10..50: lines two and three lose their middle, line one (row 1) does not
    let rows: Vec<&str> = t.lines().collect();
    assert!(rows[1].contains("rather long across"), "{t}");
    for r in [3, 5] {
        assert!(
            !rows[r].contains("rather") && !rows[r].contains("runs"),
            "row {r} leaked under modal:\n{t}"
        );
    }
    // line four (row 8) is also under the modal's rows: only its left edge survives
    assert!(
        rows[8].contains("Backgroun") && !rows[8].contains("four"),
        "{t}"
    );
}

#[test]
fn wide_clusters_keep_grid_invariants_and_widths() {
    let r = load("unicode", true);
    r.grid.check_invariants().unwrap();
    let t = r.grid.dump_text();
    for needle in ["日本語の文章", "한국어", "😀", "🇦🇹", "👨‍👩‍👧", "e\u{301}"]
    {
        assert!(t.contains(needle), "missing {needle}:\n{t}");
    }
}

#[test]
fn form_controls_are_drawn_and_interactive() {
    let r = load("form", false);
    let t = r.grid.dump_text();
    assert!(t.contains("[Ada"), "{t}");
    assert!(t.contains("type here"), "{t}");
    assert!(
        t.contains("[x]") && t.contains("( )") && t.contains("(•)"),
        "{t}"
    );
    assert!(t.contains("first line"), "{t}");
    assert!(t.contains("Beta"), "{t}");
    let kinds: Vec<_> = r.regions.iter().map(|g| g.kind).collect();
    for k in [
        RegionKind::Input,
        RegionKind::Checkbox,
        RegionKind::Radio,
        RegionKind::TextArea,
        RegionKind::Select,
        RegionKind::Button,
    ] {
        assert!(kinds.contains(&k), "no {k:?} region in {kinds:?}");
    }
}

#[test]
fn pixels_supply_backgrounds() {
    // overlay.html's modal is #223344-ish dark; with pixels its cells must be dark, without they come from CSS
    let with = load("overlay", true).grid;
    let c = with.cell(20, 4); // inside the modal
    assert!(
        c.style.bg.0 < 80 && c.style.bg.2 < 100,
        "modal bg not dark: {:?}",
        c.style.bg
    );
    let without = load("overlay", false).grid;
    assert!(without.cell(20, 4).style.bg.0 < 80);
}
