//! One browser tab: a task that owns a CDP session, keeps the page's viewport in sync with the
//! client, turns paint activity into rendered frames, and executes input.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

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
/// How long after an input the higher interactive frame rate stays available.
const INTERACTIVE: Duration = Duration::from_millis(900);
/// Frame-rate ceiling while interacting (scroll, typing), whatever the profile's idle cap.
const INTERACTIVE_FPS: f32 = 20.0;

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
    /// Scroll offset reported with the latest screencast frame.
    frame_scroll: Option<(f64, f64)>,
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
    interactive_until: Instant,
    /// Last pixel-mode frame, kept for the live-pixel fast path.
    last: Option<Rendered>,
    /// The screenshot `last` was rendered from: the baseline for spotting non-live changes.
    last_pix: Option<Arc<Pixmap>>,
    /// A verified screenshot handed from the live path to the full refresh that follows it.
    pre_shot: Option<String>,
    /// Screencast delivers full-size frames (live video on the page) instead of the tiny signal.
    big_cast: bool,
    /// Newest full-size screencast frame, consumed by the live path.
    frame_data: Option<String>,
    /// Something other than a repaint happened since the last full refresh (DOM, scroll, input).
    full_dirty: bool,
}

/// The injected page agent (animation/caret killer, terminal-native page style, scroll helper,
/// focus/caret/clipboard reporting) with its parameters filled in.
pub fn agent_js(terminal: bool, cw: f64, ch: f64) -> String {
    AGENT_JS
        .replace("__TERMINAL__", if terminal { "true" } else { "false" })
        .replace("__CW__", &cw.to_string())
        .replace("__CH__", &ch.to_string())
}

fn scroll_of(v: &Value) -> (f64, f64) {
    let a = &v["result"]["value"];
    (a[0].as_f64().unwrap_or(0.0), a[1].as_f64().unwrap_or(0.0))
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
    let fps = INTERACTIVE_FPS; // the session's AIMD controller lowers it on slow links
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
        frame_scroll: None,
        pending_ack: None,
        last: None,
        last_pix: None,
        pre_shot: None,
        big_cast: false,
        frame_data: None,
        full_dirty: true,
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
        interactive_until: Instant::now(),
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
    #[serde(default)]
    metadata: FrameMeta,
    #[serde(rename = "sessionId")]
    session_id: i64,
    #[serde(default)]
    data: String,
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
            json!({ "source": self.agent_source() }),
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

    /// The page agent with this tab's style parameters filled in.
    fn agent_source(&self) -> String {
        agent_js(
            self.cfg.caps.page_style == glyph_proto::PageStyle::Terminal,
            self.cfg.cw,
            self.cfg.ch,
        )
    }

    /// The screencast is only a "something was painted" signal (it fires exactly when Chromium
    /// produced new pixels, so an idle page costs nothing). Real pixels come from a screenshot
    /// taken together with the DOM snapshot, so the frames themselves are requested tiny and at
    /// the lowest quality: full-size ones cost Chromium encode time and CDP bandwidth for nothing.
    async fn start_screencast(&self) -> Result<()> {
        let _ = self.sess.send("Page.stopScreencast", json!({})).await;
        // normally a tiny frame: only the "something painted" signal matters. With video on the
        // page the frames themselves are the pixel source (pushed, so no capture round trip)
        let params = if self.big_cast {
            json!({ "format": "jpeg", "quality": self.cfg.profile.jpeg_quality, "maxWidth": self.m.px_w() as u32, "maxHeight": self.m.px_h() as u32, "everyNthFrame": 1 })
        } else {
            json!({ "format": "jpeg", "quality": 10, "maxWidth": 32, "maxHeight": 32, "everyNthFrame": 1 })
        };
        self.sess.send("Page.startScreencast", params).await
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

    /// An input just changed the page: refresh as soon as the rate limit allows, and allow a
    /// higher rate for a moment (scrolling should feel fluid even in the `lean` profile).
    fn mark_urgent(&mut self) {
        self.full_dirty = true;
        let now = Instant::now();
        self.interactive_until = now + INTERACTIVE;
        self.last_event = now - DEBOUNCE;
        self.dirty_since.get_or_insert(now - DEBOUNCE);
    }

    fn mark_dirty(&mut self) {
        self.full_dirty = true;
        self.mark_paint();
    }

    /// Pixels changed (screencast signal), nothing else known to have: live regions can take the
    /// cheap path.
    fn mark_paint(&mut self) {
        let now = Instant::now();
        // with video on screen paints never stop; letting each one extend the quiet period would
        // hold the refresh back until MAX_STALE, so only the first paint of a burst counts
        let live = self.last.as_ref().is_some_and(|l| !l.live.is_empty());
        if !live || self.dirty_since.is_none() {
            self.last_event = now;
        }
        self.dirty_since.get_or_insert(now);
    }

    fn next_refresh_in(&self) -> Option<Duration> {
        if !self.active {
            return None;
        }
        let since = self.dirty_since?;
        let now = Instant::now();
        let live = self.mode == RenderMode::Pixel
            && self.last.as_ref().is_some_and(|l| !l.live.is_empty());
        let cap = if now < self.interactive_until {
            self.fps.min(INTERACTIVE_FPS)
        } else if live {
            self.fps.min(self.cfg.profile.live_fps)
        } else {
            self.fps.min(self.cfg.profile.max_fps)
        };
        let min_interval = Duration::from_secs_f32(1.0 / cap.max(0.5));
        let quiet_at = (self.last_event + DEBOUNCE).min(since + MAX_STALE);
        let rate_at = self.last_refresh + min_interval;
        Some(quiet_at.max(rate_at).saturating_duration_since(now))
    }

    async fn refresh(&mut self) -> Result<()> {
        self.dirty_since = None;
        let t0 = Instant::now();
        if !self.full_dirty && self.mode == RenderMode::Pixel {
            match self.refresh_live().await {
                Ok(Some(r)) => return self.emit(r, t0).await,
                Ok(None) => {}
                Err(e) => tracing::debug!("live refresh failed: {e:#}"),
            }
        }
        self.full_dirty = false;
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
        self.last = (self.mode == RenderMode::Pixel).then(|| rendered.clone());
        let want_big = self.last.as_ref().is_some_and(|l| !l.live.is_empty());
        if want_big != self.big_cast {
            self.big_cast = want_big;
            self.frame_data = None;
            let _ = self.start_screencast().await;
        }
        self.emit(rendered, t0).await
    }

    /// Send a finished frame to the client and release the screencast throttle.
    async fn emit(&mut self, rendered: Rendered, t0: Instant) -> Result<()> {
        self.last_refresh = Instant::now();
        tracing::debug!(
            "refresh {:?} live={} fps={}",
            t0.elapsed(),
            rendered.live.len(),
            self.fps
        );
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

    /// Cheap refresh for pages whose only change is video/canvas pixels: one screenshot, no DOM
    /// snapshot, only the live rectangles recoloured in the previous frame. `None` when the
    /// preconditions fail (nothing live, the page scrolled), and the caller does a full refresh.
    async fn refresh_live(&mut self) -> Result<Option<Rendered>> {
        let (Some(last), Some(base)) = (
            self.last.as_ref().filter(|l| !l.live.is_empty()),
            self.last_pix.clone(),
        ) else {
            return Ok(None);
        };
        let at = (last.page.scroll_x, last.page.scroll_y);
        let same = |a: (f64, f64)| (a.0 - at.0).abs() < 1.0 && (a.1 - at.1).abs() < 1.0;
        // a pushed screencast frame carries its own scroll offset; otherwise take a screenshot
        // between two scroll reads
        let pushed = self
            .frame_data
            .take()
            .filter(|_| self.frame_scroll.is_some_and(same));
        let data = match pushed {
            Some(d) => d,
            None => {
                if !same(self.read_scroll().await?) {
                    return Ok(None);
                }
                #[derive(Deserialize)]
                struct R {
                    data: String,
                }
                let quality = self.cfg.profile.jpeg_quality;
                let r: R = self
                    .sess
                    .call(
                        "Page.captureScreenshot",
                        json!({ "format": "jpeg", "quality": quality, "fromSurface": true }),
                    )
                    .await?;
                if !same(self.read_scroll().await?) {
                    return Ok(None);
                }
                r.data
            }
        };
        let (mut out, m) = (last.clone(), self.m);
        let (out, data) = tokio::task::spawn_blocking(move || {
            let pix = Pixmap::decode_jpeg(&crate::b64::decode(&data).ok()?).ok()?;
            if crate::colour::changed_outside(&base, &pix, &out.live, &m) {
                return Some((None, data));
            }
            for rect in out.live.clone() {
                crate::colour::recolour_live(&mut out.grid, rect, &pix, &m);
            }
            Some((Some(out), data))
        })
        .await?
        .unzip();
        // something besides the live pixels changed (a label, the layout): the full refresh continues
        // from this screenshot instead of taking another
        if matches!(out, Some(None)) {
            self.pre_shot = data;
        }
        Ok(out.flatten())
    }

    /// Scroll offset once the compositor has committed the main thread's state (two animation
    /// frames), falling back to a plain read if the page cannot run animation frames right now.
    async fn settled_scroll(&self) -> Result<(f64, f64)> {
        let settled = self.sess.call::<Value>(
            "Runtime.evaluate",
            json!({
                "expression": "window.__glyphSettled ? window.__glyphSettled() : Promise.resolve([scrollX, scrollY])",
                "awaitPromise": true,
                "returnByValue": true
            }),
        );
        match tokio::time::timeout(Duration::from_millis(400), settled).await {
            Ok(Ok(v)) => Ok(scroll_of(&v)),
            _ => self.read_scroll().await,
        }
    }

    async fn read_scroll(&self) -> Result<(f64, f64)> {
        let v = self.eval("(function(){const r=document.scrollingElement||document.documentElement;return [r.scrollLeft,r.scrollTop]})()").await?;
        Ok(scroll_of(&v))
    }

    /// Snapshot + screenshot that provably belong together. Text comes from the snapshot and colour
    /// from the screenshot; if the page scrolled between the two, mixing them would draw rows with
    /// the wrong colours. So the scroll offset is read before and after, and the capture is
    /// retried until the snapshot and both reads agree. If it never settles the frame is rendered
    /// from the DOM alone (no pixels) and a retry is scheduled, never as a mixed frame.
    async fn render_pixel(&mut self) -> Result<Rendered> {
        let quality = self.cfg.profile.jpeg_quality;
        let mut shot: Option<String> = None;
        let mut snap: Option<SnapshotResult> = None;
        let mut pre = self.pre_shot.take();
        for attempt in 0..3 {
            #[derive(Deserialize)]
            struct R {
                data: String,
            }
            let (s0, raw, r) = if let Some(data) = pre.take() {
                // the live path already took (and scroll-verified) a screenshot: only the DOM is missing
                let at = self
                    .last
                    .as_ref()
                    .map(|l| (l.page.scroll_x, l.page.scroll_y))
                    .unwrap_or_default();
                (
                    at,
                    capture::snapshot_raw(&self.sess).await,
                    Ok::<R, anyhow::Error>(R { data }),
                )
            } else {
                let s0 = self.settled_scroll().await?;
                // the two captures run concurrently; the scroll reads around them prove they agree
                let (raw, r) = tokio::join!(
                    capture::snapshot_raw(&self.sess),
                    self.sess.call::<R>(
                        "Page.captureScreenshot",
                        json!({ "format": "jpeg", "quality": quality, "fromSurface": true })
                    )
                );
                (s0, raw, r)
            };
            let (raw, r) = (raw?, r?);
            let s1 = self.read_scroll().await?;
            let parsed = tokio::task::spawn_blocking(move || {
                serde_json::from_str::<SnapshotResult>(raw.get())
            })
            .await??;
            let at = parsed
                .documents
                .first()
                .map(|d| (d.scroll_x, d.scroll_y))
                .unwrap_or(s1);
            let same =
                |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 1.0 && (a.1 - b.1).abs() < 1.0;
            let consistent = same(s0, s1) && same(s1, at);
            snap = Some(parsed);
            if consistent {
                shot = Some(r.data);
                break;
            }
            tracing::debug!("capture attempt {attempt} raced a scroll ({s0:?} {s1:?} {at:?})");
        }
        let snap = snap.expect("at least one attempt ran");
        if shot.is_none() {
            self.mark_dirty(); // try again shortly; this frame is text only
        }
        let m = self.m;
        let wants_gfx =
            self.cfg.caps.graphics != glyph_proto::GraphicsProto::None && self.cfg.caps.images;
        let known = if wants_gfx {
            self.images_sent.clone()
        } else {
            Default::default()
        };
        let (tab, crop_quality) = (self.id, self.cfg.profile.jpeg_quality.max(55));
        let (rendered, crops, pix) = tokio::task::spawn_blocking(
            move || -> Result<(Rendered, Vec<Crop>, Option<Pixmap>)> {
                let pix = shot
                    .and_then(|b64| crate::b64::decode(&b64).ok())
                    .and_then(|b| Pixmap::decode_jpeg(&b).ok());
                let r = render(&snap, pix.as_ref(), &m);
                let crops = match (&pix, wants_gfx) {
                    (Some(p), true) => crop_images(p, &m, tab, &r.images, crop_quality, &known),
                    _ => Vec::new(),
                };
                Ok((r, crops, pix))
            },
        )
        .await??;
        self.last_pix = pix.map(Arc::new);
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
            live: Vec::new(),
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
                    self.frame_scroll = Some((f.metadata.sx, f.metadata.sy));
                    if self.big_cast {
                        self.frame_data = Some(f.data);
                    }
                    // Chromium counts frames in flight: every frame must be acked exactly once,
                    // including ones we superseded before rendering.
                    // (not awaited: at 60 frames/s a round trip per frame would starve the refresh timer)
                    if let Some(old) = self.pending_ack.replace(f.session_id) {
                        let sess = self.sess.clone();
                        tokio::spawn(async move {
                            let _ = sess
                                .send("Page.screencastFrameAck", json!({ "sessionId": old }))
                                .await;
                        });
                    }
                    self.mark_paint();
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
                self.frame_scroll = None;
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
                self.mark_urgent();
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
                self.frame_scroll = None;
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
            TabCmd::SetFps(f) => self.fps = f.clamp(0.5, INTERACTIVE_FPS),
            TabCmd::Active(a) => {
                self.active = a;
                if a {
                    self.sess
                        .send("Page.setWebLifecycleState", json!({ "state": "active" }))
                        .await
                        .ok();
                    self.images_sent.clear();
                    self.frame_scroll = None;
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
                self.frame_scroll = None;
                self.mark_dirty();
            }
            TabCmd::Close => {}
        }
        Ok(())
    }

    async fn set_mode(&mut self, m: RenderMode) -> Result<()> {
        self.mode = m;
        self.frame_scroll = None;
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
        self.mark_urgent();
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
        let (x, y) = (
            (col as f64 + 0.5) * self.m.cw,
            (row as f64 + 0.5) * self.m.ch,
        );
        let (ppx, ppy) = match unit {
            ScrollUnit::Lines => (self.m.cw, self.m.ch),
            _ => (self.m.px_w() - self.m.cw, self.m.px_h() - self.m.ch),
        };
        let edge = if unit == ScrollUnit::Edge {
            dy.signum()
        } else {
            0
        };
        let js = format!(
            "window.__glyphScroll({x},{y},{},{},{edge})",
            dx as f64 * ppx,
            dy as f64 * ppy
        );
        self.eval(&js).await?;
        self.mark_urgent();
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
