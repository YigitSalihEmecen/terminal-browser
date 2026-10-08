//! Diff computation, application and wire codec on realistic grids.
use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use glyph_proto::{apply_runs, codec, diff_runs, full_runs, Attrs, Grid, Rgb, ServerMsg, Style};

const COLS: u16 = 200;
const ROWS: u16 = 60;

/// A page-like grid: text lines in a few styles, a coloured banner, some wide glyphs.
fn page(seed: u32) -> Grid {
    let mut g = Grid::new(COLS, ROWS, Style::new(Rgb(0x22, 0x22, 0x22), Rgb::WHITE));
    let link = Style {
        fg: Rgb(0, 0x66, 0xcc),
        bg: Rgb::WHITE,
        attrs: Attrs::UNDERLINE,
        link: 3,
    };
    for y in 0..ROWS {
        let text = format!("{seed:04} line {y}: the quick brown fox jumps over the lazy dog 日本語 and some more text to fill the row out");
        g.put_str(
            2,
            y,
            &text,
            Style::new(Rgb(0x22, 0x22, 0x22), Rgb::WHITE),
            COLS - 2,
        );
        if y % 5 == 0 {
            g.put_str(60, y, "a link in the line", link, COLS);
        }
    }
    g
}

fn bench(c: &mut Criterion) {
    let (a, b) = (page(1), page(1));
    // typical interaction: a handful of lines change (a ticker, a typed character, a hover)
    let mut small = a.clone();
    for y in [3u16, 4, 20] {
        small.put_str(
            10,
            y,
            "changed text here",
            Style::new(Rgb(0, 0, 0), Rgb(255, 255, 0)),
            COLS,
        );
    }
    let scrolled = page(2); // everything changes: a page scroll

    let mut g = c.benchmark_group("diff");
    g.throughput(Throughput::Elements(COLS as u64 * ROWS as u64));
    g.bench_function("identical 200x60", |bn| {
        bn.iter(|| std::hint::black_box(diff_runs(&a, &b)))
    });
    g.bench_function("3 lines changed", |bn| {
        bn.iter(|| std::hint::black_box(diff_runs(&a, &small)))
    });
    g.bench_function("whole screen changed", |bn| {
        bn.iter(|| std::hint::black_box(diff_runs(&a, &scrolled)))
    });
    g.bench_function("full frame encode", |bn| {
        bn.iter(|| std::hint::black_box(full_runs(&a)))
    });
    let runs = diff_runs(&a, &scrolled);
    g.bench_function("apply whole-screen diff", |bn| {
        bn.iter_batched(
            || a.clone(),
            |mut x| apply_runs(&mut x, std::hint::black_box(&runs)),
            criterion::BatchSize::SmallInput,
        )
    });
    g.finish();

    let mut g = c.benchmark_group("codec");
    let full = ServerMsg::FullFrame {
        tab: 1,
        seq: 1,
        cols: COLS,
        rows: ROWS,
        runs: full_runs(&a),
    };
    let small_diff = ServerMsg::Diff {
        tab: 1,
        seq: 2,
        base: 1,
        runs: diff_runs(&a, &small),
    };
    let big_diff = ServerMsg::Diff {
        tab: 1,
        seq: 2,
        base: 1,
        runs: runs.clone(),
    };
    for (name, m) in [
        ("full frame", &full),
        ("small diff", &small_diff),
        ("whole-screen diff", &big_diff),
    ] {
        let raw = postcard::to_stdvec(m).unwrap().len();
        let mut e = codec::Encoder::new(3).unwrap();
        let wire = e.encode(m).unwrap().len();
        eprintln!("{name}: raw postcard {raw} B, steady-state-ish wire {wire} B");
        g.throughput(Throughput::Bytes(raw as u64));
        g.bench_function(format!("encode {name}"), |bn| {
            let mut e = codec::Encoder::new(3).unwrap();
            bn.iter(|| std::hint::black_box(e.encode(m).unwrap()))
        });
        g.bench_function(format!("roundtrip {name}"), |bn| {
            let (mut e, mut d) = (
                codec::Encoder::new(3).unwrap(),
                codec::Decoder::new().unwrap(),
            );
            bn.iter(|| {
                let bytes = e.encode(m).unwrap();
                std::hint::black_box(d.decode::<ServerMsg>(&bytes).unwrap())
            })
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
