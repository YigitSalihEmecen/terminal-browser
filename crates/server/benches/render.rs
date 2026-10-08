//! Renderer and reader-mode cost on the recorded Chromium fixtures (no browser needed).
use std::path::PathBuf;

use criterion::{criterion_group, criterion_main, Criterion};
use glyph_server::{
    pixmap::Pixmap,
    render::{render, Metrics},
    snapshot::SnapshotResult,
    textmode::{self, AxTree},
};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/recorded")
}

fn bench(c: &mut Criterion) {
    let m = Metrics {
        cols: 100,
        rows: 30,
        cw: 8.0,
        ch: 16.0,
    };
    let mut g = c.benchmark_group("render");
    for name in ["basic", "form", "article", "overlay"] {
        let raw = std::fs::read(dir().join(format!("{name}.snapshot.json"))).unwrap();
        let jpeg = std::fs::read(dir().join(format!("{name}.jpg"))).unwrap();
        g.bench_function(
            format!("parse snapshot json: {name} ({} KB)", raw.len() / 1024),
            |b| {
                b.iter(|| {
                    std::hint::black_box(serde_json::from_slice::<SnapshotResult>(&raw).unwrap())
                })
            },
        );
        g.bench_function(format!("decode screenshot jpeg: {name}"), |b| {
            b.iter(|| std::hint::black_box(Pixmap::decode_jpeg(&jpeg).unwrap()))
        });
        let snap: SnapshotResult = serde_json::from_slice(&raw).unwrap();
        let pix = Pixmap::decode_jpeg(&jpeg).unwrap();
        g.bench_function(format!("cells from snapshot only: {name}"), |b| {
            b.iter(|| std::hint::black_box(render(&snap, None, &m)))
        });
        g.bench_function(format!("cells from snapshot + pixels: {name}"), |b| {
            b.iter(|| std::hint::black_box(render(&snap, Some(&pix), &m)))
        });
    }
    g.finish();

    let mut g = c.benchmark_group("reader");
    let ax: AxTree =
        serde_json::from_slice(&std::fs::read(dir().join("article.ax.json")).unwrap()).unwrap();
    let disp: SnapshotResult =
        serde_json::from_slice(&std::fs::read(dir().join("article.display.json")).unwrap())
            .unwrap();
    let displays = textmode::displays_from_snapshot(&disp);
    g.bench_function("build document: article", |b| {
        b.iter(|| std::hint::black_box(textmode::build(&ax, &displays, 100)))
    });
    let doc = textmode::build(&ax, &displays, 100);
    g.bench_function("slice viewport (a scroll step)", |b| {
        b.iter(|| std::hint::black_box(doc.slice(5, 30, None)))
    });
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
