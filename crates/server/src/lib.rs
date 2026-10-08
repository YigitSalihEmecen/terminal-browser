//! glyph server: drives Chromium over CDP and turns pages into character-cell grids.

pub mod browser;
pub mod cdp;
pub mod page;

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
