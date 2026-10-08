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
    keys::{cdp_mods, key_events},
    pixmap::Pixmap,
    profile::ProfileCfg,
    render::{render, Metrics, Rendered},
    snapshot::SnapshotResult,
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
    Crashed,
}

#[derive(Clone)]
pub struct TabCfg {
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
    let events = sess.events();
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
    };
    tokio::spawn(async move {
        if let Err(e) = tab.run(start_url).await {
            tracing::warn!("tab {id} ended: {e:#}");
        }
    });
    TabHandle { id, tx }
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
            json!({ "features": [{ "name": "prefers-reduced-motion", "value": "reduce" }] }),
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
            RenderMode::Text => self.render_pixel().await?, // text mode lands in M3
        };
        self.last_refresh = Instant::now();
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
        tracing::trace!("refresh {:?}", t0.elapsed());
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
        tokio::task::spawn_blocking(move || -> Result<Rendered> {
            let pix = crate::b64::decode(&jpeg_b64)
                .ok()
                .and_then(|b| Pixmap::decode_jpeg(&b).ok());
            Ok(render(&snap, pix.as_ref(), &m))
        })
        .await?
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
                    if !self.active {
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
                    self.pending_ack = Some(f.session_id);
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
            "Fetch.requestPaused" => {
                #[derive(Deserialize)]
                struct P {
                    #[serde(rename = "requestId")]
                    id: String,
                    #[serde(rename = "resourceType")]
                    ty: String,
                }
                if let Ok(p) = e.parse::<P>() {
                    // domain patterns also match top-level navigations: never block those
                    if p.ty == "Document" {
                        let _ = self
                            .sess
                            .send("Fetch.continueRequest", json!({ "requestId": p.id }))
                            .await;
                    } else {
                        let _ = self
                            .sess
                            .send(
                                "Fetch.failRequest",
                                json!({ "requestId": p.id, "errorReason": "BlockedByClient" }),
                            )
                            .await;
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
            "caret" => {
                let (cw, ch) = (self.m.cw, self.m.ch);
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
            TabCmd::Mouse(m) => self.mouse(m).await?,
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
            } => self.find(&query, forward, case_sensitive).await?,
            TabCmd::ClearFocus => {
                self.eval("document.activeElement && document.activeElement.blur(); getSelection().removeAllRanges()").await?;
                self.mark_dirty();
            }
            TabCmd::Resize { cols, rows } => {
                self.m.cols = cols.max(1);
                self.m.rows = rows.max(1);
                self.sess.send("Page.stopScreencast", json!({})).await?;
                capture::set_viewport(&self.sess, &self.m).await?;
                self.frame = None;
                self.start_screencast().await?;
                self.mark_dirty();
            }
            TabCmd::SetMode(m) => {
                self.mode = m;
                self.mark_dirty();
            }
            TabCmd::SetFps(f) => self.fps = f.clamp(0.5, self.cfg.profile.max_fps),
            TabCmd::Active(a) => {
                self.active = a;
                if a {
                    self.sess
                        .send("Page.setWebLifecycleState", json!({ "state": "active" }))
                        .await
                        .ok();
                    self.frame = None;
                    self.start_screencast().await?;
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
                self.frame = None;
                self.mark_dirty();
            }
            TabCmd::Close => {}
        }
        Ok(())
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
