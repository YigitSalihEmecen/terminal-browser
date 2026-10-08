//! `glyph bench`: server memory, CPU and bytes streamed per minute on fixed sample pages.
//!
//! The harness is a real client of a real server: pages are served over loopback HTTP, frames
//! travel over a loopback WebSocket through the production codec (so byte counts are wire bytes),
//! and the client acks like the TUI does. Memory and CPU come from the OS process table for the
//! whole Chromium tree (plus this process, which hosts the server logic).

use std::{
    path::PathBuf,
    sync::{atomic::Ordering::Relaxed, Arc},
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use glyph_client::remote::{connect, RemoteOpts};
use glyph_proto::{ClientCaps, ClientMsg, GraphicsProto, Profile, ScrollUnit, ServerMsg};
use glyph_server::{
    metrics::{sample_process, sample_tree, ProcSample},
    net::{self, NetCfg},
    testserver, Server, ServerCfg,
};
use serde::Serialize;

const PAGES: &[(&str, &str)] = &[
    ("static", include_str!("../../../bench/pages/static.html")),
    ("dense", include_str!("../../../bench/pages/dense.html")),
    ("ticker", include_str!("../../../bench/pages/ticker.html")),
    ("images", include_str!("../../../bench/pages/images.html")),
];
const PIXEL_PNG: &[u8] = include_bytes!("../../../bench/pages/pixel.png");

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    pub profile: String,
    pub page: String,
    pub scenario: String,
    pub seconds: f64,
    /// Shared-aware memory of Chromium's process tree (macOS footprint / Linux PSS), MB.
    pub chromium_mem_mb: Option<f64>,
    /// Naive sum of resident sizes (double-counts shared pages), MB.
    pub chromium_rss_sum_mb: f64,
    pub chromium_procs: u32,
    /// glyph's own process (server logic + this harness's trivial client), MB.
    pub glyph_mem_mb: Option<f64>,
    /// CPU of the Chromium tree + glyph, as a share of one core over the window.
    pub cpu_pct: f64,
    /// …of which glyph's own process.
    pub glyph_cpu_pct: f64,
    pub wire_kb_per_min: f64,
    pub frames_per_min: f64,
    pub avg_refresh_ms: f64,
}

pub struct Opts {
    pub profiles: Vec<Profile>,
    pub pages: Vec<String>,
    pub seconds: u64,
    pub warmup: u64,
    pub chrome: Option<PathBuf>,
    pub chrome_args: Vec<String>,
    pub cpu_throttle: Option<f64>,
    pub scenarios: Vec<String>,
    pub cols: u16,
    pub rows: u16,
}

fn profile_name(p: Profile) -> &'static str {
    match p {
        Profile::Lean => "lean",
        Profile::Balanced => "balanced",
        Profile::Full => "full",
    }
}

fn mb(kb: u64) -> f64 {
    kb as f64 / 1024.0
}

pub async fn run(o: Opts) -> Result<Vec<Row>> {
    let dir = tempfile::tempdir()?;
    for (name, html) in PAGES {
        std::fs::write(dir.path().join(format!("{name}.html")), html)?;
    }
    std::fs::write(dir.path().join("pixel.png"), PIXEL_PNG)?;
    let addr = testserver::serve_dir(dir.path().to_path_buf()).await?;

    let mut rows = Vec::new();
    for &profile in &o.profiles {
        for page in &o.pages {
            if !PAGES.iter().any(|(n, _)| n == page) {
                bail!(
                    "unknown page {page:?}; choose from {:?}",
                    PAGES.iter().map(|(n, _)| *n).collect::<Vec<_>>()
                );
            }
            eprintln!("[bench] {} / {page}", profile_name(profile));
            rows.extend(
                one(&o, profile, page, addr)
                    .await
                    .with_context(|| format!("{} / {page}", profile_name(profile)))?,
            );
        }
    }
    Ok(rows)
}

async fn one(
    o: &Opts,
    profile: Profile,
    page: &str,
    http: std::net::SocketAddr,
) -> Result<Vec<Row>> {
    let srv = Server::start(ServerCfg {
        profile,
        chrome: o.chrome.clone(),
        chrome_args: o.chrome_args.clone(),
        cpu_throttle: o.cpu_throttle,
        ..Default::default()
    })
    .await?;
    let mut ncfg = NetCfg::new("127.0.0.1:0".parse()?);
    ncfg.metrics = srv.metrics.clone();
    let handle = net::serve(ncfg, Arc::new(srv.clone())).await?;

    let caps = ClientCaps {
        cols: o.cols,
        rows: o.rows,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        scheme: Default::default(),
        page_style: Default::default(),
        images: false,
    };
    let conn = connect(
        &RemoteOpts {
            url: format!("ws://{}", handle.addr),
            token: None,
            fingerprint: None,
        },
        caps,
    )
    .await?;
    let (tx, mut rx) = (conn.tx, conn.rx);

    // A client that acks promptly and notes when the page is up.
    let ready = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let ready2 = ready.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        let mut grid: Option<glyph_proto::Grid> = None;
        while let Some(m) = rx.recv().await {
            match m {
                ServerMsg::FullFrame {
                    tab,
                    seq,
                    cols,
                    rows,
                    runs,
                } => {
                    let mut g = glyph_proto::Grid::new(cols, rows, glyph_proto::Style::default());
                    glyph_proto::apply_runs(&mut g, &runs);
                    if g.dump_text().contains("BENCH-READY") {
                        ready2.store(true, Relaxed);
                    }
                    grid = Some(g);
                    let _ = tx2.send(ClientMsg::Ack { tab, seq });
                }
                ServerMsg::Diff { tab, seq, runs, .. } => {
                    if let Some(g) = grid.as_mut() {
                        glyph_proto::apply_runs(g, &runs);
                        if !ready2.load(Relaxed) && g.dump_text().contains("BENCH-READY") {
                            ready2.store(true, Relaxed);
                        }
                    }
                    let _ = tx2.send(ClientMsg::Ack { tab, seq });
                }
                _ => {}
            }
        }
    });

    tx.send(ClientMsg::Navigate {
        url: format!("http://{http}/{page}.html"),
    })?;
    let t0 = Instant::now();
    while !ready.load(Relaxed) {
        if t0.elapsed() > Duration::from_secs(30) {
            bail!("page never rendered");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    tokio::time::sleep(Duration::from_secs(o.warmup)).await;

    let chrome_pid = srv.browser.pid().context("no browser pid")?;
    let mut rows = Vec::new();
    for scenario in &o.scenarios {
        let before = (
            srv.metrics.snapshot(),
            sample_tree(chrome_pid),
            sample_process(std::process::id()),
            Instant::now(),
        );
        let end = before.3 + Duration::from_secs(o.seconds);
        let mut flip = 0;
        while Instant::now() < end {
            flip += 1;
            match scenario.as_str() {
                "idle" => {}
                // up and down over the same ~24 lines: cheap, but a streaming compressor has seen
                // every row before, so wire numbers from this are a lower bound
                "scroll" => {
                    let dy = if (flip / 6) % 2 == 0 { 4 } else { -4 };
                    let _ = tx.send(ClientMsg::Scroll {
                        unit: ScrollUnit::Lines,
                        dx: 0,
                        dy,
                        col: o.cols / 2,
                        row: o.rows / 2,
                    });
                }
                // steady reading: always fresh content below (back to the top after a while)
                "read" => {
                    let msg = if flip % 120 == 0 {
                        ClientMsg::Scroll {
                            unit: ScrollUnit::Edge,
                            dx: 0,
                            dy: -1,
                            col: 0,
                            row: 0,
                        }
                    } else {
                        ClientMsg::Scroll {
                            unit: ScrollUnit::Lines,
                            dx: 0,
                            dy: 4,
                            col: o.cols / 2,
                            row: o.rows / 2,
                        }
                    };
                    let _ = tx.send(msg);
                }
                other => bail!("unknown scenario {other:?} (idle, scroll, read)"),
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        let secs = before.3.elapsed().as_secs_f64();
        let after = (
            srv.metrics.snapshot(),
            sample_tree(chrome_pid),
            sample_process(std::process::id()),
        );
        let cpu = |a: &Option<ProcSample>, b: &Option<ProcSample>| match (a, b) {
            (Some(a), Some(b)) => (b.cpu_secs - a.cpu_secs).max(0.0),
            _ => 0.0,
        };
        let m0 = before.0;
        let m1 = after.0;
        let ch = after.1.unwrap_or_default();
        rows.push(Row {
            profile: profile_name(profile).into(),
            page: page.into(),
            scenario: scenario.clone(),
            seconds: secs,
            chromium_mem_mb: ch.mem_kb.map(mb),
            chromium_rss_sum_mb: mb(ch.rss_kb),
            chromium_procs: ch.procs,
            glyph_mem_mb: after.2.and_then(|s| s.mem_kb).map(mb),
            cpu_pct: (cpu(&before.1, &after.1) + cpu(&before.2, &after.2)) / secs * 100.0,
            glyph_cpu_pct: cpu(&before.2, &after.2) / secs * 100.0,
            wire_kb_per_min: (m1.bytes_out - m0.bytes_out) as f64 / 1024.0 / secs * 60.0,
            frames_per_min: ((m1.frames_full + m1.frames_diff) - (m0.frames_full + m0.frames_diff))
                as f64
                / secs
                * 60.0,
            avg_refresh_ms: m1.avg_refresh_ms,
        });
    }
    handle.abort();
    srv.browser.close().await;
    Ok(rows)
}

pub fn markdown(rows: &[Row]) -> String {
    let mut s = String::from("| profile | page | scenario | Chromium mem MB | (rss sum) | procs | glyph MB | CPU % | (glyph) | wire KB/min | frames/min | refresh ms |\n|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n");
    for r in rows {
        s.push_str(&format!(
            "| {} | {} | {} | {} | {:.0} | {} | {} | {:.1} | {:.1} | {:.1} | {:.0} | {:.1} |\n",
            r.profile,
            r.page,
            r.scenario,
            r.chromium_mem_mb
                .map_or("n/a".into(), |m| format!("{m:.0}")),
            r.chromium_rss_sum_mb,
            r.chromium_procs,
            r.glyph_mem_mb.map_or("n/a".into(), |m| format!("{m:.0}")),
            r.cpu_pct,
            r.glyph_cpu_pct,
            r.wire_kb_per_min,
            r.frames_per_min,
            r.avg_refresh_ms,
        ));
    }
    s
}
