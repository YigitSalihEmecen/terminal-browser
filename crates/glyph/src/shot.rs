//! `glyph shot`: drive a live session through scripted steps and write side-by-side PNGs of the
//! real page (ground truth) and what the terminal grid shows. This is the debugging tool for
//! rendering problems: it exercises the real pipeline (screencast, diffs, acks), not a one-shot render.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use glyph_proto::*;
use glyph_server::{
    gridview::{grid_to_html, wrap_page},
    testserver, Server, ServerCfg,
};
use serde_json::json;

pub struct Opts {
    pub serve: PathBuf,
    pub page: String,
    pub out: PathBuf,
    pub cols: u16,
    pub rows: u16,
    pub steps: String,
    pub profile: Profile,
    pub chrome: Option<PathBuf>,
    pub chrome_args: Vec<String>,
    pub dark: bool,
    pub faithful: bool,
}

#[derive(Default)]
struct View {
    grid: Option<Grid>,
    frames: u64,
    bytes_runs: u64,
    regions: usize,
    url: String,
    title: String,
    scroll: u16,
    mode: RenderMode,
}

pub async fn run(o: Opts) -> Result<()> {
    std::fs::create_dir_all(&o.out)?;
    let addr = testserver::serve_dir(o.serve.clone()).await?;
    let srv = Server::start(ServerCfg {
        profile: o.profile,
        chrome: o.chrome.clone(),
        chrome_args: o.chrome_args.clone(),
        ..Default::default()
    })
    .await?;
    let caps = ClientCaps {
        cols: o.cols,
        rows: o.rows,
        cell_px_w: 8,
        cell_px_h: 16,
        graphics: GraphicsProto::None,
        scheme: if o.dark {
            ColorScheme::Dark
        } else {
            ColorScheme::Light
        },
        page_style: if o.faithful {
            PageStyle::Faithful
        } else {
            PageStyle::Terminal
        },
        images: true,
    };
    let h = srv.open_session(caps);
    let (tx, mut rx) = (h.tx, h.rx);
    let view = Arc::new(Mutex::new(View::default()));
    let v2 = view.clone();
    let tx2 = tx.clone();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            let mut v = v2.lock().unwrap();
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
                    v.grid = Some(g);
                    v.frames += 1;
                    let _ = tx2.send(ClientMsg::Ack { tab, seq });
                }
                ServerMsg::Diff { tab, seq, runs, .. } => {
                    if let Some(g) = v.grid.as_mut() {
                        apply_runs(g, &runs);
                    }
                    v.bytes_runs += runs.len() as u64;
                    v.frames += 1;
                    let _ = tx2.send(ClientMsg::Ack { tab, seq });
                }
                ServerMsg::Regions { regions, .. } => v.regions = regions.len(),
                ServerMsg::Url { url, .. } => v.url = url,
                ServerMsg::Title { title, .. } => v.title = title,
                ServerMsg::Scroll { permille, .. } => v.scroll = permille,
                ServerMsg::Mode { mode, .. } => v.mode = mode,
                ServerMsg::Error(e) => eprintln!("[server] {e}"),
                _ => {}
            }
        }
    });

    let url = if o.page.contains("://") {
        o.page.clone()
    } else {
        format!("http://{addr}/{}", o.page)
    };
    tx.send(ClientMsg::Navigate { url })?;
    let mut n = 0;
    for step in o.steps.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (cmd, arg) = step.split_once(' ').unwrap_or((step, ""));
        let f = |s: &str| {
            s.trim()
                .parse::<f64>()
                .with_context(|| format!("bad number {s:?} in step {step:?}"))
        };
        match cmd {
            "wait" => tokio::time::sleep(Duration::from_secs_f64(f(arg)?)).await,
            "scroll" => {
                tx.send(ClientMsg::Scroll {
                    unit: ScrollUnit::Lines,
                    dx: 0,
                    dy: f(arg)? as i32,
                    col: o.cols / 2,
                    row: o.rows / 2,
                })?;
            }
            "page" => {
                tx.send(ClientMsg::Scroll {
                    unit: ScrollUnit::Pages,
                    dx: 0,
                    dy: f(arg)? as i32,
                    col: o.cols / 2,
                    row: o.rows / 2,
                })?;
            }
            "edge" => {
                tx.send(ClientMsg::Scroll {
                    unit: ScrollUnit::Edge,
                    dx: 0,
                    dy: if arg == "top" { -1 } else { 1 },
                    col: 0,
                    row: 0,
                })?;
            }
            "click" => {
                let (c, r) = arg.split_once(' ').context("click COL ROW")?;
                for kind in [
                    MouseKind::Down(MouseButton::Left),
                    MouseKind::Up(MouseButton::Left),
                ] {
                    tx.send(ClientMsg::Mouse(MouseEvent {
                        kind,
                        col: c.trim().parse()?,
                        row: r.trim().parse()?,
                        mods: Mods::default(),
                        clicks: 1,
                    }))?;
                }
            }
            "key" => {
                for ch in arg.chars() {
                    tx.send(ClientMsg::Key(KeyEvent {
                        code: KeyCode::Char(ch),
                        mods: Mods::default(),
                    }))?;
                }
            }
            "mode" => {
                tx.send(ClientMsg::SetMode(if arg == "text" {
                    RenderMode::Text
                } else {
                    RenderMode::Pixel
                }))?;
            }
            "navigate" => {
                tx.send(ClientMsg::Navigate {
                    url: format!("http://{addr}/{arg}"),
                })?;
            }
            "snap" => {
                n += 1;
                let name = if arg.is_empty() {
                    format!("{n:02}")
                } else {
                    arg.to_owned()
                };
                snap(&srv, &view, &o, &name).await?;
            }
            other => bail!("unknown step {other:?}"),
        }
    }
    srv.browser.close().await;
    Ok(())
}

async fn snap(srv: &Arc<Server>, view: &Arc<Mutex<View>>, o: &Opts, name: &str) -> Result<()> {
    // ground truth: a second CDP session on the page the glyph session is driving
    let cdp = srv.browser.cdp();
    let ctxs: serde_json::Value = cdp
        .call(None, "Target.getBrowserContexts", json!({}))
        .await?;
    let ours: Vec<String> = ctxs["browserContextIds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c.as_str().map(str::to_owned))
        .collect();
    let t: serde_json::Value = cdp.call(None, "Target.getTargets", json!({})).await?;
    let target = t["targetInfos"]
        .as_array()
        .into_iter()
        .flatten()
        .rfind(|t| {
            t["type"] == "page"
                && t["browserContextId"]
                    .as_str()
                    .is_some_and(|c| ours.iter().any(|o| o == c))
        })
        .and_then(|t| t["targetId"].as_str().map(str::to_owned))
        .context("no page target")?;
    let a: serde_json::Value = cdp
        .call(
            None,
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
        )
        .await?;
    let sess = cdp.session(a["sessionId"].as_str().context("session id")?);
    let shot: serde_json::Value = sess
        .call("Page.captureScreenshot", json!({ "format": "png" }))
        .await?;
    let truth = shot["data"].as_str().context("screenshot data")?.to_owned();
    let _ = cdp
        .call_raw(
            None,
            "Target.detachFromTarget",
            json!({ "sessionId": sess.id() }),
        )
        .await;

    let (grid, info) = {
        let v = view.lock().unwrap();
        let g = v.grid.clone().context("no frame received yet")?;
        (
            g,
            format!(
                "frames {} · regions {} · scroll {}‰ · {:?} · {}",
                v.frames, v.regions, v.scroll, v.mode, v.title
            ),
        )
    };
    let (pw, ph) = (o.cols as u32 * 8, o.rows as u32 * 16);
    let body = format!(
        "<div class=panel><h4>real page</h4><img style=\"display:block;width:{pw}px;height:{ph}px\" src=\"data:image/png;base64,{truth}\"></div>\
         <div class=panel><h4>terminal grid &nbsp; <span style=\"font-weight:400;color:#9a9\">{info}</span></h4>{}</div>",
        grid_to_html(&grid, 8, 16)
    );
    let html = wrap_page(&body, 8, 16);
    let tmp = tempfile::Builder::new().suffix(".html").tempfile()?;
    std::fs::write(tmp.path(), html)?;
    let (target_id, s) = srv.browser.new_target(None).await?;
    s.send(
        "Emulation.setDeviceMetricsOverride",
        json!({ "width": pw * 2 + 40, "height": ph + 40, "deviceScaleFactor": 1, "mobile": false }),
    )
    .await?;
    s.send("Page.enable", json!({})).await?;
    let mut ev = s.events();
    s.send(
        "Page.navigate",
        json!({ "url": format!("file://{}", tmp.path().display()) }),
    )
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(e) = ev.recv().await {
            if e.method == "Page.loadEventFired" {
                break;
            }
        }
    })
    .await;
    let png: serde_json::Value = s
        .call("Page.captureScreenshot", json!({ "format": "png" }))
        .await?;
    let bytes =
        base64::engine::general_purpose::STANDARD.decode(png["data"].as_str().context("png")?)?;
    let path = o.out.join(format!("{name}.png"));
    std::fs::write(&path, bytes)?;
    srv.browser.close_target(&target_id).await.ok();
    println!("{}", path.display());
    Ok(())
}
