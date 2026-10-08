//! glyph server: drives Chromium over CDP and turns pages into character-cell grids.

pub mod b64;
pub mod browser;
pub mod capture;
pub mod cdp;
pub mod keys;
pub mod metrics;
pub mod net;
pub mod outbox;
pub mod page;
pub mod pixmap;
pub mod profile;
pub mod render;
pub mod server;
pub mod snapshot;
pub mod tab;
pub mod testserver;
pub mod textmode;

pub use server::{Server, ServerCfg, SessionHandle};

use std::time::Duration;

use anyhow::Result;

/// M0 entry point: launch Chromium, load `url`, return the page's visible text.
pub async fn dump_text(url: &str, timeout: Duration) -> Result<String> {
    let browser = browser::Browser::launch(&browser::LaunchOptions::default()).await?;
    let result = async {
        let (target_id, session) = browser.new_target(None).await?;
        let page = page::Page { session, target_id };
        page.navigate(url, timeout).await?;
        page.text().await
    }
    .await;
    browser.shutdown().await;
    result
}

/// Load `url` at `cols × rows` cells and render it once (CLI `glyph render`).
pub async fn render_url(
    url: &str,
    cols: u16,
    rows: u16,
    pixels: bool,
    timeout: Duration,
) -> Result<render::Rendered> {
    let browser = browser::Browser::launch(&browser::LaunchOptions::default()).await?;
    let result = async {
        let (target_id, session) = browser.new_target(None).await?;
        let page = page::Page {
            session: session.clone(),
            target_id,
        };
        let m = render::Metrics {
            cols,
            rows,
            cw: 8.0,
            ch: 16.0,
        };
        capture::set_viewport(&session, &m).await?;
        page.navigate(url, timeout).await?;
        tokio::time::sleep(Duration::from_millis(150)).await; // let first paint settle
        capture::render_now(&session, &m, pixels).await
    }
    .await;
    browser.shutdown().await;
    result
}
