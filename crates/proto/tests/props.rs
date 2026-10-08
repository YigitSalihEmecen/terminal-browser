use glyph_proto::{apply_runs, diff_runs, full_runs, width::*, Attrs, Cell, Grid, Rgb, Style};
use proptest::prelude::*;

const ALPHABET: &[&str] = &[
    "a",
    "b",
    "Z",
    " ",
    "~",
    "日",
    "本",
    "한",
    "😀",
    "👍🏽",
    "🇦🇹",
    "👨‍👩‍👧",
    "e\u{301}",
    "ö",
    "│",
    "█",
    "\u{200b}",
    "\t",
    "\u{301}",
    "🇦",
    "🇹",
    "❤\u{fe0f}",
    "ｱ",
];

fn style(n: u8) -> Style {
    Style {
        fg: Rgb(n, 0, 0),
        bg: Rgb(0, n % 3, 0),
        attrs: Attrs(n % 4),
        link: (n % 2) as u32,
    }
}

/// A grid built the way the renderer builds one: put_str with random styles at random places.
fn arb_grid(cols: u16, rows: u16) -> impl Strategy<Value = Grid> {
    let op = (
        0..rows,
        0..cols,
        prop::collection::vec(prop::sample::select(ALPHABET), 0..12),
        0u8..6,
    );
    prop::collection::vec(op, 0..40).prop_map(move |ops| {
        let mut g = Grid::new(cols, rows, Style::default());
        for (y, x, parts, st) in ops {
            g.put_str(x, y, &parts.concat(), style(st), cols);
        }
        g
    })
}

proptest! {
    #[test]
    fn grid_invariants_hold_after_random_writes(g in arb_grid(12, 4)) {
        prop_assert_eq!(g.check_invariants(), Ok(()));
    }

    #[test]
    fn diff_then_apply_reproduces_target(a in arb_grid(14, 5), b in arb_grid(14, 5)) {
        let mut x = a.clone();
        apply_runs(&mut x, &diff_runs(&a, &b));
        prop_assert_eq!(x.check_invariants(), Ok(()));
        prop_assert_eq!(&x, &b);
    }

    #[test]
    fn full_frame_into_blank_reproduces_grid(g in arb_grid(14, 5)) {
        let mut x = Grid::new(14, 5, Style::default());
        apply_runs(&mut x, &full_runs(&g));
        prop_assert_eq!(&x, &g);
    }

    #[test]
    fn identical_grids_produce_no_runs(g in arb_grid(10, 3)) {
        prop_assert!(diff_runs(&g, &g).is_empty());
    }

    #[test]
    fn cluster_width_is_bounded_and_consistent(s in "\\PC{0,24}") {
        for g in clusters(&s) {
            prop_assert!(cluster_width(g) <= 2);
        }
        prop_assert_eq!(str_width(&s), clusters(&s).map(|g| cluster_width(g) as usize).sum::<usize>());
    }

    #[test]
    fn utf16_slice_matches_utf16_encoding(s in "\\PC{0,20}", start in 0usize..24, len in 0usize..24) {
        // Compare with slicing the UTF-16 encoding, when the cut lands on char boundaries.
        let units: Vec<u16> = s.encode_utf16().collect();
        let end = (start + len).min(units.len());
        let st = start.min(units.len());
        if let Ok(expect) = String::from_utf16(&units[st..end]) {
            prop_assert_eq!(utf16_slice(&s, start, len), expect);
        }
    }
}

#[test]
fn wide_clusters_and_edges() {
    assert_eq!(cluster_width("a"), 1);
    assert_eq!(cluster_width("日"), 2);
    assert_eq!(cluster_width("😀"), 2);
    assert_eq!(cluster_width("🇦🇹"), 2);
    assert_eq!(cluster_width("👨‍👩‍👧"), 2);
    assert_eq!(cluster_width("e\u{301}"), 1);
    assert_eq!(cluster_width("\u{200b}"), 0);
    assert_eq!(cluster_width("\t"), 0);
    // A wide glyph does not fit in the last column: it becomes a space, never a half glyph.
    let mut g = Grid::new(3, 1, Style::default());
    g.put_str(0, 0, "ab日", Style::default(), 3);
    assert_eq!(g.dump_text(), "ab\n");
    g.put(2, 0, "日", Style::default());
    assert_eq!(g.dump_text(), "ab\n");
    assert!(g.check_invariants().is_ok());
    // Overwriting half of a wide glyph blanks the other half.
    let mut g = Grid::new(4, 1, Style::default());
    g.put(0, 0, "日", Style::default());
    g.put(1, 0, "x", Style::default());
    assert_eq!(g.cell(0, 0), &Cell::blank(Style::default()));
    assert!(g.check_invariants().is_ok());
}
