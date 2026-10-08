//! The server proper: one Chromium, many client sessions, each with its own browser context.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{anyhow, Result};
use glyph_proto::{
    ClientCaps, ClientMsg, Profile, ServerHello, ServerMsg, TabId, TabInfo, PROTO_VERSION,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::{
    browser::{Browser, LaunchOptions},
    outbox::Outbox,
    profile::ProfileCfg,
    tab::{self, TabCfg, TabCmd, TabEvent, TabHandle},
};

#[derive(Clone, Debug)]
pub struct ServerCfg {
    pub profile: Profile,
    pub chrome: Option<PathBuf>,
    /// CSS pixels per terminal cell.
    pub cw: f64,
    pub ch: f64,
    /// URL schemes clients may navigate to, in addition to http/https/about:blank.
    pub extra_schemes: Vec<String>,
    /// Unacknowledged frames allowed in flight.
    pub window: u64,
    /// Extra Chromium command-line flags (tests, `--chrome-arg`).
    pub chrome_args: Vec<String>,
    /// Override the profile's background-tab discard delay (seconds; `Some(0)` = never).
    pub discard_after_secs: Option<u64>,
}

impl Default for ServerCfg {
    fn default() -> Self {
        Self {
            profile: Profile::Balanced,
            chrome: None,
            cw: 8.0,
            ch: 16.0,
            extra_schemes: vec![],
            window: 2,
            chrome_args: vec![],
            discard_after_secs: None,
        }
    }
}

pub struct Server {
    pub browser: Browser,
    pub cfg: ServerCfg,
    pub metrics: Arc<crate::metrics::Metrics>,
    popups: Mutex<HashMap<String, UnboundedSender<String>>>,
}

pub struct SessionHandle {
    pub tx: UnboundedSender<ClientMsg>,
    pub rx: UnboundedReceiver<ServerMsg>,
}

impl Server {
    pub async fn start(cfg: ServerCfg) -> Result<Arc<Self>> {
        let profile = ProfileCfg::for_profile(cfg.profile);
        let mut extra_args = profile.chrome_flags();
        extra_args.extend(cfg.chrome_args.iter().cloned());
        let browser = Browser::launch(&LaunchOptions {
            chrome: cfg.chrome.clone(),
            extra_args,
        })
        .await?;
        browser
            .cdp()
            .call_raw(
                None,
                "Target.setDiscoverTargets",
                json!({ "discover": true }),
            )
            .await?;
        let srv = Arc::new(Self {
            browser,
            cfg,
            metrics: Arc::default(),
            popups: Mutex::new(HashMap::new()),
        });

        // pages opened by pages (target=_blank, window.open) become tabs of the opener's session
        let mut ev = srv.browser.cdp().subscribe("");
        let weak = Arc::downgrade(&srv);
        tokio::spawn(async move {
            #[derive(Deserialize)]
            struct Created {
                #[serde(rename = "targetInfo")]
                info: Info,
            }
            #[derive(Deserialize)]
            struct Info {
                #[serde(rename = "targetId")]
                id: String,
                #[serde(rename = "type")]
                ty: String,
                #[serde(rename = "openerId")]
                opener: Option<String>,
                #[serde(rename = "browserContextId")]
                ctx: Option<String>,
            }
            while let Some(e) = ev.recv().await {
                if e.method != "Target.targetCreated" {
                    continue;
                }
                let Some(srv) = weak.upgrade() else { break };
                if let Ok(c) = e.parse::<Created>() {
                    if c.info.ty == "page" && c.info.opener.is_some() {
                        if let Some(tx) = c
                            .info
                            .ctx
                            .and_then(|ctx| srv.popups.lock().unwrap().get(&ctx).cloned())
                        {
                            let _ = tx.send(c.info.id);
                        }
                    }
                }
            }
        });
        Ok(srv)
    }

    pub fn is_allowed_url(&self, url: &str) -> bool {
        let Ok(u) = url::Url::parse(url) else {
            return false;
        };
        match u.scheme() {
            "http" | "https" => true,
            "about" => url == "about:blank",
            s => self.cfg.extra_schemes.iter().any(|x| x == s),
        }
    }

    /// Start a session for a client with `caps`. The session emits `Hello` first.
    pub fn open_session(self: &Arc<Self>, caps: ClientCaps) -> SessionHandle {
        let (ctx_tx, ctx_rx) = unbounded_channel();
        let (out_tx, out_rx) = unbounded_channel();
        let srv = self.clone();
        tokio::spawn(async move {
            if let Err(e) = Session::run(srv, caps, ctx_rx, out_tx.clone()).await {
                let _ = out_tx.send(ServerMsg::Error(format!("{e:#}")));
            }
        });
        SessionHandle {
            tx: ctx_tx,
            rx: out_rx,
        }
    }
}

struct TabState {
    handle: TabHandle,
    target_id: String,
    info: TabInfo,
    /// Browser target closed to save memory; revived (reloaded) on activation.
    discarded: bool,
    idle_since: Option<std::time::Instant>,
}

struct Session {
    srv: Arc<Server>,
    ctx: String,
    caps: ClientCaps,
    tabs: Vec<TabState>,
    active: TabId,
    next_id: TabId,
    ev_tx: UnboundedSender<(TabId, TabEvent)>,
    out: UnboundedSender<ServerMsg>,
    outbox: Outbox,
    profile: ProfileCfg,
    rate: Rate,
}

/// Additive-increase / multiplicative-decrease frame-rate controller driven by ack latency.
struct Rate {
    fps: f32,
    max: f32,
}

impl Rate {
    fn update(&mut self, rtt_ms: f32) {
        if rtt_ms > 250.0 {
            self.fps = (self.fps * 0.6).max(1.0);
        } else if rtt_ms < 100.0 {
            self.fps = (self.fps + 0.5).min(self.max);
        }
    }
}

impl Session {
    async fn run(
        srv: Arc<Server>,
        caps: ClientCaps,
        mut rx: UnboundedReceiver<ClientMsg>,
        out: UnboundedSender<ServerMsg>,
    ) -> Result<()> {
        let ctx = srv.browser.new_context().await?;
        let (popup_tx, mut popup_rx) = unbounded_channel();
        srv.popups.lock().unwrap().insert(ctx.clone(), popup_tx);
        let (ev_tx, mut ev_rx) = unbounded_channel();
        let profile = ProfileCfg::for_profile(srv.cfg.profile);
        let window = srv.cfg.window;
        let mut s = Session {
            srv,
            ctx,
            caps,
            tabs: Vec::new(),
            active: 0,
            next_id: 1,
            ev_tx,
            out,
            outbox: Outbox::new(0, window, 0),
            rate: Rate {
                fps: profile.max_fps,
                max: profile.max_fps,
            },
            profile,
        };
        let _ = s.out.send(ServerMsg::Hello(ServerHello {
            version: PROTO_VERSION,
            session: s.ctx.clone(),
            profile: s.srv.cfg.profile,
        }));
        s.open_tab(None, None).await?;

        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = tick.tick() => s.discard_idle(),
                msg = rx.recv() => match msg {
                    None => break,
                    Some(m) => s.on_client(m).await,
                },
                ev = ev_rx.recv() => if let Some((id, e)) = ev { s.on_tab_event(id, e) },
                target = popup_rx.recv() => if let Some(t) = target {
                    if let Err(e) = s.open_tab(None, Some(t)).await {
                        tracing::debug!("popup adopt failed: {e:#}");
                    }
                },
            }
        }
        s.srv.popups.lock().unwrap().remove(&s.ctx);
        for t in s.tabs.drain(..) {
            t.handle.send(TabCmd::Close);
        }
        let _ = s.srv.browser.dispose_context(&s.ctx).await;
        Ok(())
    }

    fn send(&self, m: ServerMsg) {
        self.srv.metrics.count_msg(&m);
        let _ = self.out.send(m);
    }

    fn tab(&self, id: TabId) -> Option<&TabState> {
        self.tabs.iter().find(|t| t.info.id == id)
    }

    fn tab_mut(&mut self, id: TabId) -> Option<&mut TabState> {
        self.tabs.iter_mut().find(|t| t.info.id == id)
    }

    fn send_tabs(&self) {
        self.send(ServerMsg::Tabs {
            active: self.active,
            tabs: self.tabs.iter().map(|t| t.info.clone()).collect(),
        });
    }

    /// Create (or adopt) a tab and make it the active one.
    async fn open_tab(&mut self, url: Option<String>, adopt: Option<String>) -> Result<()> {
        #[derive(Deserialize)]
        struct A {
            #[serde(rename = "sessionId")]
            id: String,
        }
        let (target_id, sess) = match adopt {
            Some(t) => {
                let a: A = self
                    .srv
                    .browser
                    .cdp()
                    .call(
                        None,
                        "Target.attachToTarget",
                        json!({ "targetId": t, "flatten": true }),
                    )
                    .await?;
                (t, self.srv.browser.cdp().session(&a.id))
            }
            None => self.srv.browser.new_target(Some(&self.ctx)).await?,
        };
        let id = self.next_id;
        self.next_id += 1;
        let cfg = self.tab_cfg();
        if let Some(u) = url.as_deref().filter(|u| !self.srv.is_allowed_url(u)) {
            return Err(anyhow!("navigation to {u} is not allowed"));
        }
        let handle = tab::spawn(id, sess, cfg, self.ev_tx.clone(), url.clone());
        let info = TabInfo {
            id,
            title: String::new(),
            url: url.unwrap_or_default(),
            loading: false,
        };
        self.tabs.push(TabState {
            handle,
            target_id,
            info,
            discarded: false,
            idle_since: None,
        });
        self.activate(id).await;
        Ok(())
    }

    fn tab_cfg(&self) -> TabCfg {
        TabCfg {
            profile: self.profile.clone(),
            caps: self.caps,
            cw: self.srv.cfg.cw,
            ch: self.srv.cfg.ch,
            metrics: self.srv.metrics.clone(),
        }
    }

    async fn activate(&mut self, id: TabId) {
        if self.tab(id).is_none() {
            return;
        }
        if self.active != id {
            if let Some(old) = self.tab_mut(self.active) {
                old.handle.send(TabCmd::Active(false));
                old.idle_since = Some(std::time::Instant::now());
            }
            if self.tab(id).is_some_and(|t| t.discarded) {
                if let Err(e) = self.revive(id).await {
                    self.send(ServerMsg::Error(format!("could not restore tab: {e:#}")));
                }
            } else if let Some(new) = self.tab(id) {
                new.handle.send(TabCmd::Active(true));
            }
        }
        if let Some(t) = self.tab_mut(id) {
            t.idle_since = None;
        }
        self.active = id;
        self.outbox = Outbox::new(id, self.srv.cfg.window, self.outbox.seq());
        self.send_tabs();
        if let Some(t) = self.tab(id) {
            self.send(ServerMsg::Title {
                tab: id,
                title: t.info.title.clone(),
            });
            self.send(ServerMsg::Url {
                tab: id,
                url: t.info.url.clone(),
            });
            self.send(ServerMsg::Cursor { tab: id, pos: None });
        }
    }

    /// Lean profile: close the browser target of tabs idle for too long, keep their URL/title.
    fn discard_idle(&mut self) {
        let secs = match self.srv.cfg.discard_after_secs {
            Some(0) => return,
            Some(s) => s,
            None => match self.profile.discard_after_secs {
                Some(s) => s,
                None => return,
            },
        };
        let limit = std::time::Duration::from_secs(secs);
        let active = self.active;
        for t in &mut self.tabs {
            if t.info.id != active
                && !t.discarded
                && t.idle_since.is_some_and(|i| i.elapsed() > limit)
            {
                t.handle.send(TabCmd::Close);
                t.discarded = true;
                t.info.loading = false;
                let srv = self.srv.clone();
                let target = t.target_id.clone();
                tokio::spawn(async move {
                    let _ = srv.browser.close_target(&target).await;
                });
            }
        }
    }

    async fn revive(&mut self, id: TabId) -> Result<()> {
        let (target_id, sess) = self.srv.browser.new_target(Some(&self.ctx)).await?;
        let cfg = self.tab_cfg();
        let url = self
            .tab(id)
            .map(|t| t.info.url.clone())
            .filter(|u| !u.is_empty() && self.srv.is_allowed_url(u));
        let handle = tab::spawn(id, sess, cfg, self.ev_tx.clone(), url);
        if let Some(t) = self.tab_mut(id) {
            t.handle = handle;
            t.target_id = target_id;
            t.discarded = false;
        }
        Ok(())
    }

    async fn close_tab(&mut self, id: TabId) {
        let Some(pos) = self.tabs.iter().position(|t| t.info.id == id) else {
            return;
        };
        let t = self.tabs.remove(pos);
        t.handle.send(TabCmd::Close);
        let srv = self.srv.clone();
        tokio::spawn(async move {
            let _ = srv.browser.close_target(&t.target_id).await;
        });
        if self.tabs.is_empty() {
            if let Err(e) = self.open_tab(None, None).await {
                self.send(ServerMsg::Error(format!("{e:#}")));
            }
        } else if self.active == id {
            let next = self.tabs[pos.saturating_sub(1).min(self.tabs.len() - 1)]
                .info
                .id;
            self.active = 0;
            self.activate(next).await;
        } else {
            self.send_tabs();
        }
    }

    async fn on_client(&mut self, m: ClientMsg) {
        use ClientMsg::*;
        let active = self.active;
        let to_active = |s: &Self, c: TabCmd| {
            if let Some(t) = s.tab(active) {
                t.handle.send(c);
            }
        };
        match m {
            Hello(_) => {}
            Key(k) => to_active(self, TabCmd::Key(k)),
            Mouse(e) => to_active(self, TabCmd::Mouse(e)),
            Resize { cols, rows } => {
                self.caps.cols = cols;
                self.caps.rows = rows;
                for t in &self.tabs {
                    t.handle.send(TabCmd::Resize { cols, rows });
                }
                self.outbox.force_full();
            }
            Navigate { url } => {
                if self.srv.is_allowed_url(&url) {
                    to_active(self, TabCmd::Navigate(url));
                } else {
                    self.send(ServerMsg::Error(format!(
                        "navigation to {url} is not allowed"
                    )));
                }
            }
            Back => to_active(self, TabCmd::Back),
            Forward => to_active(self, TabCmd::Forward),
            Reload => to_active(self, TabCmd::Reload),
            Stop => to_active(self, TabCmd::Stop),
            NewTab { url } => {
                if let Err(e) = self.open_tab(url, None).await {
                    self.send(ServerMsg::Error(format!("{e:#}")));
                }
            }
            CloseTab(id) => self.close_tab(id).await,
            SwitchTab(id) => self.activate(id).await,
            Scroll {
                unit,
                dx,
                dy,
                col,
                row,
            } => to_active(
                self,
                TabCmd::Scroll {
                    unit,
                    dx,
                    dy,
                    col,
                    row,
                },
            ),
            Paste(t) => to_active(self, TabCmd::Paste(t)),
            Find {
                query,
                forward,
                case_sensitive,
            } => to_active(
                self,
                TabCmd::Find {
                    query,
                    forward,
                    case_sensitive,
                },
            ),
            ClearFocus => to_active(self, TabCmd::ClearFocus),
            SetMode(mode) => {
                to_active(self, TabCmd::SetMode(mode));
                self.outbox.force_full();
            }
            Redraw => {
                self.outbox.force_full();
                to_active(self, TabCmd::Refresh);
            }
            Ack { tab, seq } => {
                if tab == self.active {
                    let (msgs, rtt) = self.outbox.ack(seq);
                    for m in msgs {
                        self.send(m);
                    }
                    if let Some(r) = rtt {
                        self.rate.update(r);
                        to_active(self, TabCmd::SetFps(self.rate.fps));
                    }
                }
            }
        }
    }

    fn on_tab_event(&mut self, id: TabId, e: TabEvent) {
        match e {
            TabEvent::Frame(r) => {
                if id == self.active {
                    for m in self.outbox.offer(r.grid, r.regions) {
                        self.send(m);
                    }
                }
            }
            TabEvent::Title(t) => {
                if let Some(s) = self.tab_mut(id) {
                    s.info.title = t.clone();
                }
                self.send(ServerMsg::Title { tab: id, title: t });
                self.send_tabs();
            }
            TabEvent::Url(u) => {
                if let Some(s) = self.tab_mut(id) {
                    s.info.url = u.clone();
                }
                self.send(ServerMsg::Url { tab: id, url: u });
                self.send_tabs();
            }
            TabEvent::Load(state) => {
                let changed = self.tab_mut(id).is_some_and(|s| {
                    std::mem::replace(&mut s.info.loading, state.loading) != state.loading
                });
                self.send(ServerMsg::LoadState { tab: id, state });
                if changed {
                    self.send_tabs();
                }
            }
            TabEvent::Cursor(pos) => {
                if id == self.active {
                    self.send(ServerMsg::Cursor { tab: id, pos });
                }
            }
            TabEvent::Clipboard(t) => self.send(ServerMsg::Clipboard(t)),
            TabEvent::FindResult(n) => self.send(ServerMsg::FindResult {
                tab: id,
                matches: n,
            }),
            TabEvent::Scroll(p) => {
                if id == self.active {
                    self.send(ServerMsg::Scroll {
                        tab: id,
                        permille: p,
                    });
                }
            }
            TabEvent::Mode(mode) => {
                if id == self.active {
                    self.outbox.force_full();
                    self.send(ServerMsg::Mode { tab: id, mode });
                }
            }
            TabEvent::Crashed => self.send(ServerMsg::Error(format!("tab {id} crashed"))),
        }
    }
}
