//! Dev probe: print the raw DOMSnapshot JSON for a URL (used to write/refresh fixtures).
use std::time::Duration;

use glyph_server::{browser::*, page::Page};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).expect("url");
    let (cols, rows): (u32, u32) = (100, 30);
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let (target_id, session) = b.new_target(None).await?;
    let page = Page {
        session: session.clone(),
        target_id,
    };
    session
        .send(
            "Emulation.setDeviceMetricsOverride",
            json!({"width": cols*8, "height": rows*16, "deviceScaleFactor": 1, "mobile": false}),
        )
        .await?;
    page.navigate(&url, Duration::from_secs(20)).await?;
    let styles = [
        "color",
        "background-color",
        "font-weight",
        "font-style",
        "text-decoration-line",
        "visibility",
        "opacity",
        "overflow-x",
        "overflow-y",
        "position",
        "background-image",
        "font-size",
        "border-top-width",
        "border-top-style",
        "border-top-color",
        "border-right-width",
        "border-right-style",
        "border-right-color",
        "border-bottom-width",
        "border-bottom-style",
        "border-bottom-color",
        "border-left-width",
        "border-left-style",
        "border-left-color",
        "border-top-left-radius",
    ];
    let raw = session
        .call_raw(
            "DOMSnapshot.captureSnapshot",
            json!({"computedStyles": styles, "includePaintOrder": true, "includeDOMRects": false,
            "includeBlendedBackgroundColors": true, "includeTextColorOpacities": true}),
        )
        .await?;
    println!("{}", raw.get());
    b.shutdown().await;
    Ok(())
}
