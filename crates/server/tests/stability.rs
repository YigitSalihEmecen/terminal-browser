//! Frame-consistency under rapid scrolling, measured rather than eyeballed.
//!
//! `tear.html` has 600 lines whose background colour (red/blue band) is a function of the line
//! number printed on it. Text comes from the DOM snapshot and colours from screencast pixels, so if
//! a frame mixes a stale screenshot with a fresh snapshot, some row's colour will not match its
//! number. Every frame the client receives is checked.

use std::{path::PathBuf, time::Duration};

use glyph_proto::*;
use glyph_server::{browser::find_chrome, testserver, Server, ServerCfg};
use tokio::time::timeout;

fn caps() -> ClientCaps {
    ClientCaps {
        cols: 100,
        rows: 30,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        scheme: Default::default(),
        page_style: Default::default(),
        images: false,
    }
}

/// `(rows checked, rows wrong)` for one frame.
fn check(g: &Grid) -> (u32, u32) {
    let (mut checked, mut wrong) = (0, 0);
    for y in 0..g.rows() {
        let row: String = (0..14).map(|x| g.cell(x, y).g.to_string()).collect();
        let start = row.len() - row.trim_start().len();
        let t = row.trim_start();
        let Some(n) = t.get(..3).and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if !t[3..].starts_with(' ') {
            continue;
        }
        let want_red = n % 8 < 4;
        let bg = g.cell(start as u16, y).style.bg;
        let is_red = bg.0 as i32 > bg.2 as i32 + 20;
        let is_blue = bg.2 as i32 > bg.0 as i32 + 20;
        checked += 1;
        if (want_red && !is_red) || (!want_red && !is_blue) {
            wrong += 1;
        }
    }
    (checked, wrong)
}

async fn run(profile: Profile) -> (u32, u32, u32) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    let addr = testserver::serve_dir(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/gallery"),
    )
    .await
    .unwrap();
    let srv = Server::start(ServerCfg {
        profile,
        ..Default::default()
    })
    .await
    .unwrap();
    let mut h = srv.open_session(caps());
    h.tx.send(ClientMsg::Navigate {
        url: format!("http://{addr}/tear.html"),
    })
    .unwrap();
    let mut grid: Option<Grid> = None;
    let (mut frames, mut checked, mut wrong) = (0u32, 0u32, 0u32);
    let apply = |m: ServerMsg,
                     grid: &mut Option<Grid>,
                     tx: &tokio::sync::mpsc::UnboundedSender<ClientMsg>| match m
    {
        ServerMsg::FullFrame {
            tab,
            seq,
            cols,
            rows,
            runs,
        } => {
            let mut g = Grid::new(cols, rows, Style::default());
            apply_runs(&mut g, &runs);
            *grid = Some(g);
            let _ = tx.send(ClientMsg::Ack { tab, seq });
            true
        }
        ServerMsg::Diff { tab, seq, runs, .. } => {
            apply_runs(grid.as_mut().expect("diff before frame"), &runs);
            let _ = tx.send(ClientMsg::Ack { tab, seq });
            true
        }
        _ => false,
    };
    // wait for the page
    let ready = timeout(Duration::from_secs(20), async {
        loop {
            let m = h.rx.recv().await.expect("closed");
            if apply(m, &mut grid, &h.tx)
                && grid
                    .as_ref()
                    .is_some_and(|g| g.dump_text().contains("line number 1"))
            {
                return;
            }
        }
    })
    .await;
    assert!(ready.is_ok(), "page never rendered");
    tokio::time::sleep(Duration::from_millis(400)).await;

    // rapid scrolling: bursts of line scrolls at 15–60 ms spacing, page jumps, direction changes
    let mut rng = 0x2545_f491u32;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng
    };
    let t_end = tokio::time::Instant::now() + Duration::from_secs(8);
    let mut last_send = tokio::time::Instant::now();
    let mut down = true;
    while tokio::time::Instant::now() < t_end {
        // send scroll commands on a jittery schedule while draining and checking frames
        if last_send.elapsed() > Duration::from_millis(15 + (next() % 45) as u64) {
            last_send = tokio::time::Instant::now();
            if next() % 40 == 0 {
                down = !down;
            }
            let msg = match next() % 12 {
                0 => ClientMsg::Scroll {
                    unit: ScrollUnit::Pages,
                    dx: 0,
                    dy: if down { 1 } else { -1 },
                    col: 50,
                    row: 15,
                },
                _ => ClientMsg::Scroll {
                    unit: ScrollUnit::Lines,
                    dx: 0,
                    dy: if down { 3 } else { -3 },
                    col: 50,
                    row: 15,
                },
            };
            let _ = h.tx.send(msg);
        }
        if let Ok(Some(m)) = timeout(Duration::from_millis(5), h.rx.recv()).await {
            if apply(m, &mut grid, &h.tx) {
                frames += 1;
                let (c, w) = check(grid.as_ref().unwrap());
                checked += c;
                wrong += w;
            }
        }
    }
    let m = srv.metrics.snapshot();
    eprintln!(
        "  server: {} refreshes, {:.0} ms average each",
        m.refreshes, m.avg_refresh_ms
    );
    srv.browser.close().await;
    (frames, checked, wrong)
}

#[tokio::test]
async fn rapid_scrolling_never_shows_a_row_with_the_wrong_colour() {
    if find_chrome(None).is_err() {
        eprintln!("SKIP: no Chromium available");
        return;
    }
    for profile in [Profile::Balanced, Profile::Lean] {
        let (frames, checked, wrong) = run(profile).await;
        eprintln!("{profile:?}: {frames} frames, {checked} numbered rows checked, {wrong} wrong");
        assert!(
            frames >= 10,
            "{profile:?}: only {frames} frames arrived while scrolling"
        );
        assert!(
            checked > 200,
            "{profile:?}: the check did not see enough rows ({checked})"
        );
        assert_eq!(wrong, 0, "{profile:?}: {wrong} of {checked} rows had a colour that does not match their line number (stale pixels mixed with fresh text)");
    }
}
