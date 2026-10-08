//! Time the phases of one consistent capture on a page (what limits scroll frame rate?).
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use glyph_server::{browser::*, capture, page::Page, render::Metrics, testserver};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file = PathBuf::from(std::env::args().nth(1).expect("html file"));
    let addr = testserver::serve_dir(file.parent().unwrap().to_path_buf()).await?;
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let (target_id, s) = b.new_target(None).await?;
    let page = Page {
        session: s.clone(),
        target_id,
    };
    let m = Metrics {
        cols: 100,
        rows: 30,
        cw: 8.0,
        ch: 16.0,
    };
    capture::set_viewport(&s, &m).await?;
    page.navigate(
        &format!(
            "http://{addr}/{}",
            file.file_name().unwrap().to_string_lossy()
        ),
        Duration::from_secs(20),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let n = 15;
    let mut t = [Duration::ZERO; 5];
    for i in 0..n {
        s.call::<serde_json::Value>(
            "Runtime.evaluate",
            json!({"expression": format!("scrollBy(0,{})", 48 * (i % 2 + 1))}),
        )
        .await?;
        let a = Instant::now();
        s.call::<serde_json::Value>("Runtime.evaluate", json!({"expression":"new Promise(r=>requestAnimationFrame(()=>requestAnimationFrame(()=>r(1))))","awaitPromise":true})).await?;
        t[0] += a.elapsed();
        let a = Instant::now();
        let _ = capture::snapshot_raw(&s).await?;
        t[1] += a.elapsed();
        let a = Instant::now();
        let _ = capture::screenshot_jpeg(&s, 80).await?;
        t[2] += a.elapsed();
        let a = Instant::now();
        s.call::<serde_json::Value>(
            "Runtime.evaluate",
            json!({"expression":"[scrollX,scrollY]","returnByValue":true}),
        )
        .await?;
        t[3] += a.elapsed();
    }
    let ms = |d: Duration| d.as_secs_f64() * 1000.0 / n as f64;
    println!(
        "two rAFs {:6.1} ms | snapshot {:6.1} ms | screenshot {:6.1} ms | scroll read {:6.1} ms",
        ms(t[0]),
        ms(t[1]),
        ms(t[2]),
        ms(t[3])
    );
    b.shutdown().await;
    Ok(())
}
