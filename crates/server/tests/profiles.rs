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
        scheme: Default::default(),
        page_style: Default::default(),
        images,
    }
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// What a client would be showing. Kept across calls: diffs are relative to earlier frames.
#[derive(Default)]
struct Screen {
    grid: Option<Grid>,
    tabs: Vec<TabInfo>,
    images: Vec<ImageMsg>,
    cleared: Vec<u32>,
}

impl Screen {
    /// Consume messages until `pred` holds or `ms` elapse. Returns whether it held.
    async fn until_or_timeout(
        &mut self,
        h: &mut glyph_server::SessionHandle,
        mut pred: impl FnMut(&Grid, &[TabInfo]) -> bool,
        ms: u64,
    ) -> bool {
        timeout(Duration::from_millis(ms), async {
            loop {
                if let Some(g) = &self.grid {
                    if pred(g, &self.tabs) {
                        return;
                    }
                }
                let Some(m) = h.rx.recv().await else {
                    panic!("session closed")
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
                        self.grid = Some(g);
                        let _ = h.tx.send(ClientMsg::Ack { tab, seq });
                    }
                    ServerMsg::Diff { tab, seq, runs, .. } => {
                        apply_runs(
                            self.grid.as_mut().expect("diff before any full frame"),
                            &runs,
                        );
                        let _ = h.tx.send(ClientMsg::Ack { tab, seq });
                    }
                    ServerMsg::Tabs { tabs, .. } => self.tabs = tabs,
                    ServerMsg::Image(i) => self.images.push(i),
                    ServerMsg::ImageClear { ids, .. } => self.cleared.extend(ids),
                    ServerMsg::Error(e) => panic!("{e}"),
                    _ => {}
                }
            }
        })
        .await
        .is_ok()
    }

    /// Keep consuming messages for `ms` (e.g. to see what a quiet period produces).
    async fn settle(&mut self, h: &mut glyph_server::SessionHandle, ms: u64) {
        self.until_or_timeout(h, |_, _| false, ms).await;
    }

    async fn until(
        &mut self,
        h: &mut glyph_server::SessionHandle,
        pred: impl FnMut(&Grid, &[TabInfo]) -> bool,
    ) {
        let ok = self.until_or_timeout(h, pred, 20_000).await;
        assert!(
            ok,
            "timed out; screen:\n{}",
            self.grid
                .as_ref()
                .map(|g| g.dump_text())
                .unwrap_or_default()
        );
    }
}

/// Pages that belong to sessions (non-default browser contexts). Chromium's own launch tab, if it
/// is still being torn down, is in the default context and does not count.
async fn pages(s: &Server) -> usize {
    let ctxs: serde_json::Value = s
        .browser
        .cdp()
        .call(None, "Target.getBrowserContexts", serde_json::json!({}))
        .await
        .unwrap();
    let ours: Vec<&str> = ctxs["browserContextIds"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|c| c.as_str())
        .collect();
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
        .filter(|t| {
            t["type"] == "page"
                && t["browserContextId"]
                    .as_str()
                    .is_some_and(|c| ours.contains(&c))
        })
        .count()
}

async fn requests_for(profile: Profile, images: bool, style: PageStyle) -> Vec<String> {
    let (addr, log) = testserver::serve_dir_logged(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg {
        profile,
        chrome_args: vec!["--host-resolver-rules=MAP ads.doubleclick.net 127.0.0.1".into()],
        ..Default::default()
    })
    .await
    .unwrap();
    let mut h = srv.open_session(ClientCaps {
        page_style: style,
        ..caps(images)
    });
    let mut screen = Screen::default();
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/assets.html"),
    })
    .unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Assets page"))
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
    let lean = requests_for(Profile::Lean, false, PageStyle::Faithful).await;
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
    let lean_img = requests_for(Profile::Lean, true, PageStyle::Faithful).await;
    assert!(lean_img.contains(&"/pixel.png".to_string()), "{lean_img:?}");
    assert!(!lean_img.contains(&"/f.woff2".to_string()), "{lean_img:?}");

    // Terminal-style pages are laid out in a monospace font, so a page's web fonts are never even
    // requested, whatever the profile: a bandwidth saving that needs no blocklist.
    let term = requests_for(Profile::Full, false, PageStyle::Terminal).await;
    assert!(
        !term.contains(&"/f.woff2".to_string()),
        "terminal style still fetched the web font: {term:?}"
    );
    assert!(
        term.contains(&"/pixel.png".to_string()),
        "images are unaffected: {term:?}"
    );

    let full = requests_for(Profile::Full, false, PageStyle::Faithful).await;
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
    let base = 0; // only session pages are counted
    let mut h = srv.open_session(caps(false));
    let mut screen = Screen::default();
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/basic.html"),
    })
    .unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Hello glyph"))
        .await;
    h.tx.send(ClientMsg::NewTab {
        url: Some(format!("http://{addr}/unicode.html")),
    })
    .unwrap();
    screen
        .until(&mut h, |g, t| t.len() == 2 && g.dump_text().contains("CJK"))
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
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Hello glyph"))
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
    let mut screen = Screen::default();
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/basic.html"),
    })
    .unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Hello glyph"))
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

#[tokio::test]
async fn private_network_guard_blocks_without_deadlocking_and_trackers_stay_navigable() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let (addr, log) = testserver::serve_dir_logged(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg {
        profile: Profile::Lean,
        block_private: true,
        chrome_args: vec![
            "--host-resolver-rules=MAP segment.com 127.0.0.1, MAP public.test 127.0.0.1".into(),
        ],
        ..Default::default()
    })
    .await
    .unwrap();
    let mut h = srv.open_session(caps(false));
    let mut screen = Screen::default();

    // a loopback target is refused: the load ends (does not hang) and the server never sees it
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://127.0.0.1:{}/basic.html", addr.port()),
    })
    .unwrap();
    let mut finished = false;
    let r = timeout(Duration::from_secs(10), async {
        while let Some(m) = h.rx.recv().await {
            if let ServerMsg::LoadState { state, .. } = m {
                finished |= !state.loading && state.progress == 100;
                if finished {
                    break;
                }
            }
        }
    })
    .await;
    assert!(
        r.is_ok() && finished,
        "navigation to a private address never finished (deadlock?)"
    );
    assert!(
        !log.lock().unwrap().contains(&"/basic.html".to_string()),
        "request reached the private server: {:?}",
        log.lock().unwrap()
    );

    // a tracker domain as a *page* is still reachable (public.test stands in for a public host;
    // segment.com is on the tracker list but must not be blocked as a top-level document)
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://segment.com:{}/basic.html", addr.port()),
    })
    .unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Hello glyph"))
        .await;
    srv.browser.close().await;
}

#[tokio::test]
async fn images_are_cropped_for_graphics_clients_and_always_halfblocked() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr = testserver::serve_dir(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg {
        profile: Profile::Balanced,
        ..Default::default()
    })
    .await
    .unwrap();
    let url = format!("http://{addr}/image.html");

    // a Kitty-capable client gets the crop...
    let kitty = ClientCaps {
        graphics: GraphicsProto::Kitty,
        images: true,
        ..caps(true)
    };
    let mut h = srv.open_session(kitty);
    let mut screen = Screen::default();
    h.tx.send(ClientMsg::Navigate { url: url.clone() }).unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Above the image"))
        .await;
    // wait for an Image whose pixels are the real (red) ones, not a pre-decode screenshot
    let is_red = |i: &ImageMsg| {
        glyph_server::pixmap::Pixmap::decode_jpeg(&i.data).is_ok_and(|p| {
            let mid = (p.h / 2 * p.w + p.w / 2) * 3;
            p.rgb[mid] > 150 && p.rgb[mid + 1] < 90 && p.rgb[mid + 2] < 90
        })
    };
    for _ in 0..40 {
        if screen.images.iter().any(is_red) {
            break;
        }
        screen.settle(&mut h, 250).await;
    }
    let im = screen
        .images
        .iter()
        .rev()
        .find(|i| is_red(i))
        .unwrap_or_else(|| {
            panic!(
                "no red image crop arrived ({} messages)",
                screen.images.len()
            )
        })
        .clone();
    assert_eq!(
        screen
            .images
            .iter()
            .map(|i| i.id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        1,
        "exactly one image placement expected"
    );
    // 160×96 css px at (16,48) = 20×6 cells at (2,3)
    assert_eq!((im.rect.x, im.rect.y, im.rect.w, im.rect.h), (2, 3, 20, 6));
    assert_eq!((im.px_w, im.px_h), (160, 96));
    // ...and the grid shows the picture in the same cells regardless: a solid red image is a flat
    // red cell (a textured one would use quadrant blocks)
    let reddish = |c: &Cell| {
        let col = if c.g == " " { c.style.bg } else { c.style.fg };
        col.0 > 150 && col.1 < 90 && col.2 < 90
    };
    screen.until(&mut h, |g, _| reddish(g.cell(10, 5))).await;

    // a client with no graphics protocol gets no Image messages at all
    let plain = ClientCaps {
        graphics: GraphicsProto::None,
        images: false,
        ..caps(false)
    };
    let mut h2 = srv.open_session(plain);
    let mut screen2 = Screen::default();
    h2.tx
        .send(ClientMsg::Navigate { url: url.clone() })
        .unwrap();
    screen2
        .until(&mut h2, |g, _| g.dump_text().contains("Above the image"))
        .await;
    screen2.settle(&mut h2, 1500).await;
    assert!(screen2.images.is_empty());

    // after Redraw the client's cache is flushed, so the placement is sent again
    let before = screen.images.len();
    h.tx.send(ClientMsg::Redraw).unwrap();
    for _ in 0..40 {
        if screen.images.len() > before {
            break;
        }
        screen.settle(&mut h, 250).await;
    }
    assert!(
        screen.images.len() > before && screen.images.last().unwrap().id == im.id,
        "no resend after Redraw"
    );

    // navigating away clears the placement
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/basic.html"),
    })
    .unwrap();
    screen
        .until(&mut h, |g, _| g.dump_text().contains("Hello glyph"))
        .await;
    for _ in 0..40 {
        if screen.cleared.contains(&im.id) {
            break;
        }
        screen.settle(&mut h, 250).await;
    }
    assert!(
        screen.cleared.contains(&im.id),
        "image placement not cleared: {:?}",
        screen.cleared
    );
    srv.browser.close().await;
}

#[tokio::test]
async fn page_sees_the_clients_color_scheme() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr = testserver::serve_dir(fixtures()).await.unwrap();
    let srv = Server::start(ServerCfg::default()).await.unwrap();
    for (scheme, dark) in [(ColorScheme::Dark, true), (ColorScheme::Light, false)] {
        let mut h = srv.open_session(ClientCaps {
            scheme,
            ..caps(false)
        });
        let mut screen = Screen::default();
        h.tx.send(ClientMsg::Navigate {
            url: format!("http://{addr}/scheme.html"),
        })
        .unwrap();
        screen
            .until(&mut h, |g, _| g.dump_text().contains("scheme test"))
            .await;
        // let the first paint settle, then look at a blank cell of the page background
        tokio::time::sleep(Duration::from_millis(300)).await;
        screen.until(&mut h, |_, _| true).await;
        let g = screen.grid.as_ref().unwrap();
        let bg = g.cell(40, 10).style.bg;
        let is_dark = bg.0 < 60 && bg.1 < 60;
        assert_eq!(is_dark, dark, "{scheme:?}: page background {bg:?}");
    }
    srv.browser.close().await;
}
