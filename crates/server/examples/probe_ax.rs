//! Dev probe: dump the raw accessibility tree for a URL.
use std::time::Duration;

use glyph_server::{browser::*, page::Page};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let url = std::env::args().nth(1).expect("url");
    let b = Browser::launch(&LaunchOptions::default()).await?;
    let (target_id, session) = b.new_target(None).await?;
    let page = Page {
        session: session.clone(),
        target_id,
    };
    page.navigate(&url, Duration::from_secs(20)).await?;
    session.send("Accessibility.enable", json!({})).await?;
    let raw = session
        .call_raw("Accessibility.getFullAXTree", json!({}))
        .await?;
    println!("{}", raw.get());
    b.shutdown().await;
    Ok(())
}
