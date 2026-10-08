//! Wire messages. The same enums travel over in-process channels (`glyph local`) and over
//! WebSocket (`serve`/`connect`, see `codec`).

use serde::{Deserialize, Serialize};

use crate::diff::Run;

pub const PROTO_VERSION: u16 = 1;
pub type TabId = u32;

// ---------------------------------------------------------------- shared small types

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct CellRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl CellRect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && y >= self.y && x < self.x + self.w && y < self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RegionKind {
    Link,
    Button,
    Input,
    TextArea,
    Select,
    Checkbox,
    Radio,
    Other,
}

impl RegionKind {
    /// Does activating this region put the page into a text-entry state?
    pub fn is_text_entry(self) -> bool {
        matches!(self, RegionKind::Input | RegionKind::TextArea)
    }
}

/// Click/hint target. One region may span several rects (a link wrapped over lines).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Region {
    pub id: u32,
    pub kind: RegionKind,
    pub rects: Vec<CellRect>,
    pub href: Option<String>,
    pub label: String,
    pub value: Option<String>,
    /// Chromium backend node id (text mode clicks resolve through it).
    pub node: i64,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum GraphicsProto {
    #[default]
    None,
    Kitty,
    Sixel,
    Iterm2,
}

/// How the page is laid out for the terminal.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum PageStyle {
    /// Inject CSS so Chromium lays the page out in a monospace font whose advance is exactly one
    /// cell and with one-cell line height: text lands exactly in cells, lines never collide.
    #[default]
    Terminal,
    /// Leave the page's own fonts and line heights alone (text is placed by approximation).
    Faithful,
}

/// What the page should see for `prefers-color-scheme`.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum ColorScheme {
    #[default]
    Light,
    Dark,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub enum RenderMode {
    /// DOM snapshot + pixels.
    #[default]
    Pixel,
    /// Accessibility-tree reader view (cheapest).
    Text,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Profile {
    Lean,
    Balanced,
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Delete,
    Esc,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    F(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct Mods(pub u8);

impl Mods {
    pub const SHIFT: Mods = Mods(1);
    pub const CTRL: Mods = Mods(2);
    pub const ALT: Mods = Mods(4);
    pub const META: Mods = Mods(8);
    pub fn contains(self, o: Mods) -> bool {
        self.0 & o.0 == o.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub mods: Mods,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MouseKind {
    Down(MouseButton),
    Up(MouseButton),
    Move,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct MouseEvent {
    pub kind: MouseKind,
    pub col: u16,
    pub row: u16,
    pub mods: Mods,
    /// 1 = single click, 2 = double, 3 = triple.
    pub clicks: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ScrollUnit {
    /// Terminal rows (`CH` css px each).
    Lines,
    /// Viewport heights (PageUp/PageDown, space).
    Pages,
    /// Absolute: 0 = top, 1 = bottom (`gg`/`G`).
    Edge,
}

// ---------------------------------------------------------------- client → server

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClientCaps {
    /// Page content area in cells (excludes tab bar, omnibox, status bar).
    pub cols: u16,
    pub rows: u16,
    /// Pixel size of one terminal cell, for graphics protocols (0 = unknown).
    pub cell_px_w: u16,
    pub cell_px_h: u16,
    pub graphics: GraphicsProto,
    pub scheme: ColorScheme,
    pub page_style: PageStyle,
    /// Client wants image data / cannot live without images (disables lean image blocking).
    pub images: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub version: u16,
    pub token: Option<String>,
    pub caps: ClientCaps,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello(ClientHello),
    Key(KeyEvent),
    Mouse(MouseEvent),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// A resolved URL (the client turns omnibox text into a URL or a search URL).
    Navigate {
        url: String,
    },
    Back,
    Forward,
    Reload,
    Stop,
    NewTab {
        url: Option<String>,
    },
    CloseTab(TabId),
    SwitchTab(TabId),
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
    /// Frame `seq` of `tab` has been drawn.
    Ack {
        tab: TabId,
        seq: u64,
    },
    SetMode(RenderMode),
    /// Remove find highlight / selection / focus.
    ClearFocus,
    /// Throw away client state and resend a full frame (Ctrl-L, or after a glitch).
    Redraw,
}

// ---------------------------------------------------------------- server → client

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ServerHello {
    pub version: u16,
    pub session: String,
    pub profile: Profile,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TabInfo {
    pub id: TabId,
    pub title: String,
    pub url: String,
    pub loading: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug, Serialize, Deserialize)]
pub struct LoadState {
    pub loading: bool,
    /// 0..=100
    pub progress: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CursorState {
    pub col: u16,
    pub row: u16,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ImageFormat {
    Png,
    Jpeg,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ImageMsg {
    pub tab: TabId,
    /// Stable per placement slot; resending the same id replaces it.
    pub id: u32,
    pub rect: CellRect,
    pub px_w: u16,
    pub px_h: u16,
    pub format: ImageFormat,
    pub data: Vec<u8>,
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello(ServerHello),
    FullFrame {
        tab: TabId,
        seq: u64,
        cols: u16,
        rows: u16,
        runs: Vec<Run>,
    },
    /// Changes relative to frame `base`.
    Diff {
        tab: TabId,
        seq: u64,
        base: u64,
        runs: Vec<Run>,
    },
    Regions {
        tab: TabId,
        seq: u64,
        regions: Vec<Region>,
    },
    Title {
        tab: TabId,
        title: String,
    },
    Url {
        tab: TabId,
        url: String,
    },
    LoadState {
        tab: TabId,
        state: LoadState,
    },
    Tabs {
        active: TabId,
        tabs: Vec<TabInfo>,
    },
    /// Text caret in cell coordinates; `None` hides it.
    Cursor {
        tab: TabId,
        pos: Option<CursorState>,
    },
    /// Page-initiated clipboard write (`navigator.clipboard.writeText`, copy event).
    Clipboard(String),
    Image(ImageMsg),
    ImageClear {
        tab: TabId,
        ids: Vec<u32>,
    },
    FindResult {
        tab: TabId,
        matches: u32,
    },
    /// Vertical scroll position of the page, 0..=1000 (for the status bar).
    Scroll {
        tab: TabId,
        permille: u16,
    },
    Mode {
        tab: TabId,
        mode: RenderMode,
    },
    Error(String),
}
