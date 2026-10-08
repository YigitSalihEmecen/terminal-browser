//! Re-record the gallery fixtures through the real page agent (terminal-native style), exactly
//! as a live session would see them:
//!   cargo run -p glyph-server --example record_gallery
//! Writes fixtures/recorded/gallery/<name>.snapshot.json and <name>.jpg (100×30 cells).
use std::{path::PathBuf, time::Duration};

use glyph_server::{browser::*, capture, page::Page, render::Metrics, tab::agent_js, testserver};
use serde_json::json;

const SKIP: &[&str] = &["index", "tear", "longpage"];

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/gallery");
    let out = root.join("../recorded/gallery");
    std::fs::create_dir_all(&out)?;
    let addr = testserver::serve_dir(root.clone()).await?;
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let m = Metrics {
        cols: 100,
        rows: 30,
        cw: 8.0,
        ch: 16.0,
    };
    let mut names: Vec<_> = std::fs::read_dir(&root)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("html"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .filter(|n| !SKIP.contains(&n.as_str()))
        .collect();
    names.sort();
    for name in names {
        let (target_id, s) = b.new_target(None).await?;
        let page = Page {
            session: s.clone(),
            target_id: target_id.clone(),
        };
        s.send("Page.enable", json!({})).await?;
        s.send("Runtime.enable", json!({})).await?;
        s.send("Runtime.addBinding", json!({ "name": "__glyph" }))
            .await?;
        s.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": agent_js(true, m.cw, m.ch) }),
        )
        .await?;
        s.send(
            "Emulation.setEmulatedMedia",
            json!({ "features": [{ "name": "prefers-reduced-motion", "value": "reduce" }] }),
        )
        .await?;
        capture::set_viewport(&s, &m).await?;
        page.navigate(
            &format!("http://{addr}/{name}.html"),
            Duration::from_secs(20),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        s.call::<serde_json::Value>(
            "Runtime.evaluate",
            json!({ "expression": "window.__glyphSettled()", "awaitPromise": true }),
        )
        .await?;
        std::fs::write(
            out.join(format!("{name}.snapshot.json")),
            capture::snapshot_raw(&s).await?.get(),
        )?;
        std::fs::write(
            out.join(format!("{name}.jpg")),
            capture::screenshot_jpeg(&s, 85).await?,
        )?;
        b.close_target(&target_id).await?;
        println!("recorded gallery/{name}");
    }
    b.shutdown().await;
    Ok(())
}
