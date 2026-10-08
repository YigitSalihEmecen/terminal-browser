//! DOMSnapshot (+ optional screencast pixels) → character-cell grid. See DESIGN.md
//! "Cell-mapping algorithm"; the numbered comments below follow that list.

use std::collections::HashMap;

use glyph_proto::{
    width::{cluster_width, clusters, str_width},
    Attrs, CellRect, Grid, Region, RegionKind, Rgb, Style,
};

use crate::{
    colour::{self, BoxRec, Kind},
    pixmap::Pixmap,
    snapshot::{parse_color, parse_px, st, DocView, SnapshotResult},
};

#[derive(Clone, Copy, Debug)]
pub struct Metrics {
    pub cols: u16,
    pub rows: u16,
    /// CSS pixels per cell.
    pub cw: f64,
    pub ch: f64,
}

impl Metrics {
    pub fn px_w(&self) -> f64 {
        self.cols as f64 * self.cw
    }
    pub fn px_h(&self) -> f64 {
        self.rows as f64 * self.ch
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PageInfo {
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub content_w: f64,
    pub content_h: f64,
}

#[derive(Clone)]
pub struct Rendered {
    pub grid: Grid,
    pub regions: Vec<Region>,
    pub images: Vec<CellRect>,
    /// `<video>` / `<canvas>` rectangles: pixels that change without the DOM changing.
    pub live: Vec<CellRect>,
    pub page: PageInfo,
}

// ---------------------------------------------------------------- geometry

#[derive(Clone, Copy, Debug)]
struct Rf {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

const INF: f64 = 1e12;
const UNBOUNDED: Rf = Rf {
    x0: -INF,
    y0: -INF,
    x1: INF,
    y1: INF,
};

impl Rf {
    fn isect(self, o: Rf) -> Rf {
        Rf {
            x0: self.x0.max(o.x0),
            y0: self.y0.max(o.y0),
            x1: self.x1.min(o.x1),
            y1: self.y1.min(o.y1),
        }
    }
    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }
    fn overlaps(self, o: Rf) -> bool {
        !self.isect(o).is_empty()
    }
}

/// Cell rectangle `[c0,c1) × [r0,r1)` (exclusive end), possibly outside the grid.
#[derive(Clone, Copy, Debug, PartialEq)]
struct CRect {
    c0: i32,
    r0: i32,
    c1: i32,
    r1: i32,
}

impl CRect {
    fn clamp(self, cols: u16, rows: u16) -> CRect {
        CRect {
            c0: self.c0.max(0),
            r0: self.r0.max(0),
            c1: self.c1.min(cols as i32),
            r1: self.r1.min(rows as i32),
        }
    }
    fn is_empty(self) -> bool {
        self.c1 <= self.c0 || self.r1 <= self.r0
    }
    fn to_cell_rect(self) -> CellRect {
        CellRect {
            x: self.c0 as u16,
            y: self.r0 as u16,
            w: (self.c1 - self.c0) as u16,
            h: (self.r1 - self.r0) as u16,
        }
    }
}

fn cell_rect(m: &Metrics, r: Rf) -> CRect {
    CRect {
        c0: (r.x0 / m.cw).round() as i32,
        r0: (r.y0 / m.ch).round() as i32,
        c1: (r.x1 / m.cw).round() as i32,
        r1: (r.y1 / m.ch).round() as i32,
    }
}

// ---------------------------------------------------------------- items

type OwnerKey = (usize, usize); // (document, node)

enum K {
    Fill {
        r: CRect,
        color: Rgb,
        alpha: u8,
    },
    Border {
        px: Rf,
        r: CRect,
        color: Rgb,
        sides: [bool; 4],
        round: bool,
    },
    Image {
        r: CRect,
        owner: Option<OwnerKey>,
    },
    LinkMark {
        r: CRect,
        owner: OwnerKey,
    },
    Glyphs(Glyphs),
}

struct Glyphs {
    row: i32,
    /// Pixel-derived start column.
    px_col: i32,
    /// Pixel x extent in viewport space (for chaining).
    x0: f64,
    x1: f64,
    /// Final start column (after chaining).
    col: i32,
    text: String,
    fg: Rgb,
    fg_alpha: u8,
    attrs: Attrs,
    owner: Option<OwnerKey>,
    /// Form-control text: fixed width, never truncated or chained.
    fixed: bool,
    /// Cells this run owns (its text extent, rows included).
    zone: CRect,
    /// Effective CSS background behind the run (ancestors composited).
    css_bg: Rgb,
}

struct Item {
    po: (i32, i32),
    seq: u32,
    k: K,
}

impl Item {
    fn order(&self) -> u8 {
        match self.k {
            K::Fill { .. } => 0,
            K::Border { .. } => 1,
            K::Image { .. } => 2,
            K::LinkMark { .. } => 3,
            K::Glyphs(_) => 4,
        }
    }
}

struct Owner {
    kind: RegionKind,
    href: Option<String>,
    attr_label: String,
    text_label: String,
    value: Option<String>,
    backend: i64,
    rects: Vec<CellRect>,
    /// Layout bounds in cells, used when no text rect exists.
    fallback: Option<CRect>,
    po: (i32, i32),
}

#[derive(Clone, Copy, PartialEq)]
enum Pos {
    Static,
    Fixed,
    Absolute,
    Other,
}

struct Ctx<'a> {
    m: &'a Metrics,
    items: Vec<Item>,
    owners: HashMap<OwnerKey, Owner>,
    canvas: Option<Rgb>,
    page: PageInfo,
    seq: u32,
    live: Vec<CellRect>,
}

/// Render a snapshot. `pix` (the screencast frame) supplies backgrounds and image halves.
pub fn render(snap: &SnapshotResult, pix: Option<&Pixmap>, m: &Metrics) -> Rendered {
    let mut cx = Ctx {
        m,
        items: Vec::new(),
        owners: HashMap::new(),
        canvas: None,
        page: PageInfo::default(),
        seq: 0,
        live: Vec::new(),
    };
    if !snap.documents.is_empty() {
        let vp = Rf {
            x0: 0.0,
            y0: 0.0,
            x1: m.px_w(),
            y1: m.px_h(),
        };
        cx.collect_doc(snap, 0, (0.0, 0.0), vp, 0, 0);
    }
    cx.finish(pix)
}

impl Ctx<'_> {
    fn next_seq(&mut self) -> u32 {
        self.seq += 1;
        self.seq
    }

    /// Steps 1–3 (+ owner discovery) for one document; recurses into iframes.
    fn collect_doc(
        &mut self,
        snap: &SnapshotResult,
        di: usize,
        off: (f64, f64),
        base_clip: Rf,
        po_major: i32,
        depth: u32,
    ) {
        let Some(doc) = snap.documents.get(di) else {
            return;
        };
        let v = DocView::new(&snap.strings, doc);
        let n = doc.nodes.parent.len();
        let lay = &doc.layout;
        if di == 0 {
            self.page = PageInfo {
                scroll_x: doc.scroll_x,
                scroll_y: doc.scroll_y,
                content_w: doc.content_w,
                content_h: doc.content_h,
            };
        }
        let shift = (off.0 - doc.scroll_x, off.1 - doc.scroll_y);
        let vb = |li: usize| -> Option<Rf> {
            let b = v.bounds(li)?;
            Some(Rf {
                x0: b[0] + shift.0,
                y0: b[1] + shift.1,
                x1: b[0] + b[2] + shift.0,
                y1: b[1] + b[3] + shift.1,
            })
        };
        let po_of = |li: usize| -> (i32, i32) {
            let p = lay.paint_orders.get(li).copied().unwrap_or(0);
            if depth == 0 {
                (p, 0)
            } else {
                (po_major, 1 + p)
            }
        };

        let mut node_layout = vec![-1i32; n];
        for (li, &ni) in lay.node_index.iter().enumerate() {
            if ni >= 0 && (ni as usize) < n {
                node_layout[ni as usize] = li as i32;
            }
        }

        // First text child of TEXTAREA/OPTION (their content is not in text boxes).
        let mut first_text: HashMap<usize, usize> = HashMap::new();
        for j in 0..n {
            if doc.nodes.node_type.get(j) == Some(&3) {
                let p = doc.nodes.parent[j];
                if p >= 0 && matches!(v.tag(p as usize), "TEXTAREA" | "OPTION") {
                    first_text.entry(p as usize).or_insert(j);
                }
            }
        }

        // 1. per-node pass
        let mut paint_ok = vec![false; n];
        let mut opacity = vec![1.0f32; n];
        let mut clip = vec![base_clip; n];
        let mut pos_clip = vec![base_clip; n];
        let mut owner_of = vec![usize::MAX; n];
        let mut eff_bg = vec![Rgb::WHITE; n];
        for i in 0..n {
            let p = doc.nodes.parent[i];
            let (pc, ppc, pop, pown) = if p >= 0 {
                let p = p as usize;
                (clip[p], pos_clip[p], opacity[p], owner_of[p])
            } else {
                (base_clip, base_clip, 1.0, usize::MAX)
            };
            let li = node_layout[i];
            let tag = v.tag(i);
            let is_root = matches!(tag, "HTML" | "BODY");
            let mut out = pc;
            let mut pc_here = ppc;
            let mut op = pop;
            if li >= 0 {
                let l = li as usize;
                let own_op: f32 = v.style(l, st::OPACITY).parse().unwrap_or(1.0);
                op = pop * own_op;
                paint_ok[i] = v.style(l, st::VISIBILITY) != "hidden"
                    && v.style(l, st::VISIBILITY) != "collapse"
                    && op > 0.02;
                let pos = match v.style(l, st::POSITION) {
                    "fixed" => Pos::Fixed,
                    "absolute" => Pos::Absolute,
                    "static" | "" => Pos::Static,
                    _ => Pos::Other,
                };
                let inherited = match pos {
                    Pos::Fixed => base_clip,
                    Pos::Absolute => ppc,
                    _ => pc,
                };
                out = inherited;
                if !is_root {
                    if let Some(b) = vb(l) {
                        let (ox, oy) = (v.style(l, st::OVERFLOW_X), v.style(l, st::OVERFLOW_Y));
                        let clips = |o: &str| !matches!(o, "visible" | "");
                        let mut c = UNBOUNDED;
                        if clips(ox) {
                            c.x0 = b.x0;
                            c.x1 = b.x1;
                        }
                        if clips(oy) {
                            c.y0 = b.y0;
                            c.y1 = b.y1;
                        }
                        out = inherited.isect(c);
                    }
                }
                if pos != Pos::Static {
                    pc_here = out;
                }
            }
            let parent_bg = if p >= 0 {
                eff_bg[p as usize]
            } else {
                Rgb::WHITE
            };
            eff_bg[i] = match (li >= 0)
                .then(|| parse_color(v.style(li as usize, st::BG)))
                .flatten()
            {
                Some((c, a)) if a > 0 => c.over(a, parent_bg),
                _ => parent_bg,
            };
            opacity[i] = op;
            clip[i] = out;
            pos_clip[i] = pc_here;

            // interactive owner: nearest ancestor-or-self
            owner_of[i] = if self.owner_kind(&v, i).is_some() {
                i
            } else {
                pown
            };
        }

        // canvas colour (html first, then body)
        if di == 0 {
            for (i, &li) in node_layout.iter().enumerate() {
                if li >= 0 && matches!(v.tag(i), "HTML" | "BODY") {
                    if let Some((c, a)) = parse_color(v.style(li as usize, st::BG)) {
                        if a >= 200 && (self.canvas.is_none() || v.tag(i) == "HTML") {
                            self.canvas = Some(c);
                        }
                    }
                }
            }
        }

        // 2. paint items
        for li in 0..lay.node_index.len() {
            let ni = lay.node_index[li];
            if ni < 0 {
                continue;
            }
            let i = ni as usize;
            if i >= n || !paint_ok[i] {
                continue;
            }
            let Some(b) = vb(li) else { continue };
            let po = po_of(li);
            let nt = doc.nodes.node_type[i];
            if nt != 1 {
                continue; // text layouts are handled via text boxes
            }
            let tag = v.tag(i);
            let visible_rect = b.isect(clip[i]);
            if visible_rect.is_empty() && !matches!(tag, "HR") && (b.y1 - b.y0) > 0.5 {
                continue;
            }
            let eff_op = opacity[i];

            // iframes
            if matches!(tag, "IFRAME" | "FRAME") && depth < 2 {
                if let Some(&child) = v.child_doc.get(&(i as i32)) {
                    let inner = Rf {
                        x0: b.x0,
                        y0: b.y0,
                        x1: b.x1,
                        y1: b.y1,
                    }
                    .isect(clip[i]);
                    self.collect_doc(snap, child as usize, (b.x0, b.y0), inner, po.0, depth + 1);
                }
                continue;
            }

            // backgrounds
            if !matches!(tag, "HTML" | "BODY") {
                if let Some((c, a)) = parse_color(v.style(li, st::BG)) {
                    let a = (a as f32 * eff_op) as u8;
                    if a > 8 {
                        let r = cell_rect(self.m, visible_rect).clamp(self.m.cols, self.m.rows);
                        if !r.is_empty() {
                            let seq = self.next_seq();
                            self.items.push(Item {
                                po,
                                seq,
                                k: K::Fill {
                                    r,
                                    color: c,
                                    alpha: a,
                                },
                            });
                        }
                    }
                }
            }

            // borders / rules
            self.border_item(&v, li, b, visible_rect, eff_op, po);

            // images
            let bg_img = v.style(li, st::BG_IMAGE);
            let is_img = matches!(tag, "IMG" | "CANVAS" | "VIDEO" | "svg" | "OBJECT" | "EMBED")
                || bg_img.starts_with("url(");
            if is_img && !visible_rect.is_empty() {
                let r = cell_rect(self.m, visible_rect).clamp(self.m.cols, self.m.rows);
                if !r.is_empty() {
                    if matches!(tag, "VIDEO" | "CANVAS") {
                        self.live.push(r.to_cell_rect());
                    }
                    let owner = (owner_of[i] != usize::MAX).then_some((di, owner_of[i]));
                    let seq = self.next_seq();
                    self.items.push(Item {
                        po,
                        seq,
                        k: K::Image { r, owner },
                    });
                }
            }

            // form controls
            for g in self.control_glyphs(&v, i, li, b, &first_text, di, eff_bg[i]) {
                let seq = self.next_seq();
                self.items.push(Item {
                    po,
                    seq,
                    k: K::Glyphs(g),
                });
            }

            // owners
            if owner_of[i] == i {
                self.register_owner(&v, snap, di, i, li, b, visible_rect, po);
            }
        }

        // text boxes
        let tb = &doc.text_boxes;
        for k in 0..tb.layout_index.len() {
            let li = tb.layout_index[k];
            if li < 0 {
                continue;
            }
            let li = li as usize;
            let Some(&ni) = lay.node_index.get(li) else {
                continue;
            };
            let i = ni as usize;
            if i >= n || !paint_ok[i] {
                continue;
            }
            let Some(bb) = tb.bounds.get(k).filter(|b| b.len() >= 4) else {
                continue;
            };
            let r = Rf {
                x0: bb[0] + shift.0,
                y0: bb[1] + shift.1,
                x1: bb[0] + bb[2] + shift.0,
                y1: bb[1] + bb[3] + shift.1,
            };
            if !r.overlaps(clip[i]) {
                continue;
            }
            let row = ((r.y0 + r.y1) / 2.0 / self.m.ch).floor() as i32;
            if row < 0 || row >= self.m.rows as i32 {
                continue;
            }
            let src = v.s(lay.text.get(li).copied().unwrap_or(-1));
            let slice = glyph_proto::width::utf16_slice(
                src,
                tb.start[k].max(0) as usize,
                tb.length[k].max(0) as usize,
            );
            let text: String = slice
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect();
            if text.trim().is_empty() {
                continue;
            }
            // style comes from the text node's parent element (text layout nodes carry the inherited style)
            let sl = li;
            let fg_s = parse_color(v.style(sl, st::COLOR)).unwrap_or((Rgb::BLACK, 255));
            let mut attrs = Attrs::default();
            let w = v.style(sl, st::WEIGHT);
            if w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600) {
                attrs = attrs.with(Attrs::BOLD);
            }
            if matches!(v.style(sl, st::FONT_STYLE), "italic" | "oblique") {
                attrs = attrs.with(Attrs::ITALIC);
            }
            let deco = v.style(sl, st::DECORATION);
            if deco.contains("underline") {
                attrs = attrs.with(Attrs::UNDERLINE);
            }
            if deco.contains("line-through") {
                attrs = attrs.with(Attrs::STRIKE);
            }
            let px_col = (r.x0 / self.m.cw).round() as i32;
            let zone = CRect {
                c0: px_col,
                r0: (r.y0 / self.m.ch).floor() as i32,
                c1: (r.x1 / self.m.cw).round() as i32,
                r1: ((r.y1 - 0.01) / self.m.ch).floor() as i32 + 1,
            };
            let owner = (owner_of[i] != usize::MAX).then_some((di, owner_of[i]));
            let seq = self.next_seq();
            self.items.push(Item {
                po: po_of(li),
                seq,
                k: K::Glyphs(Glyphs {
                    row,
                    px_col,
                    x0: r.x0,
                    x1: r.x1,
                    col: px_col,
                    text,
                    fg: fg_s.0,
                    fg_alpha: (fg_s.1 as f32 * opacity[i]) as u8,
                    attrs,
                    owner,
                    fixed: false,
                    zone,
                    css_bg: eff_bg[i],
                }),
            });
        }
    }

    fn owner_kind(&self, v: &DocView, i: usize) -> Option<RegionKind> {
        let tag = v.tag(i);
        let by_tag = match tag {
            "A" => v.attr(i, "href").is_some().then_some(RegionKind::Link),
            "BUTTON" | "SUMMARY" => Some(RegionKind::Button),
            "TEXTAREA" => Some(RegionKind::TextArea),
            "SELECT" => Some(RegionKind::Select),
            "INPUT" => match v
                .attr(i, "type")
                .unwrap_or("text")
                .to_ascii_lowercase()
                .as_str()
            {
                "hidden" => None,
                "checkbox" => Some(RegionKind::Checkbox),
                "radio" => Some(RegionKind::Radio),
                "submit" | "button" | "reset" | "image" | "file" | "color" => {
                    Some(RegionKind::Button)
                }
                _ => Some(RegionKind::Input),
            },
            _ => None,
        };
        if by_tag.is_some() {
            return by_tag;
        }
        if let Some(role) = v.attr(i, "role") {
            match role {
                "button" | "menuitem" | "tab" | "switch" | "option" | "menuitemcheckbox"
                | "menuitemradio" => return Some(RegionKind::Button),
                "link" => return Some(RegionKind::Link),
                "textbox" | "searchbox" | "combobox" => return Some(RegionKind::Input),
                "checkbox" => return Some(RegionKind::Checkbox),
                "radio" => return Some(RegionKind::Radio),
                _ => {}
            }
        }
        if matches!(
            v.attr(i, "contenteditable"),
            Some("" | "true" | "plaintext-only")
        ) {
            return Some(RegionKind::TextArea);
        }
        if v.clickable.contains(&(i as i32))
            && !matches!(tag, "HTML" | "BODY" | "#document" | "LABEL")
        {
            return Some(RegionKind::Other);
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn register_owner(
        &mut self,
        v: &DocView,
        snap: &SnapshotResult,
        di: usize,
        i: usize,
        li: usize,
        b: Rf,
        vis: Rf,
        po: (i32, i32),
    ) {
        let Some(kind) = self.owner_kind(v, i) else {
            return;
        };
        let href = v.attr(i, "href").and_then(|h| {
            let base = snap
                .strings
                .get(v.doc.base_url.max(0) as usize)
                .map_or("", String::as_str);
            let abs = url::Url::parse(base)
                .ok()
                .and_then(|b| b.join(h).ok())
                .map(String::from)
                .or_else(|| Some(h.to_owned()))?;
            (!abs.to_ascii_lowercase().starts_with("javascript:")).then_some(abs)
        });
        let attr_label = ["aria-label", "title", "alt", "placeholder", "value"]
            .iter()
            .find_map(|a| v.attr(i, a).filter(|s| !s.trim().is_empty()))
            .unwrap_or("")
            .to_owned();
        let value = v.input_value.get(&(i as i32)).map(|&s| v.s(s).to_owned());
        let r = cell_rect(self.m, vis.isect(b)).clamp(self.m.cols, self.m.rows);
        let fallback = (!r.is_empty()).then_some(r);
        let _ = li;
        self.owners.insert(
            (di, i),
            Owner {
                kind,
                href,
                attr_label,
                text_label: String::new(),
                value,
                backend: v.doc.nodes.backend_id.get(i).copied().unwrap_or(-1),
                rects: Vec::new(),
                fallback,
                po,
            },
        );
    }

    fn border_item(&mut self, v: &DocView, li: usize, b: Rf, vis: Rf, eff_op: f32, po: (i32, i32)) {
        let mut sides = [false; 4];
        let mut color = Rgb::BLACK;
        let mut found = false;
        for (s, side) in sides.iter_mut().enumerate() {
            let w = parse_px(v.style(li, st::BORDER + 3 * s));
            let style = v.style(li, st::BORDER + 3 * s + 1);
            if w >= 1.0 && !matches!(style, "none" | "hidden" | "") {
                if let Some((c, a)) = parse_color(v.style(li, st::BORDER + 3 * s + 2)) {
                    if (a as f32 * eff_op) > 40.0 {
                        *side = true;
                        if !found {
                            color = c;
                            found = true;
                        }
                    }
                }
            }
        }
        if !found || vis.is_empty() {
            return;
        }
        let r = cell_rect(self.m, b);
        let rows_tall = (b.y1 - b.y0) / self.m.ch;
        let thin = rows_tall < 0.6;
        let tall = (r.r1 - r.r0) >= 3 && (r.c1 - r.c0) >= 3;
        if !thin && !tall {
            return; // 1–2 row things (buttons, inputs, table cells) stay plain
        }
        let round = parse_px(v.style(li, st::RADIUS)) > 0.5;
        let seq = self.next_seq();
        self.items.push(Item {
            po,
            seq,
            k: K::Border {
                px: b,
                r,
                color,
                sides,
                round,
            },
        });
    }

    /// Text for inputs/textarea/select/checkbox/radio, drawn by us because Chromium keeps their
    /// content in the user-agent shadow tree (no text boxes).
    #[allow(clippy::too_many_arguments)]
    fn control_glyphs(
        &self,
        v: &DocView,
        i: usize,
        li: usize,
        b: Rf,
        first_text: &HashMap<usize, usize>,
        di: usize,
        css_bg: Rgb,
    ) -> Vec<Glyphs> {
        let tag = v.tag(i);
        let (c0, c1) = (
            (b.x0 / self.m.cw).round() as i32,
            (b.x1 / self.m.cw).round() as i32,
        );
        let width = (c1 - c0).max(1);
        let row = ((b.y0 + b.y1) / 2.0 / self.m.ch).floor() as i32;
        let rows = (((b.y1 - b.y0) / self.m.ch).round() as i32).max(1);
        if row < 0 && rows == 1 || row >= self.m.rows as i32 {
            return Vec::new();
        }
        let value = v.input_value.get(&(i as i32)).map(|&s| v.s(s).to_owned());
        let mut centered = false;
        let text = match tag {
            "INPUT" => {
                let ty = v.attr(i, "type").unwrap_or("text").to_ascii_lowercase();
                match ty.as_str() {
                    "hidden" => return Vec::new(),
                    "checkbox" => {
                        centered = true;
                        (if v.checked.contains(&(i as i32)) {
                            "[x]"
                        } else {
                            "[ ]"
                        })
                        .to_owned()
                    }
                    "radio" => {
                        centered = true;
                        (if v.checked.contains(&(i as i32)) {
                            "(•)"
                        } else {
                            "( )"
                        })
                        .to_owned()
                    }
                    "submit" | "button" | "reset" => format!(
                        "[ {} ]",
                        value
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| ty.clone())
                    ),
                    _ => {
                        let shown = match (&value, ty.as_str()) {
                            (Some(val), "password") => "•".repeat(str_width(val)),
                            (Some(val), _) => val.clone(),
                            _ => String::new(),
                        };
                        let placeholder = shown.is_empty();
                        let body = if placeholder {
                            v.attr(i, "placeholder").unwrap_or("").to_owned()
                        } else {
                            shown
                        };
                        field(&body, width as usize, placeholder)
                    }
                }
            }
            "TEXTAREA" => {
                let body = v
                    .input_value
                    .get(&(i as i32))
                    .map(|&s| v.s(s).to_owned())
                    .or_else(|| {
                        first_text
                            .get(&i)
                            .map(|&j| v.s(v.doc.nodes.node_value[j]).to_owned())
                    })
                    .unwrap_or_default();
                if rows >= 3 {
                    // boxed (see border_item): text lines sit inside the frame, no brackets
                    let (fg, a) = parse_color(v.style(li, st::COLOR)).unwrap_or((Rgb::BLACK, 255));
                    let top = ((b.y0 / self.m.ch).round() as i32) + 1;
                    return body
                        .lines()
                        .take((rows - 2) as usize)
                        .enumerate()
                        .map(|(k, l)| Glyphs {
                            row: top + k as i32,
                            px_col: c0 + 1,
                            x0: b.x0,
                            x1: b.x1,
                            col: c0 + 1,
                            text: truncate(l, (width - 2).max(1) as usize),
                            fg,
                            fg_alpha: a,
                            attrs: Attrs::default(),
                            owner: Some((di, i)),
                            fixed: true,
                            css_bg,
                            zone: CRect {
                                c0,
                                r0: top + k as i32,
                                c1,
                                r1: top + k as i32 + 1,
                            },
                        })
                        .filter(|g| g.row >= 0 && g.row < self.m.rows as i32)
                        .collect();
                }
                let first = body.lines().next().unwrap_or("").to_owned();
                field(&first, width as usize, first.is_empty())
            }
            "SELECT" => {
                let mut label = String::new();
                for (j, &p) in v.doc.nodes.parent.iter().enumerate() {
                    if p >= 0 && v.selected.contains(&(j as i32)) && {
                        let pp = p as usize;
                        pp == i || v.doc.nodes.parent.get(pp).is_some_and(|&g| g == i as i32)
                    } {
                        if let Some(&t) = first_text.get(&j) {
                            label = v.s(v.doc.nodes.node_value[t]).trim().to_owned();
                        }
                        break;
                    }
                }
                let inner = (width as usize)
                    .saturating_sub(4)
                    .max(str_width(&label))
                    .max(1);
                format!("[{label:<inner$} ▾]")
            }
            _ => return Vec::new(),
        };
        let (fg, a) = parse_color(v.style(li, st::COLOR)).unwrap_or((Rgb::BLACK, 255));
        let owner = Some((di, i));
        // 13 px checkboxes are narrower than their 3-cell glyph: centre it on the real box
        let c0 = if centered {
            (((b.x0 + b.x1) / 2.0 / self.m.cw).round() as i32 - 1).max(0)
        } else {
            c0
        };
        vec![Glyphs {
            row: row.clamp(0, self.m.rows as i32 - 1),
            px_col: c0,
            x0: b.x0,
            x1: b.x1,
            col: c0,
            text,
            fg,
            fg_alpha: a,
            attrs: Attrs::default(),
            owner,
            fixed: true,
            css_bg,
            zone: CRect {
                c0,
                r0: (b.y0 / self.m.ch).floor() as i32,
                c1,
                r1: ((b.y1 - 0.01) / self.m.ch).floor() as i32 + 1,
            },
        }]
    }

    /// Steps 3–7.
    fn finish(mut self, pix: Option<&Pixmap>) -> Rendered {
        let m = *self.m;
        let (cols, rows) = (m.cols as i32, m.rows as i32);

        // 4. chain adjacent inline boxes on a row, then compute truncation limits.
        self.place_text();

        // 5/7. regions: attach text rects, fall back to bounds, drop Other-owners that look generic.
        for it in &self.items {
            if let K::Glyphs(g) = &it.k {
                if let Some(o) = g.owner.and_then(|k| self.owners.get_mut(&k)) {
                    let w = str_width(&g.text) as i32;
                    let c1 = (g.col + w).min(cols);
                    if c1 > g.col.max(0) {
                        o.rects.push(CellRect {
                            x: g.col.max(0) as u16,
                            y: g.row as u16,
                            w: (c1 - g.col.max(0)) as u16,
                            h: 1,
                        });
                    }
                    let t = g.text.trim();
                    if !g.fixed && !t.is_empty() && o.text_label.chars().count() < 80 {
                        if !o.text_label.is_empty() {
                            o.text_label.push(' ');
                        }
                        o.text_label.push_str(t);
                    }
                }
            }
        }
        let mut keys: Vec<OwnerKey> = self.owners.keys().copied().collect();
        keys.retain(|k| {
            let o = &self.owners[k];
            let has_text = !o.rects.is_empty();
            match o.kind {
                RegionKind::Other => {
                    has_text && o.rects.len() <= 6 && o.text_label.chars().count() <= 100
                }
                _ => has_text || o.fallback.is_some(),
            }
        });
        let first_pos = |o: &Owner| {
            o.rects
                .first()
                .map(|r| (r.y, r.x))
                .or_else(|| o.fallback.map(|f| (f.r0 as u16, f.c0 as u16)))
                .unwrap_or((0, 0))
        };
        keys.sort_by_key(|k| first_pos(&self.owners[k]));
        let ids: HashMap<OwnerKey, u32> = keys
            .iter()
            .enumerate()
            .map(|(n, k)| (*k, n as u32 + 1))
            .collect();
        // owners without text (image links, icon buttons, inputs): mark their bounds
        let mut marks = Vec::new();
        for k in &keys {
            let o = &self.owners[k];
            if o.rects.is_empty() {
                if let Some(f) = o.fallback {
                    marks.push((o.po, f, *k));
                }
            }
        }
        for (po, r, owner) in marks {
            let seq = self.next_seq();
            self.items.push(Item {
                po,
                seq,
                k: K::LinkMark { r, owner },
            });
        }

        // 6. painter's algorithm
        self.items.sort_by_key(|it| (it.po, it.order(), it.seq));
        let canvas = self.canvas.unwrap_or(Rgb::WHITE);
        let default_fg = if luma(canvas) > 128 {
            Rgb(0x22, 0x22, 0x22)
        } else {
            Rgb(0xdd, 0xdd, 0xdd)
        };
        let mut grid = Grid::new(m.cols, m.rows, Style::new(default_fg, canvas));
        let mut kind = vec![Kind::Plain; (cols * rows) as usize];
        let mut cell_owner: Vec<i32> = vec![-1; (cols * rows) as usize];
        let mut boxes: Vec<BoxRec> = Vec::new();
        let mut halo = vec![false; (cols * rows) as usize];
        let idx = |c: i32, r: i32| (r * cols + c) as usize;
        let mut images = Vec::new();

        for it in &self.items {
            match &it.k {
                K::Fill { r, color, alpha } => {
                    for y in r.r0..r.r1 {
                        for x in r.c0..r.c1 {
                            let (ux, uy) = (x as u16, y as u16);
                            if *alpha >= 230 {
                                let s = Style {
                                    fg: grid.cell(ux, uy).style.fg,
                                    bg: *color,
                                    attrs: Attrs::default(),
                                    link: 0,
                                };
                                grid.clear(ux, uy, s);
                                kind[idx(x, y)] = Kind::Plain;
                                cell_owner[idx(x, y)] = -1;
                            } else {
                                let bg = color.over(*alpha, grid.cell(ux, uy).style.bg);
                                grid.restyle(ux, uy, |s| s.bg = bg);
                            }
                        }
                    }
                }
                K::Border {
                    px,
                    r,
                    color,
                    sides,
                    round,
                } => {
                    draw_border(&mut grid, &mut kind, &m, *px, *r, *color, *sides, *round);
                    // The drawn glyphs stand for this border; its pixel line, which usually
                    // straddles a cell boundary, must not be drawn a second time next to them.
                    for y in (r.r0 - 1).max(0)..(r.r1 + 1).min(rows) {
                        for x in (r.c0 - 1).max(0)..(r.c1 + 1).min(cols) {
                            halo[idx(x, y)] = true;
                        }
                    }
                }
                K::Image { r, owner } => {
                    let link = owner.and_then(|o| ids.get(&o)).copied().unwrap_or(0);
                    for y in r.r0..r.r1 {
                        for x in r.c0..r.c1 {
                            let bg = grid.cell(x as u16, y as u16).style.bg;
                            grid.clear(
                                x as u16,
                                y as u16,
                                Style {
                                    fg: Rgb(0x88, 0x88, 0x88),
                                    bg,
                                    attrs: Attrs::DIM,
                                    link,
                                },
                            );
                            grid.put(
                                x as u16,
                                y as u16,
                                "░",
                                Style {
                                    fg: Rgb(0x99, 0x99, 0x99),
                                    bg,
                                    attrs: Attrs::default(),
                                    link,
                                },
                            );
                            kind[idx(x, y)] = Kind::Image;
                            cell_owner[idx(x, y)] = -1;
                        }
                    }
                    if (r.c1 - r.c0) >= 2 && (r.r1 - r.r0) >= 1 {
                        images.push(r.to_cell_rect());
                    }
                }
                K::LinkMark { r, owner } => {
                    if let Some(&id) = ids.get(owner) {
                        for y in r.r0..r.r1 {
                            for x in r.c0..r.c1 {
                                grid.restyle(x as u16, y as u16, |s| s.link = id);
                            }
                        }
                    }
                }
                K::Glyphs(g) => {
                    let link = g.owner.and_then(|o| ids.get(&o)).copied().unwrap_or(0);
                    paint_glyphs(
                        &mut grid,
                        &mut kind,
                        &mut cell_owner,
                        &mut boxes,
                        &m,
                        g,
                        link,
                    );
                }
            }
        }

        // 5. colours from pixels
        if let Some(p) = pix {
            colour::apply(&mut grid, &kind, &cell_owner, &halo, &boxes, p, &m);
        }

        // blank cells carry no foreground: normalising keeps equal-looking cells in one span
        for y in 0..m.rows {
            for x in 0..m.cols {
                if grid.cell(x, y).g == " " {
                    // borrow the left neighbour's foreground (same background) so a space between
                    // two words of one colour stays inside their span
                    let left = (x > 0)
                        .then(|| grid.cell(x - 1, y).style)
                        .filter(|l| l.bg == grid.cell(x, y).style.bg);
                    let fg = left.map_or(default_fg, |l| l.fg);
                    grid.restyle(x, y, |s| {
                        s.fg = fg;
                        s.attrs = Attrs::default();
                    });
                }
            }
        }

        // prune regions that were fully occluded
        let mut visible: HashMap<u32, u32> = HashMap::new();
        for y in 0..m.rows {
            for c in grid.row(y) {
                if c.style.link != 0 {
                    *visible.entry(c.style.link).or_default() += 1;
                }
            }
        }
        let regions = keys
            .iter()
            .filter_map(|k| {
                let id = ids[k];
                visible.contains_key(&id).then(|| {
                    let o = &self.owners[k];
                    let rects = if o.rects.is_empty() {
                        o.fallback
                            .map(|f| vec![f.to_cell_rect()])
                            .unwrap_or_default()
                    } else {
                        o.rects.clone()
                    };
                    let label = if o.text_label.is_empty() {
                        o.attr_label.trim()
                    } else {
                        o.text_label.trim()
                    };
                    Region {
                        id,
                        kind: o.kind,
                        rects,
                        href: o.href.clone(),
                        label: label.to_owned(),
                        value: o.value.clone(),
                        node: o.backend,
                    }
                })
            })
            .collect();

        Rendered {
            grid,
            regions,
            images,
            live: self.live,
            page: self.page,
        }
    }

    /// Chain touching boxes on a row so proportional text keeps reading order, then limit each
    /// non-chained box to the start of its right neighbour.
    fn place_text(&mut self) {
        let cols = self.m.cols as i32;
        let cw = self.m.cw;
        let mut by_row: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::new();
        for (n, it) in self.items.iter().enumerate() {
            if let K::Glyphs(g) = &it.k {
                by_row.entry((it.po.0, it.po.1, g.row)).or_default().push(n);
            }
        }
        for (_, mut list) in by_row {
            list.sort_by(|&a, &b| {
                let (ga, gb) = (glyphs(&self.items[a]), glyphs(&self.items[b]));
                ga.x0
                    .partial_cmp(&gb.x0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(self.items[a].seq.cmp(&self.items[b].seq))
            });
            // pass A: start columns
            let mut chained = vec![false; list.len()];
            for k in 1..list.len() {
                let (p, c) = (
                    glyphs(&self.items[list[k - 1]]),
                    glyphs(&self.items[list[k]]),
                );
                let gap = c.x0 - p.x1;
                chained[k] = !p.fixed && !c.fixed && (-2.0..=cw * 0.75).contains(&gap);
            }
            let mut prev_end = 0;
            for k in 0..list.len() {
                let mut start = if k > 0 && chained[k] {
                    prev_end
                } else {
                    glyphs(&self.items[list[k]]).px_col
                };
                if k > 0
                    && !chained[k]
                    && glyphs(&self.items[list[k - 1]]).fixed
                    && !glyphs(&self.items[list[k]]).fixed
                    && start < prev_end
                {
                    start = prev_end; // never overlap a preceding form control
                }
                let g = glyphs_mut(&mut self.items[list[k]]);
                g.col = start;
                prev_end = start + str_width(&g.text) as i32;
            }
            // pass B: truncate against the next non-chained start
            for k in 0..list.len() {
                let limit = if k + 1 < list.len() && !chained[k + 1] {
                    glyphs(&self.items[list[k + 1]]).col
                } else {
                    cols
                };
                let g = glyphs_mut(&mut self.items[list[k]]);
                if g.fixed {
                    continue;
                }
                let avail = limit - g.col;
                if avail < 1 {
                    g.text.clear();
                } else if str_width(&g.text) as i32 > avail {
                    g.text = truncate(&g.text, avail as usize);
                }
            }
        }
    }
}

fn glyphs(it: &Item) -> &Glyphs {
    match &it.k {
        K::Glyphs(g) => g,
        _ => unreachable!("indexed as glyphs"),
    }
}

fn glyphs_mut(it: &mut Item) -> &mut Glyphs {
    match &mut it.k {
        K::Glyphs(g) => g,
        _ => unreachable!("indexed as glyphs"),
    }
}

fn luma(c: Rgb) -> u32 {
    (c.0 as u32 * 299 + c.1 as u32 * 587 + c.2 as u32 * 114) / 1000
}

/// Cut `s` to at most `max` cells, ending in `…` when anything was removed.
fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if str_width(s) <= max {
        return s.to_owned();
    }
    let mut out = String::new();
    let mut w = 0;
    for g in clusters(s) {
        let gw = cluster_width(g) as usize;
        if w + gw > max.saturating_sub(1) {
            break;
        }
        out.push_str(g);
        w += gw;
    }
    out.push('…');
    out
}

/// `[body____]` padded to `width` cells.
fn field(body: &str, width: usize, placeholder: bool) -> String {
    let inner = width.saturating_sub(2).max(1);
    let b = truncate(body, inner);
    let pad = inner.saturating_sub(str_width(&b));
    let fill = if placeholder { " " } else { "_" };
    format!("[{b}{}]", fill.repeat(pad))
}

fn paint_glyphs(
    grid: &mut Grid,
    kind: &mut [Kind],
    owner: &mut [i32],
    boxes: &mut Vec<BoxRec>,
    m: &Metrics,
    g: &Glyphs,
    link: u32,
) {
    let (cols, rows) = (m.cols as i32, m.rows as i32);
    // The run owns its text extent (plus one cell either side: italic/bold overhang), so its
    // background is estimated once for all of those cells and no stroke noise reaches the
    // non-text colour stage.
    let c0 = (g.zone.c0.min(g.col) - 1).max(0);
    let c1 = (g.zone.c1.max(g.col + str_width(&g.text) as i32) + 1).min(cols);
    let (r0, r1) = (g.zone.r0.max(0), g.zone.r1.min(rows));
    if c1 > c0 && r1 > r0 {
        let id = boxes.len() as i32;
        boxes.push(BoxRec {
            c0,
            r0,
            c1,
            r1,
            fg: g.fg,
            css_bg: g.css_bg,
        });
        for y in r0..r1 {
            for x in c0..c1 {
                owner[(y * cols + x) as usize] = id;
            }
        }
    }
    let mut x = g.col;
    for cl in clusters(&g.text) {
        if cl.chars().next().is_some_and(char::is_whitespace) {
            x += 1;
            continue;
        }
        let w = cluster_width(cl) as i32;
        if w == 0 {
            continue;
        }
        if x >= 0 && x + w <= cols && g.row >= 0 && g.row < rows {
            let under = grid.cell(x as u16, g.row as u16).style.bg;
            let fg = g.fg.over(g.fg_alpha, under);
            let st = Style {
                fg,
                bg: under,
                attrs: g.attrs,
                link,
            };
            grid.put(x as u16, g.row as u16, cl, st);
            for dx in 0..w {
                kind[(g.row * cols + x + dx) as usize] = Kind::Glyph;
            }
        }
        x += w;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_border(
    grid: &mut Grid,
    kind: &mut [Kind],
    m: &Metrics,
    px: Rf,
    r: CRect,
    color: Rgb,
    sides: [bool; 4],
    round: bool,
) {
    let (cols, rows) = (m.cols as i32, m.rows as i32);
    let mut put = |x: i32, y: i32, ch: &str| {
        if x >= 0 && y >= 0 && x < cols && y < rows {
            let bg = grid.cell(x as u16, y as u16).style.bg;
            let st = Style {
                fg: color.over(255, bg),
                bg,
                attrs: Attrs::default(),
                link: 0,
            };
            grid.put(x as u16, y as u16, ch, st);
            kind[(y * cols + x) as usize] = Kind::Glyph;
        }
    };
    let thin = (px.y1 - px.y0) / m.ch < 0.6;
    if sides.iter().all(|&s| s) && !thin && (r.r1 - r.r0) >= 3 && (r.c1 - r.c0) >= 3 {
        let (l, t, rr, b) = (r.c0, r.r0, r.c1 - 1, r.r1 - 1);
        let (tl, tr, bl, br) = if round {
            ("╭", "╮", "╰", "╯")
        } else {
            ("┌", "┐", "└", "┘")
        };
        for x in l + 1..rr {
            put(x, t, "─");
            put(x, b, "─");
        }
        for y in t + 1..b {
            put(l, y, "│");
            put(rr, y, "│");
        }
        put(l, t, tl);
        put(rr, t, tr);
        put(l, b, bl);
        put(rr, b, br);
        return;
    }
    let row_of = |y: f64| (y / m.ch).floor() as i32;
    let col_of = |x: f64| (x / m.cw).floor() as i32;
    if thin {
        if sides[0] || sides[2] {
            let y = row_of(if sides[0] { px.y0 + 0.5 } else { px.y1 - 0.5 });
            for x in r.c0..r.c1.max(r.c0 + 1) {
                put(x, y, "─");
            }
        }
        return;
    }
    if sides[0] {
        for x in r.c0..r.c1 {
            put(x, row_of(px.y0 + 0.5), "─");
        }
    }
    if sides[2] {
        for x in r.c0..r.c1 {
            put(x, row_of(px.y1 - 0.5), "─");
        }
    }
    if sides[3] {
        for y in r.r0..r.r1 {
            put(col_of(px.x0 + 0.5), y, "│");
        }
    }
    if sides[1] {
        for y in r.r0..r.r1 {
            put(col_of(px.x1 - 0.5), y, "│");
        }
    }
}
