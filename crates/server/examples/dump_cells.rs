use glyph_server::{
    pixmap::Pixmap,
    render::{render, Metrics},
    snapshot::SnapshotResult,
};
use std::path::PathBuf;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (name, x0, x1, y0, y1): (&str, u16, u16, u16, u16) = (
        &a[1],
        a[2].parse().unwrap(),
        a[3].parse().unwrap(),
        a[4].parse().unwrap(),
        a[5].parse().unwrap(),
    );
    let dir = PathBuf::from("fixtures/recorded/gallery");
    let snap: SnapshotResult =
        serde_json::from_slice(&std::fs::read(dir.join(format!("{name}.snapshot.json"))).unwrap())
            .unwrap();
    let pix =
        Pixmap::decode_jpeg(&std::fs::read(dir.join(format!("{name}.jpg"))).unwrap()).unwrap();
    let r = render(
        &snap,
        Some(&pix),
        &Metrics {
            cols: 100,
            rows: 30,
            cw: 8.0,
            ch: 16.0,
        },
    );
    for y in y0..=y1 {
        for x in x0..=x1 {
            let c = r.grid.cell(x, y);
            let s = c.style;
            println!("({x},{y}) {:?} fg={:?} bg={:?}", c.g, s.fg, s.bg);
        }
    }
}
