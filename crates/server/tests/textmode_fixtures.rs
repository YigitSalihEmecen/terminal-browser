//! Reader (text) mode rendered offline from recorded accessibility trees.

use std::path::PathBuf;

use glyph_proto::{Grid, RegionKind};
use glyph_server::{
    snapshot::SnapshotResult,
    textmode::{self, AxTree, TextDoc},
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recorded")
}

fn doc(name: &str, cols: u16) -> TextDoc {
    let ax: AxTree =
        serde_json::from_slice(&std::fs::read(dir().join(format!("{name}.ax.json"))).unwrap())
            .unwrap();
    let snap: SnapshotResult =
        serde_json::from_slice(&std::fs::read(dir().join(format!("{name}.display.json"))).unwrap())
            .unwrap();
    textmode::build(&ax, &textmode::displays_from_snapshot(&snap), cols)
}

fn whole(d: &TextDoc) -> (Grid, Vec<glyph_proto::Region>) {
    d.slice(0, d.lines().max(1) as u16, None)
}

macro_rules! reader_snapshots {
    ($($name:ident),*) => {$(
        #[test]
        fn $name() {
            let d = doc(stringify!($name), 80);
            let (g, regions) = whole(&d);
            g.check_invariants().unwrap();
            insta::assert_snapshot!(concat!("reader_", stringify!($name)), g.dump_text());
            let r: String = regions.iter().map(|r| format!("#{} {:?} {:?} {:?}\n", r.id, r.kind, r.href, r.label)).collect();
            insta::assert_snapshot!(concat!("reader_", stringify!($name), "_regions"), r);
        }
    )*};
}

reader_snapshots!(article, basic, form, unicode);

#[test]
fn article_structure() {
    let d = doc("article", 80);
    let (g, regions) = whole(&d);
    let t = g.dump_text();
    for needle in [
        "# Terminal browsers",
        "## Features",
        "[img: a tiny logo]",
        "1. First",
        "2. Second",
        "│ Quoted text",
    ] {
        assert!(t.contains(needle), "missing {needle:?} in\n{t}");
    }
    // divs break lines, spans join
    assert!(
        t.contains("block one\n") && t.contains("block two\n"),
        "{t}"
    );
    assert!(t.contains("inline joined"), "{t}");
    // nested list is indented deeper than its parent
    let indent = |s: &str| {
        t.lines()
            .find(|l| l.contains(s))
            .map(|l| l.len() - l.trim_start().len())
            .unwrap()
    };
    assert!(indent("inner one") > indent("Nested lists"), "{t}");
    // a long paragraph wrapped, and the link inside it kept one region per wrapped line
    let spec = regions
        .iter()
        .find(|r| r.href.as_deref().is_some_and(|h| h.ends_with("/spec")))
        .expect("spec link");
    assert!(!spec.rects.is_empty());
    for r in &spec.rects {
        assert_eq!(g.cell(r.x, r.y).style.link, spec.id);
    }
    // table cells are separated and the header is ruled
    assert!(t.contains("Mode") && t.contains("│ Cost"), "{t}");
    // controls
    assert!(regions
        .iter()
        .any(|r| r.kind == RegionKind::Input && r.value.as_deref() == Some("glyph")));
    assert!(regions.iter().any(|r| r.kind == RegionKind::Checkbox));
    assert!(regions
        .iter()
        .any(|r| r.kind == RegionKind::Button && r.label == "Go"));
    assert!(t.contains("[glyph"), "{t}");
    assert!(t.contains("[x] Remember"), "{t}");
}

#[test]
fn width_changes_reflow_but_keep_text() {
    // decoration (quote bars, rules) legitimately depends on width; the words must not
    let words = |d: &TextDoc| {
        whole(d)
            .0
            .dump_text()
            .split_whitespace()
            .filter(|w| !w.chars().all(|c| matches!(c, '│' | '─')))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    let (narrow, wide) = (doc("article", 40), doc("article", 120));
    assert!(narrow.lines() > wide.lines());
    assert_eq!(
        words(&narrow),
        words(&wide),
        "reflow must not lose or reorder words"
    );
}

#[test]
fn every_line_fits_the_width() {
    for cols in [30u16, 52, 80] {
        let d = doc("article", cols);
        for line in whole(&d).0.dump_text().lines() {
            assert!(
                glyph_proto::width::str_width(line) <= cols as usize,
                "{cols}: {line:?}"
            );
        }
    }
}

#[test]
fn slices_scroll() {
    let d = doc("article", 40);
    assert!(d.lines() > 12);
    let (top, _) = d.slice(0, 5, None);
    let (later, regions) = d.slice(6, 5, None);
    assert_ne!(top.dump_text(), later.dump_text());
    for r in regions {
        for c in r.rects {
            assert!(c.y < 5);
        }
    }
}
