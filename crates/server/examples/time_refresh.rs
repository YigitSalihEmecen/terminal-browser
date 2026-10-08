//! Where does one refresh spend its time? (CDP snapshot vs our parse vs our render)
//!   cargo run --release -p glyph-server --example time_refresh -- bench/pages/dense.html
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use glyph_server::{
    browser::*,
    capture,
    page::Page,
    pixmap::Pixmap,
    render::{render, Metrics},
    snapshot::SnapshotResult,
    testserver,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file = PathBuf::from(std::env::args().nth(1).expect("html file"));
    let dir = file.parent().unwrap().to_path_buf();
    let name = file.file_name().unwrap().to_string_lossy().to_string();
    let addr = testserver::serve_dir(dir).await?;
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let (target_id, session) = b.new_target(None).await?;
    let page = Page {
        session: session.clone(),
        target_id,
    };
    let m = Metrics {
        cols: 120,
        rows: 40,
        cw: 8.0,
        ch: 16.0,
    };
    capture::set_viewport(&session, &m).await?;
    page.navigate(&format!("http://{addr}/{name}"), Duration::from_secs(30))
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let n = 20;
    let (mut t_cdp, mut t_parse, mut t_jpeg, mut t_render) = (
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
        Duration::ZERO,
    );
    let mut bytes = 0;
    for _ in 0..n {
        let t = Instant::now();
        let raw = capture::snapshot_raw(&session).await?;
        t_cdp += t.elapsed();
        bytes = raw.get().len();
        let t = Instant::now();
        let snap: SnapshotResult = serde_json::from_str(raw.get())?;
        t_parse += t.elapsed();
        let jpeg = capture::screenshot_jpeg(&session, 45).await?;
        let t = Instant::now();
        let pix = Pixmap::decode_jpeg(&jpeg)?;
        t_jpeg += t.elapsed();
        let t = Instant::now();
        let _ = render(&snap, Some(&pix), &m);
        t_render += t.elapsed();
    }
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n as f64;
    println!("{name}: snapshot json {} KB", bytes / 1024);
    println!("  chromium builds + sends snapshot : {:6.1} ms", ms(t_cdp));
    println!(
        "  glyph parses snapshot json       : {:6.1} ms",
        ms(t_parse)
    );
    println!("  glyph decodes screenshot jpeg    : {:6.1} ms", ms(t_jpeg));
    println!(
        "  glyph renders cells              : {:6.1} ms",
        ms(t_render)
    );
    b.shutdown().await;
    Ok(())
}
