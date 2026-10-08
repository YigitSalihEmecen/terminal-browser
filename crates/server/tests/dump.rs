//! Needs a Chromium; skipped (with a message) when none is installed.
use std::time::Duration;

#[tokio::test]
async fn dumps_text_of_data_url() {
    if glyph_server::browser::find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let text = glyph_server::dump_text(
        "data:text/html;charset=utf-8,<h1>Hello</h1><p>日本語 😀</p>",
        Duration::from_secs(20),
    )
    .await
    .unwrap();
    assert!(
        text.contains("Hello") && text.contains("日本語 😀"),
        "{text:?}"
    );
}
