//! One-shot capture of a page: snapshot + screenshot → [`Rendered`].

use anyhow::Result;
use serde_json::json;

use crate::{
    cdp::Session,
    pixmap::Pixmap,
    render::{render, Metrics, Rendered},
    snapshot::{SnapshotResult, COMPUTED_STYLES},
};

pub async fn snapshot_raw(s: &Session) -> Result<Box<serde_json::value::RawValue>> {
    s.call_raw(
        "DOMSnapshot.captureSnapshot",
        json!({ "computedStyles": COMPUTED_STYLES, "includePaintOrder": true, "includeDOMRects": false }),
    )
    .await
}

pub async fn snapshot(s: &Session) -> Result<SnapshotResult> {
    Ok(serde_json::from_str(snapshot_raw(s).await?.get())?)
}

pub async fn screenshot_jpeg(s: &Session, quality: u8) -> Result<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct R {
        data: String,
    }
    let r: R = s
        .call(
            "Page.captureScreenshot",
            json!({ "format": "jpeg", "quality": quality, "optimizeForSpeed": true, "fromSurface": true }),
        )
        .await?;
    crate::b64::decode(&r.data)
}

pub async fn screenshot(s: &Session, quality: u8) -> Result<Pixmap> {
    Pixmap::decode_jpeg(&screenshot_jpeg(s, quality).await?)
}

pub async fn set_viewport(s: &Session, m: &Metrics) -> Result<()> {
    s.send(
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": m.px_w() as u32, "height": m.px_h() as u32, "deviceScaleFactor": 1, "mobile": false }),
    )
    .await
}

pub async fn render_now(s: &Session, m: &Metrics, with_pixels: bool) -> Result<Rendered> {
    let snap = snapshot(s).await?;
    let pix = if with_pixels {
        Some(screenshot(s, 50).await?)
    } else {
        None
    };
    Ok(render(&snap, pix.as_ref(), m))
}
