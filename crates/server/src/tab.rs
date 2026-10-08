//! One browser tab: a task that owns a CDP session, keeps the page's viewport in sync with the
//! client, turns paint activity into rendered frames, and executes input.

use std::time::{Duration, Instant};

use anyhow::Result;
use glyph_proto::{
    width::str_width, ClientCaps, CursorState, KeyEvent, LoadState, MouseButton, MouseEvent,
    MouseKind, RenderMode, ScrollUnit, TabId,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{
    capture,
    cdp::{Event, Session},
    images::{crop_images, Crop},
    keys::{cdp_mods, key_events},
    pixmap::Pixmap,
    profile::{ProfileCfg, Verdict},
    render::{render, Metrics, PageInfo, Rendered},
    snapshot::SnapshotResult,
    textmode::{self, AxTree, TextDoc},
};

const AGENT_JS: &str = include_str!("inject.js");
const DEBOUNCE: Duration = Duration::from_millis(25);
const MAX_STALE: Duration = Duration::from_millis(250);

#[derive(Debug)]
pub enum TabCmd {
    Navigate(String),
    Back,
    Forward,
    Reload,
    Stop,
    Key(KeyEvent),
    Mouse(MouseEvent),
    Scroll {
        unit: ScrollUnit,
        dx: i32,
        dy: i32,
        col: u16,
        row: u16,
    },
    Paste(String),
    Find {
        query: String,
        forward: bool,
        case_sensitive: bool,
    },
    ClearFocus,
    Resize {
        cols: u16,
        rows: u16,
    },
    SetMode(RenderMode),
    SetFps(f32),
    /// Foreground (true) or background (false).
    Active(bool),
    /// Ask for a fresh frame even if nothing changed (client lost sync, resize, …).
    Refresh,
    Close,
}

pub enum TabEvent {
    Frame(Box<Rendered>),
    Title(String),
    Url(String),
    Load(LoadState),
    Cursor(Option<CursorState>),
    Clipboard(String),
    FindResult(u32),
    Scroll(u16),
    Mode(RenderMode),
    Image(glyph_proto::ImageMsg),
    ImageClear(Vec<u32>),
    Crashed,
}

#[derive(Clone)]
pub struct TabCfg {
    pub metrics: std::sync::Arc<crate::metrics::Metrics>,
    pub profile: ProfileCfg,
    pub caps: ClientCaps,
    pub cw: f64,
    pub ch: f64,
}

#[derive(Clone)]
pub struct TabHandle {
    pub id: TabId,
    pub tx: UnboundedSender<TabCmd>,
}

impl TabHandle {
    pub fn send(&self, c: TabCmd) {
        let _ = self.tx.send(c);
    }
}

struct FrameData {
    jpeg_b64: String,
    /// Page scroll offset when the frame was produced (screencast metadata).
    scroll: Option<(f64, f64)>,
}

struct Tab {
    id: TabId,
    sess: Session,
    cfg: TabCfg,
    m: Metrics,
    mode: RenderMode,
    active: bool,
    cmds: UnboundedReceiver<TabCmd>,
    events: UnboundedReceiver<Event>,
    out: UnboundedSender<(TabId, TabEvent)>,
    // paint pipeline
    frame: Option<FrameData>,
    pending_ack: Option<i64>,
    dirty_since: Option<Instant>,
    last_event: Instant,
    last_refresh: Instant,
    fps: f32,
    // page state
    url: String,
    title: String,
    load: LoadState,
    cursor: Option<CursorState>,
    mouse: (f64, f64),
    scroll: u16,
    // text (reader) mode
    text_doc: Option<TextDoc>,
    text_stale: bool,
    text_scroll: usize,
    text_focus: Option<usize>,
    find_hl: Option<String>,
    find_hits: Vec<usize>,
    // terminal-graphics images already delivered: id -> pixel hash
    images_sent: std::collections::HashMap<u32, u64>,
    pending_crops: Option<Vec<Crop>>,
}

/// Attach to `sess` and run the tab until `Close`.
pub fn spawn(
    id: TabId,
    sess: Session,
    cfg: TabCfg,
    out: UnboundedSender<(TabId, TabEvent)>,
    start_url: Option<String>,
) -> TabHandle {
    let (tx, cmds) = tokio::sync::mpsc::unbounded_channel();
    // Paused requests are answered by their own task. If the tab loop did it, `Page.navigate`
    // (which cannot return until its Document request is resolved) would deadlock the loop that
    // is waiting for it.
    let events = {
        let mut raw = sess.events();
        let (fwd_tx, fwd_rx) = tokio::sync::mpsc::unbounded_channel();
        let (psess, profile, wants_images) = (sess.clone(), cfg.profile.clone(), cfg.caps.images);
        tokio::spawn(async move {
            while let Some(e) = raw.recv().await {
                if e.method == "Fetch.requestPaused" {
                    answer_paused(&psess, &profile, wants_images, &e).await;
                } else if fwd_tx.send(e).is_err() {
                    break;
                }
            }
        });
        fwd_rx
    };
    let m = Metrics {
        cols: cfg.caps.cols.max(1),
        rows: cfg.caps.rows.max(1),
        cw: cfg.cw,
        ch: cfg.ch,
    };
    let fps = cfg.profile.max_fps;
    let tab = Tab {
        id,
        sess,
        cfg,
        m,
        mode: RenderMode::Pixel,
        active: true,
        cmds,
        events,
        out,
        frame: None,
        pending_ack: None,
        dirty_since: None,
        last_event: Instant::now(),
        last_refresh: Instant::now() - Duration::from_secs(10),
        fps,
        url: String::new(),
        title: String::new(),
        load: LoadState::default(),
        cursor: None,
        mouse: (0.0, 0.0),
        scroll: 0,
        text_doc: None,
        text_stale: true,
        text_scroll: 0,
        text_focus: None,
        find_hl: None,
        find_hits: Vec::new(),
        images_sent: std::collections::HashMap::new(),
        pending_crops: None,
    };
    tokio::spawn(async move {
        if let Err(e) = tab.run(start_url).await {
            tracing::warn!("tab {id} ended: {e:#}");
        }
    });
    TabHandle { id, tx }
}

async fn answer_paused(sess: &Session, profile: &ProfileCfg, wants_images: bool, e: &Event) {
    #[derive(Deserialize)]
    struct P {
        #[serde(rename = "requestId")]
        id: String,
        #[serde(rename = "resourceType")]
        ty: String,
        request: Req,
    }
    #[derive(Deserialize)]
    struct Req {
        url: String,
    }
    let Ok(p) = e.parse::<P>() else { return };
    let _ = match profile.verdict(&p.ty, &p.request.url, wants_images) {
        Verdict::Continue => {
            sess.send("Fetch.continueRequest", json!({ "requestId": p.id }))
                .await
        }
        Verdict::Fail => {
            sess.send(
                "Fetch.failRequest",
                json!({ "requestId": p.id, "errorReason": "BlockedByClient" }),
            )
            .await
        }
    };
}

#[derive(Deserialize, Default)]
struct FrameMeta {
    #[serde(default, rename = "scrollOffsetX")]
    sx: f64,
    #[serde(default, rename = "scrollOffsetY")]
    sy: f64,
}

#[derive(Deserialize)]
struct Frame {
    data: String,
    #[serde(default)]
    metadata: FrameMeta,
    #[serde(rename = "sessionId")]
    session_id: i64,
}

#[derive(Deserialize)]
struct AgentMsg {
    t: String,
    #[serde(default)]
    k: String,
    #[serde(default)]
    x: f64,
    #[serde(default)]
    y: f64,
    #[serde(default)]
    h: f64,
    #[serde(default)]
    pre: String,
    #[serde(default)]
    pw: bool,
    #[serde(default)]
    text: String,
}

impl Tab {
    async fn setup(&mut self) -> Result<()> {
        let s = &self.sess;
        s.send("Page.enable", json!({})).await?;
        s.send("Runtime.enable", json!({})).await?;
        s.send("Page.setLifecycleEventsEnabled", json!({ "enabled": true }))
            .await?;
        s.send("Runtime.addBinding", json!({ "name": "__glyph" }))
            .await?;
        s.send(
            "Page.addScriptToEvaluateOnNewDocument",
            json!({ "source": AGENT_JS }),
        )
        .await?;
        s.send(
            "Emulation.setEmulatedMedia",
            json!({ "features": [
                { "name": "prefers-reduced-motion", "value": "reduce" },
                { "name": "prefers-color-scheme", "value": if self.cfg.caps.scheme == glyph_proto::ColorScheme::Dark { "dark" } else { "light" } },
            ] }),
        )
        .await?;
        capture::set_viewport(s, &self.m).await?;
        self.cfg.profile.apply(s, self.cfg.caps.images).await?;
        self.start_screencast().await
    }

    async fn start_screencast(&self) -> Result<()> {
        self.sess
            .send(
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": self.cfg.profile.jpeg_quality,
                    "maxWidth": self.m.px_w() as u32,
                    "maxHeight": self.m.px_h() as u32,
                    "everyNthFrame": 1,
                }),
            )
            .await
    }

    async fn run(mut self, start_url: Option<String>) -> Result<()> {
        self.setup().await?;
        if let Some(u) = start_url {
            self.navigate(&u).await;
        }
        self.mark_dirty();
        loop {
            let wait = self.next_refresh_in();
            tokio::select! {
                biased;
                cmd = self.cmds.recv() => match cmd {
                    None | Some(TabCmd::Close) => break,
                    Some(c) => self.on_cmd(c).await,
                },
                ev = self.events.recv() => match ev {
                    None => break,
                    Some(e) => self.on_event(e).await,
                },
                _ = tokio::time::sleep(wait.unwrap_or(Duration::from_secs(3600))), if wait.is_some() => {
                    if let Err(e) = self.refresh().await {
                        tracing::debug!("refresh failed: {e:#}");
                        self.dirty_since = None;
                    }
                }
            }
        }
        let _ = self.sess.send("Page.stopScreencast", json!({})).await;
        Ok(())
    }

    // ------------------------------------------------------------ scheduling

    fn mark_dirty(&mut self) {
        let now = Instant::now();
        self.last_event = now;
        self.dirty_since.get_or_insert(now);
    }

    fn next_refresh_in(&self) -> Option<Duration> {
        if !self.active {
            return None;
        }
        let since = self.dirty_since?;
        let now = Instant::now();
        let min_interval = Duration::from_secs_f32(1.0 / self.fps.max(0.5));
        let quiet_at = (self.last_event + DEBOUNCE).min(since + MAX_STALE);
        let rate_at = self.last_refresh + min_interval;
        Some(quiet_at.max(rate_at).saturating_duration_since(now))
    }

    async fn refresh(&mut self) -> Result<()> {
        self.dirty_since = None;
        let t0 = Instant::now();
        // meta first, so the client never shows a frame with the previous page's title/url
        if let Ok(Some((title, url))) = self.read_meta().await {
            if title != self.title {
                self.title = title.clone();
                let _ = self.out.send((self.id, TabEvent::Title(title)));
            }
            if url != self.url {
                self.url = url.clone();
                let _ = self.out.send((self.id, TabEvent::Url(url)));
            }
        }
        let rendered = match self.mode {
            RenderMode::Pixel => self.render_pixel().await?,
            RenderMode::Text => self.render_text().await?,
        };
        self.last_refresh = Instant::now();
        self.publish_images();
        let m = &self.cfg.metrics;
        m.refreshes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        m.refresh_micros.fetch_add(
            t0.elapsed().as_micros() as u64,
            std::sync::atomic::Ordering::Relaxed,
        );
        let page = rendered.page;
        let _ = self
            .out
            .send((self.id, TabEvent::Frame(Box::new(rendered))));
        let range = (page.content_h - self.m.px_h()).max(1.0);
        let permille = (page.scroll_y / range * 1000.0).clamp(0.0, 1000.0) as u16;
        if permille != self.scroll {
            self.scroll = permille;
            let _ = self.out.send((self.id, TabEvent::Scroll(permille)));
        }
        // release Chromium's screencast throttle only now: this is the back-pressure to the browser
        if let Some(sid) = self.pending_ack.take() {
            let _ = self
                .sess
                .send("Page.screencastFrameAck", json!({ "sessionId": sid }))
                .await;
        }
        Ok(())
    }

    async fn render_pixel(&mut self) -> Result<Rendered> {
        let raw = capture::snapshot_raw(&self.sess).await?;
        let (snap, snap_scroll) =
            tokio::task::spawn_blocking(move || -> Result<(SnapshotResult, Option<(f64, f64)>)> {
                let snap: SnapshotResult = serde_json::from_str(raw.get())?;
                let scroll = snap.documents.first().map(|d| (d.scroll_x, d.scroll_y));
                Ok((snap, scroll))
            })
            .await??;
        // A screencast frame from a different scroll position would paint the old page into
        // cells the snapshot says are empty: drop it and take a fresh screenshot instead.
        let fresh = self
            .frame
            .as_ref()
            .is_some_and(|f| match (f.scroll, snap_scroll) {
                (Some((fx, fy)), Some((sx, sy))) => (fx - sx).abs() < 1.0 && (fy - sy).abs() < 1.0,
                _ => true,
            });
        let jpeg_b64 = match (&self.frame, fresh) {
            (Some(f), true) => f.jpeg_b64.clone(),
            _ => {
                #[derive(Deserialize)]
                struct R {
                    data: String,
                }
                let r: R = self
                    .sess
                    .call("Page.captureScreenshot", json!({ "format": "jpeg", "quality": self.cfg.profile.jpeg_quality, "fromSurface": true }))
                    .await?;
                r.data
            }
        };
        let m = self.m;
        let wants_gfx =
            self.cfg.caps.graphics != glyph_proto::GraphicsProto::None && self.cfg.caps.images;
        let known = if wants_gfx {
            self.images_sent.clone()
        } else {
            Default::default()
        };
        let (tab, quality) = (self.id, self.cfg.profile.jpeg_quality.max(55));
        let (rendered, crops) =
            tokio::task::spawn_blocking(move || -> Result<(Rendered, Vec<Crop>)> {
                let pix = crate::b64::decode(&jpeg_b64)
                    .ok()
                    .and_then(|b| Pixmap::decode_jpeg(&b).ok());
                let r = render(&snap, pix.as_ref(), &m);
                let crops = match (&pix, wants_gfx) {
                    (Some(p), true) => crop_images(p, &m, tab, &r.images, quality, &known),
                    _ => Vec::new(),
                };
                Ok((r, crops))
            })
            .await??;
        self.pending_crops = Some(crops);
        Ok(rendered)
    }

    /// Send new/changed image crops and clear the ones that disappeared.
    fn publish_images(&mut self) {
        let Some(crops) = self.pending_crops.take() else {
            return;
        };
        if self.cfg.caps.graphics == glyph_proto::GraphicsProto::None {
            return;
        }
        let now: std::collections::HashMap<u32, u64> =
            crops.iter().map(|c| (c.id, c.hash)).collect();
        let gone: Vec<u32> = self
            .images_sent
            .keys()
            .filter(|id| !now.contains_key(id))
            .copied()
            .collect();
        if !gone.is_empty() {
            let _ = self.out.send((self.id, TabEvent::ImageClear(gone)));
        }
        for c in crops {
            if let Some(msg) = c.msg {
                let _ = self.out.send((self.id, TabEvent::Image(msg)));
            }
        }
        self.images_sent = now;
    }

    /// Reader mode: (re)build the document from the AX tree when stale, then slice the viewport.
    async fn render_text(&mut self) -> Result<Rendered> {
        if self.text_stale || self.text_doc.is_none() {
            let tree_raw = self
                .sess
                .call_raw("Accessibility.getFullAXTree", json!({}))
                .await?;
            let disp_raw = self
                .sess
                .call_raw(
                    "DOMSnapshot.captureSnapshot",
                    json!({ "computedStyles": ["display"], "includePaintOrder": false, "includeDOMRects": false }),
                )
                .await?;
            let cols = self.m.cols;
            let doc = tokio::task::spawn_blocking(move || -> Result<TextDoc> {
                let tree: AxTree = serde_json::from_str(tree_raw.get())?;
                let snap: SnapshotResult = serde_json::from_str(disp_raw.get())?;
                Ok(textmode::build(
                    &tree,
                    &textmode::displays_from_snapshot(&snap),
                    cols,
                ))
            })
            .await??;
            self.text_doc = Some(doc);
            self.text_stale = false;
            self.text_hits_refresh();
        }
        let doc = self.text_doc.as_ref().expect("built above");
        let max_scroll = doc.lines().saturating_sub(self.m.rows as usize);
        self.text_scroll = self.text_scroll.min(max_scroll);
        let (grid, regions) = doc.slice(self.text_scroll, self.m.rows, self.find_hl.as_deref());
        Ok(Rendered {
            grid,
            regions,
            images: Vec::new(),
            page: PageInfo {
                scroll_x: 0.0,
                scroll_y: self.text_scroll as f64,
                content_w: self.m.cols as f64,
                content_h: (doc.lines().max(self.m.rows as usize)) as f64,
            },
        })
    }

    fn text_hits_refresh(&mut self) {
        self.find_hits = match (&self.text_doc, &self.find_hl) {
            (Some(d), Some(q)) => d.find(q, q.chars().any(char::is_uppercase)),
            _ => Vec::new(),
        };
    }

    async fn read_meta(&self) -> Result<Option<(String, String)>> {
        let v: Value = self
            .sess
            .call(
                "Runtime.evaluate",
                json!({ "expression": "JSON.stringify([document.title, location.href])", "returnByValue": true }),
            )
            .await?;
        let s = v["result"]["value"].as_str().unwrap_or("[]");
        let a: Vec<String> = serde_json::from_str(s).unwrap_or_default();
        Ok((a.len() == 2).then(|| (a[0].clone(), a[1].clone())))
    }

    // ------------------------------------------------------------ browser events

    async fn on_event(&mut self, e: Event) {
        match e.method.as_str() {
            "Page.screencastFrame" => {
                if let Ok(f) = e.parse::<Frame>() {
                    if !self.active || self.mode == RenderMode::Text {
                        let _ = self
                            .sess
                            .send(
                                "Page.screencastFrameAck",
                                json!({ "sessionId": f.session_id }),
                            )
                            .await;
                        return;
                    }
                    // an older unacked frame is superseded (acked together with this one)
                    self.frame = Some(FrameData {
                        jpeg_b64: f.data,
                        scroll: Some((f.metadata.sx, f.metadata.sy)),
                    });
                    // Chromium counts frames in flight: every frame must be acked exactly once,
                    // including ones we superseded before rendering.
                    if let Some(old) = self.pending_ack.replace(f.session_id) {
                        let _ = self
                            .sess
                            .send("Page.screencastFrameAck", json!({ "sessionId": old }))
                            .await;
                    }
                    self.mark_dirty();
                }
            }
            "Page.frameStartedLoading" => {
                self.load = LoadState {
                    loading: true,
                    progress: 5,
                };
                self.emit_load();
            }
            "Page.lifecycleEvent" => {
                let name = e.params.get().contains("\"name\":\"");
                if name {
                    #[derive(Deserialize)]
                    struct L {
                        name: String,
                    }
                    if let Ok(l) = e.parse::<L>() {
                        let p = match l.name.as_str() {
                            "commit" => 20,
                            "DOMContentLoaded" => 55,
                            "firstContentfulPaint" => 70,
                            "load" => 90,
                            "networkAlmostIdle" | "networkIdle" => 100,
                            _ => 0,
                        };
                        if p > self.load.progress || p == 100 {
                            self.load = LoadState {
                                loading: p < 100,
                                progress: p,
                            };
                            self.emit_load();
                        }
                        if matches!(l.name.as_str(), "load" | "DOMContentLoaded" | "commit") {
                            if self.mode == RenderMode::Text {
                                self.text_stale = true;
                                let _ = self
                                    .eval("window.__glyphWatch && window.__glyphWatch(true)")
                                    .await;
                            }
                            self.mark_dirty();
                        }
                    }
                }
            }
            "Page.frameStoppedLoading" => {
                self.load = LoadState {
                    loading: false,
                    progress: 100,
                };
                self.emit_load();
                self.mark_dirty();
            }
            "Page.frameNavigated" => {
                // new document: forget the old pixels so we never mix pages
                self.frame = None;
                self.cursor = None;
                self.text_stale = true;
                self.text_scroll = 0;
                self.text_focus = None;
                self.find_hl = None;
                let _ = self.out.send((self.id, TabEvent::Cursor(None)));
                self.mark_dirty();
            }
            "Runtime.bindingCalled" => {
                #[derive(Deserialize)]
                struct B {
                    payload: String,
                }
                if let Ok(b) = e.parse::<B>() {
                    if let Ok(m) = serde_json::from_str::<AgentMsg>(&b.payload) {
                        self.on_agent(m);
                    }
                }
            }
            "Inspector.targetCrashed" => {
                let _ = self.out.send((self.id, TabEvent::Crashed));
            }
            _ => {}
        }
    }

    fn emit_load(&self) {
        let _ = self.out.send((self.id, TabEvent::Load(self.load)));
    }

    fn on_agent(&mut self, m: AgentMsg) {
        match m.t.as_str() {
            "caret" if self.mode == RenderMode::Text => {
                let pre = if m.pw {
                    "•".repeat(m.pre.chars().count())
                } else {
                    m.pre.clone()
                };
                let pos = self
                    .text_focus
                    .and_then(|ri| self.text_doc.as_ref()?.regions.get(ri))
                    .and_then(|r| {
                        let &(y, x, _) = r.rects.first()?;
                        let row = y
                            .checked_sub(self.text_scroll)
                            .filter(|r| *r < self.m.rows as usize)?;
                        let last = pre.rsplit('\n').next().unwrap_or("");
                        let line = pre.matches('\n').count();
                        let col = (self.m.cols as usize)
                            .saturating_sub(self.text_doc.as_ref()?.margin_width())
                            / 2
                            + x
                            + 1
                            + str_width(last);
                        Some(CursorState {
                            col: col.min(self.m.cols as usize - 1) as u16,
                            row: (row + line).min(self.m.rows as usize - 1) as u16,
                        })
                    });
                if pos != self.cursor {
                    self.cursor = pos;
                    let _ = self.out.send((self.id, TabEvent::Cursor(pos)));
                }
            }
            "caret" => {
                let (cw, ch) = (self.m.cols as f64 * 0.0 + self.m.cw, self.m.ch);
                let pre = if m.pw {
                    "•".repeat(m.pre.chars().count())
                } else {
                    m.pre.clone()
                };
                let c0 = (m.x / cw).round();
                let (col, row) = match m.k.as_str() {
                    "ta" => {
                        let line = pre.matches('\n').count();
                        let last = pre.rsplit('\n').next().unwrap_or("");
                        let top = (m.y / ch).round() + 1.0;
                        (c0 + 1.0 + str_width(last) as f64, top + line as f64)
                    }
                    "in" => (
                        c0 + 1.0 + str_width(&pre) as f64,
                        ((m.y + m.h / 2.0) / ch).floor(),
                    ),
                    _ => (c0, ((m.y + m.h / 2.0) / ch).floor()),
                };
                let pos =
                    (col >= 0.0 && row >= 0.0 && row < self.m.rows as f64).then(|| CursorState {
                        col: (col as u16).min(self.m.cols.saturating_sub(1)),
                        row: row as u16,
                    });
                if pos != self.cursor {
                    self.cursor = pos;
                    let _ = self.out.send((self.id, TabEvent::Cursor(pos)));
                }
            }
            "blur" => {
                if self.cursor.take().is_some() {
                    let _ = self.out.send((self.id, TabEvent::Cursor(None)));
                }
            }
            "copy" => {
                if !m.text.is_empty() {
                    let _ = self.out.send((self.id, TabEvent::Clipboard(m.text)));
                }
            }
            "dirty" => self.mark_dirty(),
            _ => {}
        }
    }

    // ------------------------------------------------------------ commands

    async fn on_cmd(&mut self, c: TabCmd) {
        if let Err(e) = self.handle(c).await {
            tracing::debug!("command failed: {e:#}");
        }
    }

    async fn handle(&mut self, c: TabCmd) -> Result<()> {
        match c {
            TabCmd::Navigate(u) => self.navigate(&u).await,
            TabCmd::Back => self.history(-1).await?,
            TabCmd::Forward => self.history(1).await?,
            TabCmd::Reload => self.sess.send("Page.reload", json!({})).await?,
            TabCmd::Stop => self.sess.send("Page.stopLoading", json!({})).await?,
            TabCmd::Key(k) => {
                for p in key_events(k) {
                    self.sess.send("Input.dispatchKeyEvent", p).await?;
                }
                self.mark_dirty();
            }
            TabCmd::Mouse(m) if self.mode == RenderMode::Text => self.text_mouse(m).await?,
            TabCmd::Mouse(m) => self.mouse(m).await?,
            TabCmd::Scroll { unit, dy, .. } if self.mode == RenderMode::Text => {
                let rows = self.m.rows as i64;
                let cur = self.text_scroll as i64;
                let next = match unit {
                    ScrollUnit::Lines => cur + dy as i64,
                    ScrollUnit::Pages => cur + dy as i64 * (rows - 1).max(1),
                    ScrollUnit::Edge => {
                        if dy < 0 {
                            0
                        } else {
                            i64::MAX / 2
                        }
                    }
                };
                self.text_scroll = next.max(0) as usize;
                self.mark_dirty();
            }
            TabCmd::Scroll {
                unit,
                dx,
                dy,
                col,
                row,
            } => self.scroll(unit, dx, dy, col, row).await?,
            TabCmd::Paste(t) => {
                self.sess
                    .send("Input.insertText", json!({ "text": t }))
                    .await?;
                self.mark_dirty();
            }
            TabCmd::Find {
                query,
                forward,
                case_sensitive,
            } if self.mode == RenderMode::Text => {
                self.text_find(&query, forward, case_sensitive);
            }
            TabCmd::Find {
                query,
                forward,
                case_sensitive,
            } => self.find(&query, forward, case_sensitive).await?,
            TabCmd::ClearFocus => {
                self.find_hl = None;
                self.find_hits.clear();
                self.text_focus = None;
                self.eval("document.activeElement && document.activeElement.blur(); getSelection().removeAllRanges()").await?;
                self.mark_dirty();
            }
            TabCmd::Resize { cols, rows } => {
                self.m.cols = cols.max(1);
                self.m.rows = rows.max(1);
                capture::set_viewport(&self.sess, &self.m).await?;
                self.frame = None;
                self.images_sent.clear(); // the client dropped its placements on resize
                self.text_stale = true; // reader layout depends on width
                if self.mode == RenderMode::Pixel {
                    self.sess.send("Page.stopScreencast", json!({})).await?;
                    self.start_screencast().await?;
                }
                self.mark_dirty();
            }
            TabCmd::SetMode(m) if m != self.mode => self.set_mode(m).await?,
            TabCmd::SetMode(_) => {}
            TabCmd::SetFps(f) => self.fps = f.clamp(0.5, self.cfg.profile.max_fps),
            TabCmd::Active(a) => {
                self.active = a;
                if a {
                    self.sess
                        .send("Page.setWebLifecycleState", json!({ "state": "active" }))
                        .await
                        .ok();
                    self.images_sent.clear();
                    self.frame = None;
                    if self.mode == RenderMode::Pixel {
                        self.start_screencast().await?;
                    }
                    self.mark_dirty();
                } else {
                    self.sess.send("Page.stopScreencast", json!({})).await?;
                    self.sess
                        .send("Page.setWebLifecycleState", json!({ "state": "frozen" }))
                        .await
                        .ok();
                    self.dirty_since = None;
                }
            }
            TabCmd::Refresh => {
                self.images_sent.clear();
                self.frame = None;
                self.mark_dirty();
            }
            TabCmd::Close => {}
        }
        Ok(())
    }

    async fn set_mode(&mut self, m: RenderMode) -> Result<()> {
        self.mode = m;
        self.frame = None;
        self.cursor = None;
        let _ = self.out.send((self.id, TabEvent::Cursor(None)));
        match m {
            RenderMode::Text => {
                if !self.images_sent.is_empty() {
                    let ids = self.images_sent.drain().map(|(id, _)| id).collect();
                    let _ = self.out.send((self.id, TabEvent::ImageClear(ids)));
                }
                self.sess.send("Page.stopScreencast", json!({})).await?;
                if let Some(sid) = self.pending_ack.take() {
                    let _ = self
                        .sess
                        .send("Page.screencastFrameAck", json!({ "sessionId": sid }))
                        .await;
                }
                self.sess.send("Accessibility.enable", json!({})).await?;
                self.eval("window.__glyphWatch && window.__glyphWatch(true)")
                    .await?;
                self.text_stale = true;
                self.text_scroll = 0;
            }
            RenderMode::Pixel => {
                self.eval("window.__glyphWatch && window.__glyphWatch(false)")
                    .await?;
                self.sess
                    .send("Accessibility.disable", json!({}))
                    .await
                    .ok();
                self.start_screencast().await?;
                self.text_doc = None;
            }
        }
        let _ = self.out.send((self.id, TabEvent::Mode(m)));
        self.mark_dirty();
        Ok(())
    }

    /// Reader-mode click: find the region under the cell and activate its DOM node.
    async fn text_mouse(&mut self, m: MouseEvent) -> Result<()> {
        if m.kind != MouseKind::Up(MouseButton::Left) {
            return Ok(());
        }
        let Some(doc) = &self.text_doc else {
            return Ok(());
        };
        let y = self.text_scroll + m.row as usize;
        let margin = doc.margin_width();
        let hit = doc.regions.iter().enumerate().find(|(_, r)| {
            r.rects.iter().any(|&(ry, rx, rw)| {
                ry == y && (m.col as usize) >= margin + rx && (m.col as usize) < margin + rx + rw
            })
        });
        let Some((ri, r)) = hit else { return Ok(()) };
        let backend = r.backend;
        let text_entry = matches!(
            r.kind,
            glyph_proto::RegionKind::Input | glyph_proto::RegionKind::TextArea
        );
        self.text_focus = text_entry.then_some(ri);
        if backend < 0 {
            return Ok(());
        }
        #[derive(Deserialize)]
        struct Obj {
            object: O,
        }
        #[derive(Deserialize)]
        struct O {
            #[serde(rename = "objectId")]
            id: String,
        }
        let o: Obj = self
            .sess
            .call("DOM.resolveNode", json!({ "backendNodeId": backend }))
            .await?;
        self.sess
            .send(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": o.object.id,
                    "functionDeclaration": "function(){ if (this.focus) this.focus(); if (this.click) this.click(); }",
                    "silent": true,
                }),
            )
            .await?;
        self.text_stale = true;
        self.mark_dirty();
        Ok(())
    }

    /// Find in reader mode: search the document text, scroll to the next/previous hit.
    fn text_find(&mut self, q: &str, forward: bool, case: bool) {
        self.find_hl = (!q.is_empty()).then(|| q.to_owned());
        let Some(doc) = &self.text_doc else { return };
        let hits = doc.find(q, case);
        let _ = self
            .out
            .send((self.id, TabEvent::FindResult(hits.len() as u32)));
        let cur = self.text_scroll;
        let next = if forward {
            hits.iter()
                .copied()
                .find(|&y| y > cur || (y == cur && cur == 0))
                .or_else(|| hits.first().copied())
        } else {
            hits.iter()
                .rev()
                .copied()
                .find(|&y| y < cur)
                .or_else(|| hits.last().copied())
        };
        if let Some(y) = next {
            self.text_scroll = y.saturating_sub(2);
        }
        self.find_hits = hits;
        self.mark_dirty();
    }

    async fn navigate(&mut self, url: &str) {
        self.load = LoadState {
            loading: true,
            progress: 3,
        };
        self.emit_load();
        match self
            .sess
            .call::<Value>("Page.navigate", json!({ "url": url }))
            .await
        {
            Ok(v) => {
                if let Some(err) = v.get("errorText").and_then(Value::as_str) {
                    tracing::debug!("navigate error: {err}");
                    self.load = LoadState {
                        loading: false,
                        progress: 100,
                    };
                    self.emit_load();
                }
            }
            Err(e) => tracing::debug!("navigate failed: {e:#}"),
        }
    }

    async fn eval(&self, expr: &str) -> Result<Value> {
        self.sess
            .call(
                "Runtime.evaluate",
                json!({ "expression": expr, "returnByValue": true }),
            )
            .await
    }

    async fn history(&mut self, delta: i32) -> Result<()> {
        #[derive(Deserialize)]
        struct H {
            #[serde(rename = "currentIndex")]
            cur: i32,
            entries: Vec<E>,
        }
        #[derive(Deserialize)]
        struct E {
            id: i64,
        }
        let h: H = self
            .sess
            .call("Page.getNavigationHistory", json!({}))
            .await?;
        let i = h.cur + delta;
        if i >= 0 && (i as usize) < h.entries.len() {
            self.sess
                .send(
                    "Page.navigateToHistoryEntry",
                    json!({ "entryId": h.entries[i as usize].id }),
                )
                .await?;
        }
        Ok(())
    }

    async fn mouse(&mut self, m: MouseEvent) -> Result<()> {
        let x = (m.col as f64 + 0.5) * self.m.cw;
        let y = (m.row as f64 + 0.5) * self.m.ch;
        self.mouse = (x, y);
        let btn = |b: MouseButton| match b {
            MouseButton::Left => "left",
            MouseButton::Middle => "middle",
            MouseButton::Right => "right",
        };
        let (ty, button) = match m.kind {
            MouseKind::Down(b) => ("mousePressed", btn(b)),
            MouseKind::Up(b) => ("mouseReleased", btn(b)),
            MouseKind::Move => ("mouseMoved", "none"),
        };
        self.sess
            .send(
                "Input.dispatchMouseEvent",
                json!({ "type": ty, "x": x, "y": y, "button": button, "clickCount": m.clicks.max(1), "modifiers": cdp_mods(m.mods) }),
            )
            .await?;
        self.mark_dirty();
        Ok(())
    }

    async fn scroll(
        &mut self,
        unit: ScrollUnit,
        dx: i32,
        dy: i32,
        col: u16,
        row: u16,
    ) -> Result<()> {
        match unit {
            ScrollUnit::Edge => {
                let js = if dy < 0 {
                    "window.scrollTo(0,0)"
                } else {
                    "window.scrollTo(0, document.documentElement.scrollHeight)"
                };
                self.eval(js).await?;
            }
            _ => {
                let (ppx, ppy) = match unit {
                    ScrollUnit::Lines => (self.m.cw, self.m.ch),
                    _ => (self.m.px_w() - self.m.cw, self.m.px_h() - self.m.ch),
                };
                self.sess
                    .send(
                        "Input.dispatchMouseEvent",
                        json!({
                            "type": "mouseWheel",
                            "x": (col as f64 + 0.5) * self.m.cw,
                            "y": (row as f64 + 0.5) * self.m.ch,
                            "deltaX": dx as f64 * ppx,
                            "deltaY": dy as f64 * ppy,
                        }),
                    )
                    .await?;
            }
        }
        self.mark_dirty();
        Ok(())
    }

    async fn find(&mut self, q: &str, forward: bool, cs: bool) -> Result<()> {
        let js = format!(
            "(function(q,cs,fwd){{if(!q)return 0;window.find(q,cs,!fwd,true,false,false,false);\
             const t=document.body?document.body.innerText:'';\
             const re=new RegExp(q.replace(/[.*+?^${{}}()|[\\]\\\\]/g,'\\\\$&'),cs?'g':'gi');return (t.match(re)||[]).length}})({},{},{})",
            json!(q),
            cs,
            forward
        );
        let v = self.eval(&js).await?;
        let n = v["result"]["value"].as_u64().unwrap_or(0) as u32;
        let _ = self.out.send((self.id, TabEvent::FindResult(n)));
        self.mark_dirty();
        Ok(())
    }
}
