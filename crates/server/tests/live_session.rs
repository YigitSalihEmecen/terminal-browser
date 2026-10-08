//! End-to-end through real Chromium: session → tab task → outbox → protocol messages.
//! Skipped (with a message) when no Chromium is installed.

use std::{path::PathBuf, time::Duration};

use glyph_proto::*;
use glyph_server::{browser::find_chrome, testserver, Server, ServerCfg, SessionHandle};
use tokio::time::timeout;

struct Rig {
    h: SessionHandle,
    grid: Option<Grid>,
    seq: u64,
    tab: TabId,
    regions: Vec<Region>,
    tabs: Vec<TabInfo>,
    active: TabId,
    cursor: Option<CursorState>,
    title: String,
}

impl Rig {
    fn new(h: SessionHandle) -> Self {
        Self {
            h,
            grid: None,
            seq: 0,
            tab: 0,
            regions: vec![],
            tabs: vec![],
            active: 0,
            cursor: None,
            title: String::new(),
        }
    }

    fn apply(&mut self, m: ServerMsg) {
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
                self.seq = seq;
                self.tab = tab;
                let _ = self.h.tx.send(ClientMsg::Ack { tab, seq });
            }
            ServerMsg::Diff {
                tab,
                seq,
                base,
                runs,
            } => {
                assert_eq!(base, self.seq, "diff base mismatch");
                apply_runs(self.grid.as_mut().expect("diff before full frame"), &runs);
                self.seq = seq;
                let _ = self.h.tx.send(ClientMsg::Ack { tab, seq });
            }
            ServerMsg::Regions { regions, .. } => self.regions = regions,
            ServerMsg::Tabs { active, tabs } => {
                self.active = active;
                self.tabs = tabs;
            }
            ServerMsg::Cursor { pos, .. } => self.cursor = pos,
            ServerMsg::Title { title, .. } => self.title = title,
            ServerMsg::Error(e) => panic!("server error: {e}"),
            _ => {}
        }
    }

    /// Pump messages until `pred` holds for the grid text (or time out with a dump).
    async fn until(&mut self, what: &str, pred: impl Fn(&Self) -> bool) {
        let r = timeout(Duration::from_secs(15), async {
            loop {
                // like the real client: apply everything already queued before looking at the screen
                while let Ok(m) = self.h.rx.try_recv() {
                    self.apply(m);
                }
                if pred(self) {
                    return;
                }
                match self.h.rx.recv().await {
                    Some(m) => self.apply(m),
                    None => panic!("session closed"),
                }
            }
        })
        .await;
        if r.is_err() {
            let text = self
                .grid
                .as_ref()
                .map(|g| g.dump_text())
                .unwrap_or_default();
            panic!(
                "timed out waiting for {what}; screen:\n{text}\ntabs: {:?}",
                self.tabs
            );
        }
    }

    fn text(&self) -> String {
        self.grid
            .as_ref()
            .map(|g| g.dump_text())
            .unwrap_or_default()
    }

    fn click(&self, col: u16, row: u16) {
        for kind in [
            MouseKind::Down(MouseButton::Left),
            MouseKind::Up(MouseButton::Left),
        ] {
            let _ = self.h.tx.send(ClientMsg::Mouse(MouseEvent {
                kind,
                col,
                row,
                mods: Mods::default(),
                clicks: 1,
            }));
        }
    }

    fn region(&self, label: &str) -> Region {
        self.regions
            .iter()
            .find(|r| r.label.contains(label))
            .unwrap_or_else(|| panic!("no region {label}: {:?}", self.regions))
            .clone()
    }
}

fn caps() -> ClientCaps {
    ClientCaps {
        cols: 80,
        rows: 24,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        images: false,
    }
}

#[tokio::test]
async fn browse_click_type_scroll_tabs() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr =
        testserver::serve_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"))
            .await
            .unwrap();
    let srv = Server::start(ServerCfg {
        profile: Profile::Balanced,
        ..Default::default()
    })
    .await
    .unwrap();
    let mut rig = Rig::new(srv.open_session(caps()));

    // initial blank tab arrives
    rig.until("hello + first frame", |r| r.grid.is_some()).await;

    // navigate
    rig.h
        .tx
        .send(ClientMsg::Navigate {
            url: format!("http://{addr}/interact.html"),
        })
        .unwrap();
    rig.until("page text", |r| {
        r.text().contains("go to basic") && r.text().contains("idle")
    })
    .await;
    assert_eq!(rig.title, "Interact");

    // click the button (found through the region table, like link hints)
    let b = rig.region("press");
    assert_eq!(b.kind, RegionKind::Button);
    rig.click(b.rects[0].x + 1, b.rects[0].y);
    rig.until("button click effect", |r| r.text().contains("clicked 1"))
        .await;

    // focus the input by clicking it, type, and see the caret + page reaction
    let input = rig
        .regions
        .iter()
        .find(|r| r.kind == RegionKind::Input)
        .expect("input region")
        .clone();
    rig.click(input.rects[0].x + 2, input.rects[0].y);
    rig.until("caret after focus", |r| r.cursor.is_some()).await;
    for c in "hi!".chars() {
        rig.h
            .tx
            .send(ClientMsg::Key(KeyEvent {
                code: KeyCode::Char(c),
                mods: Mods::default(),
            }))
            .unwrap();
    }
    rig.until("typed text reaches the page and the screen", |r| {
        r.text().contains("typed:hi!") && r.text().contains("[hi!")
    })
    .await;
    let cur = rig.cursor.unwrap();
    assert_eq!(cur.row, input.rects[0].y, "caret on the input's row");
    assert!(cur.col > input.rects[0].x, "caret inside the field");

    // scroll to the bottom and back
    rig.h
        .tx
        .send(ClientMsg::Scroll {
            unit: ScrollUnit::Edge,
            dx: 0,
            dy: 1,
            col: 0,
            row: 0,
        })
        .unwrap();
    rig.until("bottom of page", |r| r.text().contains("THE BOTTOM"))
        .await;
    rig.h
        .tx
        .send(ClientMsg::Scroll {
            unit: ScrollUnit::Edge,
            dx: 0,
            dy: -1,
            col: 0,
            row: 0,
        })
        .unwrap();
    rig.until("back at top", |r| r.text().contains("go to basic"))
        .await;

    // follow a link
    let link = rig.region("go to basic");
    assert_eq!(
        link.href.as_deref(),
        Some(&*format!("http://{addr}/basic.html"))
    );
    rig.click(link.rects[0].x + 1, link.rects[0].y);
    rig.until("navigated", |r| r.text().contains("Hello glyph"))
        .await;

    // back
    rig.h.tx.send(ClientMsg::Back).unwrap();
    rig.until("back works", |r| r.text().contains("go to basic"))
        .await;

    // new tab, then switch back
    rig.h
        .tx
        .send(ClientMsg::NewTab {
            url: Some(format!("http://{addr}/unicode.html")),
        })
        .unwrap();
    rig.until("second tab shown", |r| {
        r.tabs.len() == 2 && r.text().contains("日本語")
    })
    .await;
    let (first, second) = (rig.tabs[0].id, rig.tabs[1].id);
    assert_eq!(rig.active, second);
    rig.h.tx.send(ClientMsg::SwitchTab(first)).unwrap();
    rig.until("first tab again", |r| {
        r.active == first && r.text().contains("go to basic")
    })
    .await;
    rig.h.tx.send(ClientMsg::CloseTab(second)).unwrap();
    rig.until("tab closed", |r| r.tabs.len() == 1).await;

    // disallowed scheme is refused (server answers with Error; this rig panics on Error, so check separately)
    assert!(!srv.is_allowed_url("file:///etc/passwd"));
    assert!(!srv.is_allowed_url("javascript:alert(1)"));
    assert!(srv.is_allowed_url("https://example.org/"));
}

#[tokio::test]
async fn idle_page_sends_nothing() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    let addr =
        testserver::serve_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures"))
            .await
            .unwrap();
    let srv = Server::start(ServerCfg::default()).await.unwrap();
    let mut rig = Rig::new(srv.open_session(caps()));
    rig.h
        .tx
        .send(ClientMsg::Navigate {
            url: format!("http://{addr}/basic.html"),
        })
        .unwrap();
    rig.until("loaded", |r| r.text().contains("Hello glyph"))
        .await;
    // let it settle, then count messages over 2 s of idleness
    tokio::time::sleep(Duration::from_millis(800)).await;
    while let Ok(Some(m)) = timeout(Duration::from_millis(50), rig.h.rx.recv()).await {
        rig.apply(m);
    }
    let mut n = 0;
    let end = tokio::time::Instant::now() + Duration::from_secs(2);
    while let Ok(Some(m)) = tokio::time::timeout_at(end, rig.h.rx.recv()).await {
        if matches!(m, ServerMsg::FullFrame { .. } | ServerMsg::Diff { .. }) {
            n += 1;
        }
        rig.apply(m);
    }
    assert_eq!(n, 0, "idle page produced {n} frame messages");
}
