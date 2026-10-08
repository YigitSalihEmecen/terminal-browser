//! Resource profiles do real work: blocked requests never reach the network, background tabs
//! are discarded and revived. Skipped without Chromium.

use std::{path::PathBuf, time::Duration};

use glyph_proto::*;
use glyph_server::{browser::find_chrome, testserver, Server, ServerCfg};
use tokio::time::timeout;

fn caps(images: bool) -> ClientCaps {
    ClientCaps {
        cols: 80,
        rows: 20,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        images,
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

async fn drain_until(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<ServerMsg>,
    tx: &tokio::sync::mpsc::UnboundedSender<ClientMsg>,
    mut pred: impl FnMut(&Grid, &[TabInfo]) -> bool,
) {
    let mut grid: Option<Grid> = None;
    let mut tabs = vec![];
    let r = timeout(Duration::from_secs(20), async {
        loop {
            let Some(m) = rx.recv().await else {
                panic!("closed")
            };
            match m {
                ServerMsg::FullFrame {
                    tab,
                    seq,
                    cols,
                    rows,
                    runs,
                } => {
                    let mut g = Grid::new(cols, rows, Style::default());
                    apply_runs(&mut g, &runs);
                    grid = Some(g);
                    let _ = tx.send(ClientMsg::Ack { tab, seq });
                }
                ServerMsg::Diff { tab, seq, runs, .. } => {
                    apply_runs(grid.as_mut().unwrap(), &runs);
                    let _ = tx.send(ClientMsg::Ack { tab, seq });
                }
                ServerMsg::Tabs { tabs: t, .. } => tabs = t,
                ServerMsg::Error(e) => panic!("{e}"),
                _ => {}
            }
            if let Some(g) = &grid {
                if pred(g, &tabs) {
                    return;
                }
            }
        }
    })
    .await;
    assert!(
        r.is_ok(),
        "timed out; screen:\n{}",
        grid.map(|g| g.dump_text()).unwrap_or_default()
    );
}

async fn pages(s: &Server) -> usize {
    let v: serde_json::Value = s
        .browser
        .cdp()
        .call(None, "Target.getTargets", serde_json::json!({}))
        .await
        .unwrap();
    v["targetInfos"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|t| t["type"] == "page")
        .count()
}

async fn requests_for(profile: Profile, images: bool) -> Vec<String> {
    let (addr, log) = testserver::serve_dir_logged(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg {
        profile,
        chrome_args: vec!["--host-resolver-rules=MAP ads.doubleclick.net 127.0.0.1".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    let mut h = srv.open_session(caps(images));
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/assets.html"),
    })
    .unwrap();
    drain_until(&mut h.rx, &h.tx, |g, _| {
        g.dump_text().contains("Assets page")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await; // let subresources settle
    let l = log.lock().unwrap().clone();
    srv.browser.close().await;
    l
}

#[tokio::test]
async fn lean_blocks_images_fonts_and_trackers_full_does_not() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let lean = requests_for(Profile::Lean, false).await;
    assert!(lean.contains(&"/assets.html".to_string()), "{lean:?}");
    assert!(
        !lean.contains(&"/favicon.ico".to_string()),
        "lean fetched a favicon: {lean:?}"
    );
    assert!(
        !lean.contains(&"/pixel.png".to_string()),
        "lean fetched an image: {lean:?}"
    );
    assert!(
        !lean.contains(&"/f.woff2".to_string()),
        "lean fetched a font: {lean:?}"
    );
    assert!(
        !lean.contains(&"/x.js".to_string()),
        "lean fetched a tracker script: {lean:?}"
    );

    // a client that can show images turns image blocking off even in lean
    let lean_img = requests_for(Profile::Lean, true).await;
    assert!(lean_img.contains(&"/pixel.png".to_string()), "{lean_img:?}");
    assert!(!lean_img.contains(&"/f.woff2".to_string()), "{lean_img:?}");

    let full = requests_for(Profile::Full, false).await;
    for p in ["/pixel.png", "/f.woff2", "/x.js"] {
        assert!(
            full.contains(&p.to_string()),
            "full should fetch {p}: {full:?}"
        );
    }
}

#[tokio::test]
async fn background_tabs_are_discarded_then_revived_on_activation() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr = testserver::serve_dir(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg {
        profile: Profile::Lean,
        discard_after_secs: Some(2),
        ..Default::default()
    })
    .await
    .unwrap();
    let base = pages(&srv).await; // Chromium's own launch tab
    let mut h = srv.open_session(caps(false));
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/basic.html"),
    })
    .unwrap();
    drain_until(&mut h.rx, &h.tx, |g, _| {
        g.dump_text().contains("Hello glyph")
    })
    .await;
    h.tx.send(ClientMsg::NewTab {
        url: Some(format!("http://{addr}/unicode.html")),
    })
    .unwrap();
    drain_until(&mut h.rx, &h.tx, |g, t| {
        t.len() == 2 && g.dump_text().contains("CJK")
    })
    .await;
    let first = 1u32;

    assert_eq!(pages(&srv).await, base + 2);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(
        pages(&srv).await,
        base + 1,
        "the background tab's browser target should be gone"
    );

    h.tx.send(ClientMsg::SwitchTab(first)).unwrap();
    drain_until(&mut h.rx, &h.tx, |g, _| {
        g.dump_text().contains("Hello glyph")
    })
    .await;
    assert_eq!(pages(&srv).await, base + 2, "revived into a fresh target");
    srv.browser.close().await;
}

#[tokio::test]
async fn metrics_see_the_chromium_process_tree_and_count_frames() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr = testserver::serve_dir(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg::default()).await.unwrap();
    let mut h = srv.open_session(caps(false));
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/basic.html"),
    })
    .unwrap();
    drain_until(&mut h.rx, &h.tx, |g, _| {
        g.dump_text().contains("Hello glyph")
    })
    .await;

    let pid = srv.browser.pid().expect("pid");
    let s = glyph_server::metrics::sample_tree(pid).expect("ps sample");
    assert!(s.procs >= 2, "browser + renderer expected: {s:?}");
    assert!(
        s.rss_kb > 50_000,
        "a loaded Chromium is well over 50 MB: {s:?}"
    );

    let m = srv.metrics.snapshot();
    assert!(m.frames_full >= 1 && m.msgs >= m.frames_full, "{m:?}");
    assert!(m.refreshes >= 1 && m.avg_refresh_ms > 0.0, "{m:?}");
    srv.browser.close().await;
}
