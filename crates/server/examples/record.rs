//! Re-record render fixtures from live Chromium:
//!   cargo run -p glyph-server --example record
//! Writes fixtures/recorded/<name>.snapshot.json and <name>.jpg (100×30 cells, 8×16 px).
use std::{path::PathBuf, time::Duration};

use glyph_server::{browser::*, capture, page::Page, render::Metrics, testserver};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    let out = root.join("recorded");
    std::fs::create_dir_all(&out)?;
    let addr = testserver::serve_dir(root.clone()).await?;
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let m = Metrics {
        cols: 100,
        rows: 30,
        cw: 8.0,
        ch: 16.0,
    };
    for entry in std::fs::read_dir(&root)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("html") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().to_string();
        let (target_id, session) = b.new_target(None).await?;
        let page = Page {
            session: session.clone(),
            target_id: target_id.clone(),
        };
        capture::set_viewport(&session, &m).await?;
        page.navigate(
            &format!("http://{addr}/{name}.html"),
            Duration::from_secs(20),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        std::fs::write(
            out.join(format!("{name}.snapshot.json")),
            capture::snapshot_raw(&session).await?.get(),
        )?;
        std::fs::write(
            out.join(format!("{name}.jpg")),
            capture::screenshot_jpeg(&session, 60).await?,
        )?;
        // reader mode inputs: AX tree + a display-only snapshot (block vs inline)
        session
            .send("Accessibility.enable", serde_json::json!({}))
            .await?;
        let ax = session
            .call_raw("Accessibility.getFullAXTree", serde_json::json!({}))
            .await?;
        std::fs::write(out.join(format!("{name}.ax.json")), ax.get())?;
        let disp = session
            .call_raw(
                "DOMSnapshot.captureSnapshot",
                serde_json::json!({ "computedStyles": ["display"], "includePaintOrder": false, "includeDOMRects": false }),
            )
            .await?;
        std::fs::write(out.join(format!("{name}.display.json")), disp.get())?;
        b.close_target(&target_id).await?;
        println!("recorded {name}");
    }
    b.shutdown().await;
    Ok(())
}
