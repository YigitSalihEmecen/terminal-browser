//! Client state machine. Pure: input events and server messages in, [`Effect`]s out. All drawing
//! lives in `ui`, all terminal I/O in `term`, so everything here is unit-testable.

use std::time::{Duration, Instant};

use glyph_proto::{
    apply_runs, width::str_width, ClientMsg, CursorState, Grid, KeyCode, KeyEvent, LoadState, Mods,
    MouseButton, MouseEvent, MouseKind, Region, RenderMode, ScrollUnit, ServerMsg, Style, TabId,
    TabInfo,
};

use crate::{
    config::Config,
    hints,
    keymap::{Action, Key, KeyMode, Lookup},
    omnibox::{resolve, LineEdit},
};

pub const TAB_ROW: u16 = 0;
pub const OMNI_ROW: u16 = 1;
pub const CONTENT_TOP: u16 = 2;

/// Page area for a terminal of `w × h` cells (tab bar, omnibox and status bar take 3 rows).
pub fn content_size(w: u16, h: u16) -> (u16, u16) {
    (w.max(1), h.saturating_sub(3).max(1))
}

#[derive(Clone, Debug, PartialEq)]
pub struct HintState {
    pub new_tab: bool,
    pub typed: String,
    /// (label, region id), labels lowercase.
    pub items: Vec<(String, u32)>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    Normal,
    Insert,
    Omnibox { new_tab: bool },
    Find,
    Hint(HintState),
    Visual,
    Help,
}

/// What the event loop must do after an update.
#[derive(Default, Debug)]
pub struct Out {
    pub send: Vec<ClientMsg>,
    pub copy: Vec<String>,
    pub quit: bool,
}

impl Out {
    fn msg(&mut self, m: ClientMsg) {
        self.send.push(m);
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MouseKindIn {
    Down(MouseButton),
    Up(MouseButton),
    Drag(MouseButton),
    Move,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MouseIn {
    pub kind: MouseKindIn,
    /// Terminal cell coordinates.
    pub x: u16,
    pub y: u16,
    pub mods: Mods,
}

/// A selection on the page grid (inclusive cell coordinates, any direction).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Selection {
    pub anchor: (u16, u16),
    pub head: (u16, u16),
}

impl Selection {
    /// `(start, end)` in reading order.
    pub fn ordered(&self) -> ((u16, u16), (u16, u16)) {
        let (a, h) = (self.anchor, self.head);
        if (a.1, a.0) <= (h.1, h.0) {
            (a, h)
        } else {
            (h, a)
        }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        let (s, e) = self.ordered();
        (y, x) >= (s.1, s.0) && (y, x) <= (e.1, e.0)
    }
}

pub struct TabBox {
    pub id: TabId,
    pub x0: u16,
    pub x1: u16,
    pub close_x: u16,
}

pub fn tab_label(index: usize, t: &TabInfo, max: usize) -> String {
    let name = if !t.title.is_empty() {
        t.title.as_str()
    } else if !t.url.is_empty() {
        t.url.as_str()
    } else {
        "New tab"
    };
    let mut label = String::new();
    let mut w = 0;
    for g in glyph_proto::width::clusters(name) {
        let gw = glyph_proto::width::cluster_width(g) as usize;
        if w + gw > max.saturating_sub(1) {
            label.push('…');
            break;
        }
        label.push_str(g);
        w += gw;
    }
    let spin = if t.loading { "⋯" } else { " " };
    format!(" {}{spin}{label} ", index + 1)
}

/// Hit boxes of the tab bar, and the x of the `+` button.
pub fn tab_boxes(tabs: &[TabInfo], width: u16) -> (Vec<TabBox>, u16) {
    let n = tabs.len().max(1);
    let per = ((width as usize).saturating_sub(5) / n).clamp(8, 26);
    let mut x = 0u16;
    let mut v = Vec::new();
    for (i, t) in tabs.iter().enumerate() {
        let label = tab_label(i, t, per.saturating_sub(5));
        let w = (str_width(&label) + 2) as u16; // label + " ×"
        v.push(TabBox {
            id: t.id,
            x0: x,
            x1: x + w,
            close_x: x + w - 1,
        });
        x += w;
    }
    (v, x)
}

pub struct App {
    pub cfg: Config,
    pub size: (u16, u16),
    pub mode: Mode,
    pub grid: Option<Grid>,
    pub seq: u64,
    pub regions: Vec<Region>,
    pub tabs: Vec<TabInfo>,
    pub active: TabId,
    pub load: LoadState,
    pub title: String,
    pub url: String,
    pub scroll_permille: u16,
    pub cursor: Option<CursorState>,
    pub omni: LineEdit,
    pub find: LineEdit,
    pub find_matches: Option<u32>,
    last_find: Option<String>,
    pub selection: Option<Selection>,
    press: Option<(u16, u16)>,
    last_click: Option<(Instant, (u16, u16), u8)>,
    pub hover: Option<(u16, u16)>,
    pub message: Option<(String, Instant)>,
    pub text_mode: bool,
    pending: Vec<Key>,
    pending_at: Instant,
    ack_due: Option<(TabId, u64)>,
    pub dirty: bool,
}

impl App {
    pub fn new(cfg: Config, size: (u16, u16)) -> Self {
        let warn = cfg.warnings.first().cloned();
        let mut app = Self {
            cfg,
            size,
            mode: Mode::Normal,
            grid: None,
            seq: 0,
            regions: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            load: LoadState::default(),
            title: String::new(),
            url: String::new(),
            scroll_permille: 0,
            cursor: None,
            omni: LineEdit::default(),
            find: LineEdit::default(),
            find_matches: None,
            last_find: None,
            selection: None,
            press: None,
            last_click: None,
            hover: None,
            message: None,
            text_mode: false,
            pending: Vec::new(),
            pending_at: Instant::now(),
            ack_due: None,
            dirty: true,
        };
        if let Some(w) = warn {
            app.say(format!("config: {w}"));
        }
        app
    }

    pub fn content(&self) -> (u16, u16) {
        content_size(self.size.0, self.size.1)
    }

    pub fn say(&mut self, s: impl Into<String>) {
        self.message = Some((s.into(), Instant::now()));
        self.dirty = true;
    }

    /// The ack to send after the frame just applied has been drawn.
    pub fn take_ack(&mut self) -> Option<ClientMsg> {
        self.ack_due
            .take()
            .map(|(tab, seq)| ClientMsg::Ack { tab, seq })
    }

    pub fn region_by_id(&self, id: u32) -> Option<&Region> {
        self.regions.iter().find(|r| r.id == id)
    }

    /// Region under page cell `(x, y)`, resolved through the cell's link id (so occlusion is honoured).
    pub fn region_at(&self, x: u16, y: u16) -> Option<&Region> {
        let g = self.grid.as_ref()?;
        if x >= g.cols() || y >= g.rows() {
            return None;
        }
        let id = g.cell(x, y).style.link;
        (id != 0).then(|| self.region_by_id(id)).flatten()
    }

    // ------------------------------------------------------------------ server messages

    pub fn on_server(&mut self, m: ServerMsg) -> Out {
        let mut out = Out::default();
        self.dirty = true;
        match m {
            ServerMsg::Hello(_) => {}
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
                self.ack_due = Some((tab, seq));
                self.selection = None;
            }
            ServerMsg::Diff {
                tab,
                seq,
                base,
                runs,
            } => match self.grid.as_mut() {
                Some(g) if base == self.seq => {
                    apply_runs(g, &runs);
                    self.seq = seq;
                    self.ack_due = Some((tab, seq));
                }
                _ => {
                    // lost sync (should not happen on an ordered transport): ask for a full frame
                    out.msg(ClientMsg::Redraw);
                }
            },
            ServerMsg::Regions { regions, .. } => self.regions = regions,
            ServerMsg::Title { tab, title } => {
                if tab == self.active {
                    self.title = title;
                }
            }
            ServerMsg::Url { tab, url } => {
                if tab == self.active {
                    self.url = url;
                }
            }
            ServerMsg::LoadState { tab, state } => {
                if tab == self.active {
                    self.load = state;
                }
            }
            ServerMsg::Tabs { active, tabs } => {
                if active != self.active {
                    self.selection = None;
                    self.find_matches = None;
                    self.cursor = None;
                    if self.mode == Mode::Insert {
                        self.mode = Mode::Normal;
                    }
                }
                self.active = active;
                if let Some(t) = tabs.iter().find(|t| t.id == active) {
                    self.title = t.title.clone();
                    self.url = t.url.clone();
                    self.load.loading = t.loading;
                }
                self.tabs = tabs;
            }
            ServerMsg::Cursor { pos, .. } => {
                self.cursor = pos;
                match (&self.mode, pos.is_some()) {
                    (Mode::Normal, true) => self.mode = Mode::Insert,
                    (Mode::Insert, false) => self.mode = Mode::Normal,
                    _ => {}
                }
            }
            ServerMsg::Clipboard(t) => {
                out.copy.push(t);
                self.say("copied from page");
            }
            ServerMsg::FindResult { matches, .. } => self.find_matches = Some(matches),
            ServerMsg::Scroll { permille, .. } => self.scroll_permille = permille,
            ServerMsg::Mode { mode, .. } => self.text_mode = mode == RenderMode::Text,
            ServerMsg::Error(e) => self.say(e),
            ServerMsg::Image(_) | ServerMsg::ImageClear { .. } => {}
        }
        out
    }

    // ------------------------------------------------------------------ keyboard

    pub fn on_key(&mut self, e: KeyEvent) -> Out {
        self.dirty = true;
        self.message = self
            .message
            .take()
            .filter(|(_, t)| t.elapsed() < Duration::from_secs(4));
        match self.mode.clone() {
            Mode::Normal => self.key_mapped(e, KeyMode::Normal),
            Mode::Insert => self.key_insert(e),
            Mode::Omnibox { new_tab } => self.key_omnibox(e, new_tab),
            Mode::Find => self.key_find(e),
            Mode::Hint(h) => self.key_hint(e, h),
            Mode::Visual => self.key_visual(e),
            Mode::Help => {
                self.mode = Mode::Normal;
                Out::default()
            }
        }
    }

    fn key_mapped(&mut self, e: KeyEvent, km: KeyMode) -> Out {
        let k = Key::from_event(e);
        self.pending.push(k);
        loop {
            match self.cfg.keymap.lookup(km, &self.pending) {
                Lookup::Action(a) => {
                    self.pending.clear();
                    return self.run(a);
                }
                Lookup::Pending => {
                    self.pending_at = Instant::now();
                    return Out::default();
                }
                Lookup::None => {
                    if self.pending.len() > 1 {
                        // "gx": 'g' was a false start; try 'x' on its own
                        self.pending = vec![k];
                        continue;
                    }
                    self.pending.clear();
                    return Out::default();
                }
            }
        }
    }

    fn key_insert(&mut self, e: KeyEvent) -> Out {
        let k = Key::from_event(e);
        if let Lookup::Action(a) = self.cfg.keymap.lookup(KeyMode::Insert, &[k]) {
            return self.run(a);
        }
        let mut out = Out::default();
        out.msg(ClientMsg::Key(e));
        out
    }

    fn key_omnibox(&mut self, e: KeyEvent, new_tab: bool) -> Out {
        let mut out = Out::default();
        let ctrl = e.mods.contains(Mods::CTRL);
        match (e.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                self.mode = Mode::Normal
            }
            (KeyCode::Enter, _) => {
                let url = resolve(&self.omni.text, &self.cfg.ui.search);
                self.mode = Mode::Normal;
                out.msg(if new_tab {
                    ClientMsg::NewTab { url: Some(url) }
                } else {
                    ClientMsg::Navigate { url }
                });
            }
            (KeyCode::Backspace, _) => self.omni.backspace(),
            (KeyCode::Delete, _) | (KeyCode::Char('d'), true) => self.omni.delete(),
            (KeyCode::Left, _) | (KeyCode::Char('b'), true) => self.omni.left(),
            (KeyCode::Right, _) | (KeyCode::Char('f'), true) => self.omni.right(),
            (KeyCode::Home, _) | (KeyCode::Char('a'), true) => self.omni.home(),
            (KeyCode::End, _) | (KeyCode::Char('e'), true) => self.omni.end(),
            (KeyCode::Char('u'), true) => self.omni.kill_to_start(),
            (KeyCode::Char('k'), true) => self.omni.kill_to_end(),
            (KeyCode::Char('w'), true) => self.omni.kill_word(),
            (KeyCode::Char(c), false) => self.omni.insert(c),
            _ => {}
        }
        out
    }

    fn smartcase(q: &str) -> bool {
        q.chars().any(char::is_uppercase)
    }

    fn find_msg(&mut self, forward: bool) -> Option<ClientMsg> {
        let q = self.last_find.clone()?;
        Some(ClientMsg::Find {
            case_sensitive: Self::smartcase(&q),
            query: q,
            forward,
        })
    }

    fn key_find(&mut self, e: KeyEvent) -> Out {
        let mut out = Out::default();
        let ctrl = e.mods.contains(Mods::CTRL);
        let before = self.find.text.clone();
        match (e.code, ctrl) {
            (KeyCode::Esc, _) | (KeyCode::Char('c'), true) => {
                self.mode = Mode::Normal;
                self.find_matches = None;
                self.last_find = None;
                out.msg(ClientMsg::ClearFocus);
            }
            (KeyCode::Enter, _) => {
                self.mode = Mode::Normal;
                if !self.find.text.is_empty() {
                    self.last_find = Some(self.find.text.clone());
                }
            }
            (KeyCode::Backspace, _) => self.find.backspace(),
            (KeyCode::Left, _) => self.find.left(),
            (KeyCode::Right, _) => self.find.right(),
            (KeyCode::Char('u'), true) => self.find.kill_to_start(),
            (KeyCode::Char('w'), true) => self.find.kill_word(),
            (KeyCode::Char(c), false) => self.find.insert(c),
            _ => {}
        }
        // incremental search: re-run on every edit
        if self.mode == Mode::Find && self.find.text != before {
            self.last_find = Some(self.find.text.clone()).filter(|s| !s.is_empty());
            if let Some(m) = self.find_msg(true) {
                out.msg(m);
            }
        }
        out
    }

    fn key_hint(&mut self, e: KeyEvent, mut h: HintState) -> Out {
        let out = Out::default();
        match e.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                return out;
            }
            KeyCode::Backspace => {
                h.typed.pop();
            }
            KeyCode::Char(c) if !e.mods.contains(Mods::CTRL) => {
                let mut t = h.typed.clone();
                t.push(c.to_ascii_lowercase());
                if h.items.iter().any(|(l, _)| l.starts_with(&t)) {
                    h.typed = t;
                }
            }
            _ => {}
        }
        if let Some((_, id)) = h.items.iter().find(|(l, _)| *l == h.typed).cloned() {
            self.mode = Mode::Normal;
            return self.activate_region(id, h.new_tab);
        }
        self.mode = Mode::Hint(h);
        out
    }

    fn key_visual(&mut self, e: KeyEvent) -> Out {
        let mut out = Out::default();
        let Some(g) = self.grid.as_ref() else {
            self.mode = Mode::Normal;
            return out;
        };
        let (cols, rows) = (g.cols(), g.rows());
        let mut sel = self.selection.unwrap_or(Selection {
            anchor: (0, 0),
            head: (0, 0),
        });
        let (mut x, mut y) = sel.head;
        match e.code {
            KeyCode::Esc | KeyCode::Char('v') | KeyCode::Char('q') => {
                self.mode = Mode::Normal;
                self.selection = None;
                return out;
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                if let Some(t) = self.selection_text() {
                    out.copy.push(t);
                    self.say("copied selection");
                }
                self.mode = Mode::Normal;
                self.selection = None;
                return out;
            }
            KeyCode::Char('h') | KeyCode::Left => x = x.saturating_sub(1),
            KeyCode::Char('l') | KeyCode::Right => x = (x + 1).min(cols - 1),
            KeyCode::Char('k') | KeyCode::Up => y = y.saturating_sub(1),
            KeyCode::Char('j') | KeyCode::Down => y = (y + 1).min(rows - 1),
            KeyCode::Char('0') | KeyCode::Home => x = 0,
            KeyCode::Char('$') | KeyCode::End => {
                x = (0..cols)
                    .rev()
                    .find(|&c| g.cell(c, y).g.trim() != "" && !g.cell(c, y).is_cont())
                    .unwrap_or(0);
            }
            KeyCode::Char('w') => x = next_word(g, x, y),
            KeyCode::Char('b') => x = prev_word(g, x, y),
            KeyCode::Char('G') => y = rows - 1,
            KeyCode::Char('g') => y = 0,
            _ => {}
        }
        sel.head = (x, y);
        self.selection = Some(sel);
        out
    }

    // ------------------------------------------------------------------ actions

    fn center(&self) -> (u16, u16) {
        let (c, r) = self.content();
        (c / 2, r / 2)
    }

    fn scroll(&self, unit: ScrollUnit, dx: i32, dy: i32) -> ClientMsg {
        let (col, row) = self.center();
        ClientMsg::Scroll {
            unit,
            dx,
            dy,
            col,
            row,
        }
    }

    pub fn run(&mut self, a: Action) -> Out {
        let mut out = Out::default();
        let n = self.cfg.ui.scroll_lines;
        let half = (self.content().1 / 2).max(1) as i32;
        match a {
            Action::Noop => {}
            Action::ScrollDown => out.msg(self.scroll(ScrollUnit::Lines, 0, n)),
            Action::ScrollUp => out.msg(self.scroll(ScrollUnit::Lines, 0, -n)),
            Action::ScrollLeft => out.msg(self.scroll(ScrollUnit::Lines, -n * 4, 0)),
            Action::ScrollRight => out.msg(self.scroll(ScrollUnit::Lines, n * 4, 0)),
            Action::HalfPageDown => out.msg(self.scroll(ScrollUnit::Lines, 0, half)),
            Action::HalfPageUp => out.msg(self.scroll(ScrollUnit::Lines, 0, -half)),
            Action::PageDown => out.msg(self.scroll(ScrollUnit::Pages, 0, 1)),
            Action::PageUp => out.msg(self.scroll(ScrollUnit::Pages, 0, -1)),
            Action::ScrollTop => out.msg(self.scroll(ScrollUnit::Edge, 0, -1)),
            Action::ScrollBottom => out.msg(self.scroll(ScrollUnit::Edge, 0, 1)),
            Action::Back => out.msg(ClientMsg::Back),
            Action::Forward => out.msg(ClientMsg::Forward),
            Action::Reload => out.msg(ClientMsg::Reload),
            Action::Stop => out.msg(ClientMsg::Stop),
            Action::Omnibox => {
                self.omni.set("");
                self.mode = Mode::Omnibox { new_tab: false };
            }
            Action::OmniboxCurrent => {
                let u = self.url.clone();
                self.omni.set(&u);
                self.mode = Mode::Omnibox { new_tab: false };
            }
            Action::OmniboxNewTab => {
                self.omni.set("");
                self.mode = Mode::Omnibox { new_tab: true };
            }
            Action::NewTab => out.msg(ClientMsg::NewTab {
                url: Some(self.cfg.ui.home.clone()).filter(|u| u != "about:blank"),
            }),
            Action::CloseTab => out.msg(ClientMsg::CloseTab(self.active)),
            Action::NextTab => self.cycle_tab(1, &mut out),
            Action::PrevTab => self.cycle_tab(-1, &mut out),
            Action::FirstTab => {
                if let Some(t) = self.tabs.first() {
                    out.msg(ClientMsg::SwitchTab(t.id));
                }
            }
            Action::LastTab => {
                if let Some(t) = self.tabs.last() {
                    out.msg(ClientMsg::SwitchTab(t.id));
                }
            }
            Action::GotoTab(n) => {
                if let Some(t) = self.tabs.get(n as usize - 1) {
                    out.msg(ClientMsg::SwitchTab(t.id));
                }
            }
            Action::Hints => self.start_hints(false, false),
            Action::HintsNewTab => self.start_hints(true, false),
            Action::HintsInput => self.start_hints(false, true),
            Action::Find => {
                self.find.set("");
                self.find_matches = None;
                self.mode = Mode::Find;
            }
            Action::FindNext => {
                if let Some(m) = self.find_msg(true) {
                    out.msg(m);
                } else {
                    self.say("no previous search");
                }
            }
            Action::FindPrev => {
                if let Some(m) = self.find_msg(false) {
                    out.msg(m);
                }
            }
            Action::Insert => self.mode = Mode::Insert,
            Action::Visual => {
                let (x, y) = self.hover.unwrap_or((0, 0));
                self.selection = Some(Selection {
                    anchor: (x, y),
                    head: (x, y),
                });
                self.mode = Mode::Visual;
            }
            Action::CopyUrl => {
                out.copy.push(self.url.clone());
                self.say("copied URL");
            }
            Action::CopyTitle => {
                out.copy.push(self.title.clone());
                self.say("copied title");
            }
            Action::ToggleTextMode => {
                self.text_mode = !self.text_mode;
                out.msg(ClientMsg::SetMode(if self.text_mode {
                    RenderMode::Text
                } else {
                    RenderMode::Pixel
                }));
                self.say(if self.text_mode {
                    "reader mode"
                } else {
                    "page mode"
                });
            }
            Action::Help => self.mode = Mode::Help,
            Action::Redraw => out.msg(ClientMsg::Redraw),
            Action::Escape => {
                self.selection = None;
                self.find_matches = None;
                self.mode = Mode::Normal;
                out.msg(ClientMsg::ClearFocus);
            }
            Action::Quit => out.quit = true,
        }
        self.dirty = true;
        out
    }

    fn cycle_tab(&self, d: i32, out: &mut Out) {
        if self.tabs.is_empty() {
            return;
        }
        let i = self
            .tabs
            .iter()
            .position(|t| t.id == self.active)
            .unwrap_or(0) as i32;
        let n = self.tabs.len() as i32;
        out.msg(ClientMsg::SwitchTab(
            self.tabs[((i + d).rem_euclid(n)) as usize].id,
        ));
    }

    fn start_hints(&mut self, new_tab: bool, input_only: bool) {
        let Some(g) = &self.grid else { return };
        let visible: Vec<&Region> = self
            .regions
            .iter()
            .filter(|r| {
                r.rects
                    .first()
                    .is_some_and(|c| c.x < g.cols() && c.y < g.rows())
            })
            .filter(|r| {
                !input_only || r.kind.is_text_entry() || r.kind == glyph_proto::RegionKind::Select
            })
            .collect();
        if visible.is_empty() {
            self.say(if input_only {
                "no input fields"
            } else {
                "no links"
            });
            return;
        }
        let labels = hints::labels(visible.len(), &self.cfg.ui.hint_chars);
        let items = visible
            .iter()
            .zip(labels)
            .map(|(r, l)| (l.to_lowercase(), r.id))
            .collect();
        self.mode = Mode::Hint(HintState {
            new_tab,
            typed: String::new(),
            items,
        });
    }

    /// Click (or open in a new tab) the region with `id`.
    pub fn activate_region(&mut self, id: u32, new_tab: bool) -> Out {
        let mut out = Out::default();
        let Some(r) = self.region_by_id(id).cloned() else {
            return out;
        };
        if let (true, Some(href)) = (new_tab, r.href.clone()) {
            out.msg(ClientMsg::NewTab { url: Some(href) });
            return out;
        }
        if let Some(c) = r.rects.first() {
            let (col, row) = (c.x + c.w / 2, c.y + c.h.saturating_sub(1) / 2);
            self.click(col, row, &mut out);
            if r.kind.is_text_entry() {
                self.mode = Mode::Insert;
            }
        }
        out
    }

    fn click(&self, col: u16, row: u16, out: &mut Out) {
        for kind in [
            MouseKind::Down(MouseButton::Left),
            MouseKind::Up(MouseButton::Left),
        ] {
            out.msg(ClientMsg::Mouse(MouseEvent {
                kind,
                col,
                row,
                mods: Mods::default(),
                clicks: 1,
            }));
        }
    }

    pub fn on_paste(&mut self, s: String) -> Out {
        self.dirty = true;
        let mut out = Out::default();
        match self.mode {
            Mode::Omnibox { .. } => self.omni.insert_str(&s),
            Mode::Find => self.find.insert_str(&s),
            Mode::Insert => out.msg(ClientMsg::Paste(s)),
            _ => {}
        }
        out
    }

    pub fn on_resize(&mut self, w: u16, h: u16) -> Out {
        self.size = (w, h);
        self.dirty = true;
        let (cols, rows) = self.content();
        let mut out = Out::default();
        out.msg(ClientMsg::Resize { cols, rows });
        out
    }

    pub fn on_tick(&mut self) {
        if let Some((_, t)) = &self.message {
            if t.elapsed() > Duration::from_secs(4) {
                self.message = None;
                self.dirty = true;
            }
        }
        if !self.pending.is_empty() && self.pending_at.elapsed() > Duration::from_millis(1200) {
            self.pending.clear();
        }
    }

    // ------------------------------------------------------------------ mouse

    pub fn on_mouse(&mut self, m: MouseIn) -> Out {
        let mut out = Out::default();
        self.dirty = true;
        let (cw, ch) = self.content();
        let in_content = m.y >= CONTENT_TOP && m.y < CONTENT_TOP + ch && m.x < cw;
        let cell = (m.x, m.y.saturating_sub(CONTENT_TOP));

        if m.y == TAB_ROW {
            if let MouseKindIn::Down(MouseButton::Left) | MouseKindIn::Down(MouseButton::Middle) =
                m.kind
            {
                let (boxes, plus) = tab_boxes(&self.tabs, self.size.0);
                if m.x >= plus && m.x < plus + 3 {
                    out.msg(ClientMsg::NewTab { url: None });
                } else if let Some(b) = boxes.iter().find(|b| m.x >= b.x0 && m.x < b.x1) {
                    if m.x == b.close_x || m.kind == MouseKindIn::Down(MouseButton::Middle) {
                        out.msg(ClientMsg::CloseTab(b.id));
                    } else {
                        out.msg(ClientMsg::SwitchTab(b.id));
                    }
                }
            }
            return out;
        }
        if m.y == OMNI_ROW {
            if m.kind == MouseKindIn::Down(MouseButton::Left) {
                let u = self.url.clone();
                self.omni.set(&u);
                self.mode = Mode::Omnibox { new_tab: false };
            }
            return out;
        }
        if !in_content {
            return out;
        }
        if !matches!(self.mode, Mode::Normal | Mode::Insert | Mode::Visual) {
            return out;
        }
        match m.kind {
            MouseKindIn::Move => self.hover = Some(cell),
            MouseKindIn::ScrollUp
            | MouseKindIn::ScrollDown
            | MouseKindIn::ScrollLeft
            | MouseKindIn::ScrollRight => {
                let n = self.cfg.ui.wheel_lines;
                let (dx, dy) = match m.kind {
                    MouseKindIn::ScrollUp => (0, -n),
                    MouseKindIn::ScrollDown => (0, n),
                    MouseKindIn::ScrollLeft => (-n * 4, 0),
                    _ => (n * 4, 0),
                };
                out.msg(ClientMsg::Scroll {
                    unit: ScrollUnit::Lines,
                    dx,
                    dy,
                    col: cell.0,
                    row: cell.1,
                });
            }
            MouseKindIn::Down(MouseButton::Left) => {
                self.press = Some(cell);
                self.selection = None;
                if self.mode == Mode::Visual {
                    self.mode = Mode::Normal;
                }
            }
            MouseKindIn::Drag(MouseButton::Left) => {
                if let Some(p) = self.press {
                    if p != cell || self.selection.is_some() {
                        self.selection = Some(Selection {
                            anchor: p,
                            head: cell,
                        });
                    }
                }
            }
            MouseKindIn::Up(MouseButton::Left) => {
                let pressed = self.press.take();
                if self.selection.is_some() {
                    if let Some(t) = self.selection_text() {
                        out.copy.push(t);
                        self.say("copied selection");
                    }
                } else if pressed == Some(cell) {
                    self.left_click(cell, &mut out);
                }
            }
            MouseKindIn::Down(MouseButton::Middle) => {
                if let Some(href) = self.region_at(cell.0, cell.1).and_then(|r| r.href.clone()) {
                    out.msg(ClientMsg::NewTab { url: Some(href) });
                }
            }
            _ => {}
        }
        out
    }

    fn left_click(&mut self, cell: (u16, u16), out: &mut Out) {
        let now = Instant::now();
        let count = match self.last_click {
            Some((t, c, n)) if c == cell && t.elapsed() < Duration::from_millis(450) => (n % 3) + 1,
            _ => 1,
        };
        self.last_click = Some((now, cell, count));
        match count {
            1 => {
                self.click(cell.0, cell.1, out);
                if self
                    .region_at(cell.0, cell.1)
                    .is_some_and(|r| r.kind.is_text_entry())
                {
                    self.mode = Mode::Insert;
                }
            }
            2 => {
                if let Some(g) = &self.grid {
                    let (x0, x1) = word_bounds(g, cell.0, cell.1);
                    self.selection = Some(Selection {
                        anchor: (x0, cell.1),
                        head: (x1, cell.1),
                    });
                }
            }
            _ => {
                if let Some(g) = &self.grid {
                    let end = (0..g.cols())
                        .rev()
                        .find(|&c| !g.cell(c, cell.1).g.trim().is_empty())
                        .unwrap_or(0);
                    self.selection = Some(Selection {
                        anchor: (0, cell.1),
                        head: (end, cell.1),
                    });
                }
            }
        }
        if count >= 2 {
            if let Some(t) = self.selection_text() {
                out.copy.push(t);
                self.say("copied selection");
            }
        }
    }

    // ------------------------------------------------------------------ selection

    pub fn selection_text(&self) -> Option<String> {
        let g = self.grid.as_ref()?;
        let sel = self.selection?;
        let (s, e) = sel.ordered();
        let mut lines = Vec::new();
        for y in s.1..=e.1.min(g.rows().saturating_sub(1)) {
            let x0 = if y == s.1 { s.0 } else { 0 };
            let x1 = if y == e.1 {
                e.0
            } else {
                g.cols().saturating_sub(1)
            };
            let mut line = String::new();
            for x in x0..=x1.min(g.cols().saturating_sub(1)) {
                line.push_str(&g.cell(x, y).g);
            }
            lines.push(line.trim_end().to_owned());
        }
        let text = lines.join("\n");
        (!text.trim().is_empty()).then_some(text)
    }
}

fn is_blank(g: &Grid, x: u16, y: u16) -> bool {
    let c = g.cell(x, y);
    !c.is_cont() && c.g.trim().is_empty()
}

fn word_bounds(g: &Grid, x: u16, y: u16) -> (u16, u16) {
    if y >= g.rows() || x >= g.cols() {
        return (x, x);
    }
    let (mut a, mut b) = (x, x);
    while a > 0 && !is_blank(g, a - 1, y) {
        a -= 1;
    }
    while b + 1 < g.cols() && !is_blank(g, b + 1, y) {
        b += 1;
    }
    (a, b)
}

fn next_word(g: &Grid, mut x: u16, y: u16) -> u16 {
    while x + 1 < g.cols() && !is_blank(g, x, y) {
        x += 1;
    }
    while x + 1 < g.cols() && is_blank(g, x, y) {
        x += 1;
    }
    x
}

fn prev_word(g: &Grid, mut x: u16, y: u16) -> u16 {
    while x > 0 && is_blank(g, x - 1, y) {
        x -= 1;
    }
    while x > 0 && !is_blank(g, x - 1, y) {
        x -= 1;
    }
    x
}

#[cfg(test)]
mod tests {
    use glyph_proto::{CellRect, RegionKind};

    use super::*;

    fn app() -> App {
        let mut a = App::new(Config::default(), (80, 24));
        a.on_server(ServerMsg::Tabs {
            active: 1,
            tabs: vec![TabInfo {
                id: 1,
                title: "T".into(),
                url: "https://a.org/".into(),
                loading: false,
            }],
        });
        a
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent {
            code: KeyCode::Char(c),
            mods: Mods::default(),
        }
    }

    fn special(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            mods: Mods::default(),
        }
    }

    fn frame(a: &mut App, lines: &[&str]) {
        let mut g = Grid::new(80, 21, Style::default());
        for (y, l) in lines.iter().enumerate() {
            g.put_str(0, y as u16, l, Style::default(), 80);
        }
        let runs = glyph_proto::full_runs(&g);
        a.on_server(ServerMsg::FullFrame {
            tab: 1,
            seq: 1,
            cols: 80,
            rows: 21,
            runs,
        });
    }

    fn link(id: u32, x: u16, y: u16, href: &str) -> Region {
        Region {
            id,
            kind: RegionKind::Link,
            rects: vec![CellRect { x, y, w: 4, h: 1 }],
            href: Some(href.into()),
            label: "l".into(),
            value: None,
            node: id as i64,
        }
    }

    #[test]
    fn scroll_keys_send_scroll_messages() {
        let mut a = app();
        let o = a.on_key(key('j'));
        assert!(matches!(
            o.send[0],
            ClientMsg::Scroll {
                unit: ScrollUnit::Lines,
                dy: 2,
                ..
            }
        ));
        a.on_key(key('g'));
        let o = a.on_key(key('g'));
        assert!(matches!(
            o.send[0],
            ClientMsg::Scroll {
                unit: ScrollUnit::Edge,
                dy: -1,
                ..
            }
        ));
        let o = a.on_key(key('G'));
        assert!(matches!(
            o.send[0],
            ClientMsg::Scroll {
                unit: ScrollUnit::Edge,
                dy: 1,
                ..
            }
        ));
    }

    #[test]
    fn false_prefix_retries_last_key() {
        let mut a = app();
        a.on_key(key('g'));
        let o = a.on_key(key('j')); // "gj" is nothing; 'j' alone scrolls
        assert!(matches!(o.send[0], ClientMsg::Scroll { dy: 2, .. }));
    }

    #[test]
    fn omnibox_flow_resolves_search_and_urls() {
        let mut a = app();
        a.on_key(key('o'));
        assert_eq!(a.mode, Mode::Omnibox { new_tab: false });
        for c in "rust lang".chars() {
            a.on_key(key(c));
        }
        let o = a.on_key(special(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        assert!(
            matches!(&o.send[0], ClientMsg::Navigate { url } if url == "https://duckduckgo.com/?q=rust%20lang")
        );
        a.on_key(key('t'));
        for c in "example.org".chars() {
            a.on_key(key(c));
        }
        let o = a.on_key(special(KeyCode::Enter));
        assert!(
            matches!(&o.send[0], ClientMsg::NewTab { url: Some(u) } if u == "https://example.org")
        );
        // Esc cancels without sending
        a.on_key(key('o'));
        let o = a.on_key(special(KeyCode::Esc));
        assert!(o.send.is_empty() && a.mode == Mode::Normal);
    }

    #[test]
    fn capital_o_prefills_current_url() {
        let mut a = app();
        a.on_key(key('O'));
        assert_eq!(a.omni.text, "https://a.org/");
    }

    #[test]
    fn hints_label_regions_and_click_the_center() {
        let mut a = app();
        frame(&mut a, &["", "  link1", "", "  link2"]);
        a.on_server(ServerMsg::Regions {
            tab: 1,
            seq: 1,
            regions: vec![link(1, 2, 1, "https://x/1"), link(2, 2, 3, "https://x/2")],
        });
        a.on_key(key('f'));
        let Mode::Hint(h) = a.mode.clone() else {
            panic!("not in hint mode")
        };
        assert_eq!(h.items.len(), 2);
        let second = h.items[1].0.clone();
        let mut sent = vec![];
        for c in second.chars() {
            sent = a.on_key(key(c)).send;
        }
        assert_eq!(a.mode, Mode::Normal);
        assert!(
            matches!(
                sent[0],
                ClientMsg::Mouse(MouseEvent {
                    kind: MouseKind::Down(_),
                    col: 4,
                    row: 3,
                    ..
                })
            ),
            "{sent:?}"
        );
        assert!(matches!(
            sent[1],
            ClientMsg::Mouse(MouseEvent {
                kind: MouseKind::Up(_),
                ..
            })
        ));
    }

    #[test]
    fn shifted_hints_open_new_tab_with_href() {
        let mut a = app();
        frame(&mut a, &["  link1"]);
        a.on_server(ServerMsg::Regions {
            tab: 1,
            seq: 1,
            regions: vec![link(1, 2, 0, "https://x/1")],
        });
        a.on_key(key('F'));
        let Mode::Hint(h) = a.mode.clone() else {
            panic!()
        };
        let o = a.on_key(key(h.items[0].0.chars().next().unwrap()));
        assert!(matches!(&o.send[0], ClientMsg::NewTab { url: Some(u) } if u == "https://x/1"));
    }

    #[test]
    fn hint_mode_ignores_chars_that_match_nothing_and_esc_cancels() {
        let mut a = app();
        frame(&mut a, &["  link1", "  link2"]);
        a.on_server(ServerMsg::Regions {
            tab: 1,
            seq: 1,
            regions: vec![link(1, 2, 0, "h"), link(2, 2, 1, "h")],
        });
        a.on_key(key('f'));
        let o = a.on_key(key('1')); // not in the alphabet
        assert!(o.send.is_empty());
        assert!(matches!(&a.mode, Mode::Hint(h) if h.typed.is_empty()));
        a.on_key(special(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn text_entry_hint_enters_insert_mode() {
        let mut a = app();
        frame(&mut a, &["[____]"]);
        let r = Region {
            id: 1,
            kind: RegionKind::Input,
            rects: vec![CellRect {
                x: 0,
                y: 0,
                w: 6,
                h: 1,
            }],
            href: None,
            label: String::new(),
            value: None,
            node: 1,
        };
        a.on_server(ServerMsg::Regions {
            tab: 1,
            seq: 1,
            regions: vec![r],
        });
        a.on_key(key('g'));
        a.on_key(key('i'));
        assert!(matches!(a.mode, Mode::Hint(_)));
        let Mode::Hint(h) = a.mode.clone() else {
            panic!()
        };
        a.on_key(key(h.items[0].0.chars().next().unwrap()));
        assert_eq!(a.mode, Mode::Insert);
    }

    #[test]
    fn insert_mode_forwards_keys_and_escape_leaves() {
        let mut a = app();
        a.on_key(key('i'));
        assert_eq!(a.mode, Mode::Insert);
        let o = a.on_key(key('j'));
        assert!(matches!(
            o.send[0],
            ClientMsg::Key(KeyEvent {
                code: KeyCode::Char('j'),
                ..
            })
        ));
        let o = a.on_key(special(KeyCode::Esc));
        assert_eq!(a.mode, Mode::Normal);
        assert!(matches!(o.send[0], ClientMsg::ClearFocus));
    }

    #[test]
    fn page_focus_drives_mode() {
        let mut a = app();
        a.on_server(ServerMsg::Cursor {
            tab: 1,
            pos: Some(CursorState { col: 3, row: 1 }),
        });
        assert_eq!(a.mode, Mode::Insert);
        a.on_server(ServerMsg::Cursor { tab: 1, pos: None });
        assert_eq!(a.mode, Mode::Normal);
    }

    #[test]
    fn find_is_incremental_and_smartcase() {
        let mut a = app();
        a.on_key(key('/'));
        let o = a.on_key(key('f'));
        assert!(
            matches!(&o.send[0], ClientMsg::Find { query, case_sensitive: false, forward: true } if query == "f")
        );
        let o = a.on_key(key('O'));
        assert!(
            matches!(&o.send[0], ClientMsg::Find { query, case_sensitive: true, .. } if query == "fO")
        );
        a.on_key(special(KeyCode::Enter));
        assert_eq!(a.mode, Mode::Normal);
        let o = a.on_key(key('N'));
        assert!(matches!(&o.send[0], ClientMsg::Find { forward: false, .. }));
    }

    #[test]
    fn diff_with_wrong_base_requests_redraw() {
        let mut a = app();
        frame(&mut a, &["x"]);
        let o = a.on_server(ServerMsg::Diff {
            tab: 1,
            seq: 9,
            base: 7,
            runs: vec![],
        });
        assert!(matches!(o.send[0], ClientMsg::Redraw));
        // the right base applies and queues an ack
        let o = a.on_server(ServerMsg::Diff {
            tab: 1,
            seq: 2,
            base: 1,
            runs: vec![],
        });
        assert!(o.send.is_empty());
        assert!(matches!(
            a.take_ack(),
            Some(ClientMsg::Ack { tab: 1, seq: 2 })
        ));
        assert!(a.take_ack().is_none());
    }

    fn mouse(kind: MouseKindIn, x: u16, y: u16) -> MouseIn {
        MouseIn {
            kind,
            x,
            y,
            mods: Mods::default(),
        }
    }

    #[test]
    fn click_sends_down_up_but_drag_selects_and_copies() {
        let mut a = app();
        frame(&mut a, &["hello wide 日本語 text"]);
        // click on content row 0 == terminal row 2
        a.on_mouse(mouse(MouseKindIn::Down(MouseButton::Left), 3, 2));
        let o = a.on_mouse(mouse(MouseKindIn::Up(MouseButton::Left), 3, 2));
        assert_eq!(o.send.len(), 2);
        assert!(matches!(
            o.send[0],
            ClientMsg::Mouse(MouseEvent { col: 3, row: 0, .. })
        ));

        // drag across "wide 日本語"
        a.on_mouse(mouse(MouseKindIn::Down(MouseButton::Left), 6, 2));
        a.on_mouse(mouse(MouseKindIn::Drag(MouseButton::Left), 16, 2));
        let o = a.on_mouse(mouse(MouseKindIn::Up(MouseButton::Left), 16, 2));
        assert!(o.send.is_empty(), "a drag must not click the page");
        assert_eq!(o.copy, vec!["wide 日本語".to_string()]);
    }

    #[test]
    fn selection_across_lines_trims_and_joins() {
        let mut a = app();
        frame(&mut a, &["first line   ", "second line"]);
        a.selection = Some(Selection {
            anchor: (6, 0),
            head: (5, 1),
        });
        assert_eq!(a.selection_text().unwrap(), "line\nsecond");
        // reversed direction yields the same text
        a.selection = Some(Selection {
            anchor: (5, 1),
            head: (6, 0),
        });
        assert_eq!(a.selection_text().unwrap(), "line\nsecond");
    }

    #[test]
    fn double_click_selects_word_and_copies() {
        let mut a = app();
        frame(&mut a, &["alpha beta gamma"]);
        let down = mouse(MouseKindIn::Down(MouseButton::Left), 8, 2);
        let up = mouse(MouseKindIn::Up(MouseButton::Left), 8, 2);
        a.on_mouse(down);
        a.on_mouse(up);
        a.on_mouse(down);
        let o = a.on_mouse(up);
        assert_eq!(o.copy, vec!["beta".to_string()]);
        assert!(
            o.send.is_empty(),
            "second click of a double click is client-side only"
        );
    }

    #[test]
    fn wheel_scrolls_at_the_pointer() {
        let mut a = app();
        let o = a.on_mouse(mouse(MouseKindIn::ScrollDown, 10, 5));
        assert!(matches!(
            o.send[0],
            ClientMsg::Scroll {
                unit: ScrollUnit::Lines,
                dy: 3,
                col: 10,
                row: 3,
                ..
            }
        ));
    }

    #[test]
    fn tab_bar_clicks() {
        let mut a = app();
        a.on_server(ServerMsg::Tabs {
            active: 1,
            tabs: vec![
                TabInfo {
                    id: 1,
                    title: "One".into(),
                    url: String::new(),
                    loading: false,
                },
                TabInfo {
                    id: 2,
                    title: "Two".into(),
                    url: String::new(),
                    loading: false,
                },
            ],
        });
        let (boxes, plus) = tab_boxes(&a.tabs, 80);
        let o = a.on_mouse(mouse(
            MouseKindIn::Down(MouseButton::Left),
            boxes[1].x0 + 1,
            0,
        ));
        assert!(matches!(o.send[0], ClientMsg::SwitchTab(2)));
        let o = a.on_mouse(mouse(
            MouseKindIn::Down(MouseButton::Left),
            boxes[1].close_x,
            0,
        ));
        assert!(matches!(o.send[0], ClientMsg::CloseTab(2)));
        let o = a.on_mouse(mouse(MouseKindIn::Down(MouseButton::Left), plus + 1, 0));
        assert!(matches!(o.send[0], ClientMsg::NewTab { url: None }));
    }

    #[test]
    fn tab_navigation_wraps() {
        let mut a = app();
        a.on_server(ServerMsg::Tabs {
            active: 2,
            tabs: vec![
                TabInfo {
                    id: 1,
                    title: String::new(),
                    url: String::new(),
                    loading: false,
                },
                TabInfo {
                    id: 2,
                    title: String::new(),
                    url: String::new(),
                    loading: false,
                },
            ],
        });
        let o = a.on_key(key('K'));
        assert!(matches!(o.send[0], ClientMsg::SwitchTab(1)));
        let o = a.on_key(KeyEvent {
            code: KeyCode::Char('1'),
            mods: Mods::ALT,
        });
        assert!(matches!(o.send[0], ClientMsg::SwitchTab(1)));
    }

    #[test]
    fn resize_reports_content_area() {
        let mut a = app();
        let o = a.on_resize(100, 40);
        assert!(matches!(
            o.send[0],
            ClientMsg::Resize {
                cols: 100,
                rows: 37
            }
        ));
    }

    #[test]
    fn visual_mode_selects_and_yanks() {
        let mut a = app();
        frame(&mut a, &["one two three"]);
        a.hover = Some((4, 0));
        a.on_key(key('v'));
        a.on_key(key('e')); // unknown keys are ignored
        a.on_key(key('w'));
        let o = a.on_key(key('y'));
        assert_eq!(a.mode, Mode::Normal);
        assert_eq!(o.copy, vec!["two t".to_string()]);
    }

    #[test]
    fn paste_goes_where_the_focus_is() {
        let mut a = app();
        a.on_key(key('o'));
        a.on_paste("example.org".into());
        assert_eq!(a.omni.text, "example.org");
        a.on_key(special(KeyCode::Esc));
        a.on_key(key('i'));
        let o = a.on_paste("hi".into());
        assert!(matches!(&o.send[0], ClientMsg::Paste(t) if t == "hi"));
        a.on_key(special(KeyCode::Esc));
        assert!(a.on_paste("ignored".into()).send.is_empty());
    }

    #[test]
    fn page_clipboard_writes_are_forwarded_to_the_terminal() {
        let mut a = app();
        let o = a.on_server(ServerMsg::Clipboard("from page".into()));
        assert_eq!(o.copy, vec!["from page".to_string()]);
    }
}
