//! Text (reader) mode: accessibility tree → linear, word-wrapped document → viewport slice.
//!
//! The AX tree supplies semantics (roles, heading levels, link URLs, control values, list
//! markers). It does not say whether a `generic` node was a `div` or a `span`, so block-vs-inline
//! comes from a tiny `display`-only DOMSnapshot joined on `backendNodeId`.

use std::collections::HashMap;

use glyph_proto::{
    width::{cluster_width, clusters, str_width},
    Attrs, CellRect, Grid, Region, RegionKind, Rgb, Style,
};
use serde::Deserialize;
use serde_json::Value;

// ---------------------------------------------------------------- AX types

#[derive(Debug, Default, Deserialize)]
pub struct AxTree {
    #[serde(default)]
    pub nodes: Vec<AxNode>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AxValue {
    #[serde(default)]
    pub value: Option<Value>,
}

impl AxValue {
    fn text(&self) -> String {
        match &self.value {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Bool(b)) => b.to_string(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct AxProp {
    pub name: String,
    #[serde(default)]
    pub value: AxValue,
}

#[derive(Debug, Default, Deserialize)]
pub struct AxNode {
    #[serde(rename = "nodeId")]
    pub id: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub role: Option<AxValue>,
    #[serde(default)]
    pub name: Option<AxValue>,
    #[serde(default)]
    pub value: Option<AxValue>,
    #[serde(default)]
    pub properties: Vec<AxProp>,
    #[serde(default, rename = "childIds")]
    pub children: Vec<String>,
    #[serde(default, rename = "backendDOMNodeId")]
    pub backend: Option<i64>,
}

impl AxNode {
    fn role(&self) -> String {
        self.role.as_ref().map(AxValue::text).unwrap_or_default()
    }
    fn name(&self) -> String {
        self.name.as_ref().map(AxValue::text).unwrap_or_default()
    }
    fn value(&self) -> String {
        self.value.as_ref().map(AxValue::text).unwrap_or_default()
    }
    fn prop(&self, n: &str) -> Option<String> {
        self.properties
            .iter()
            .find(|p| p.name == n)
            .map(|p| p.value.text())
    }
}

/// `backendNodeId → computed display`, from a `DOMSnapshot` requested with `["display"]`.
pub fn displays_from_snapshot(snap: &crate::snapshot::SnapshotResult) -> HashMap<i64, String> {
    let mut out = HashMap::new();
    for d in &snap.documents {
        for (li, &ni) in d.layout.node_index.iter().enumerate() {
            let Some(&b) = d.nodes.backend_id.get(ni as usize) else {
                continue;
            };
            let disp = d
                .layout
                .styles
                .get(li)
                .and_then(|s| s.first())
                .and_then(|&i| snap.strings.get(i as usize));
            if let Some(disp) = disp {
                out.insert(b, disp.clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- reader palette

const BG: Rgb = Rgb(0x1e, 0x1e, 0x2e);
const FG: Rgb = Rgb(0xcd, 0xd6, 0xf4);
const HEADING: Rgb = Rgb(0x89, 0xb4, 0xfa);
const LINK: Rgb = Rgb(0x74, 0xc7, 0xec);
const DIM: Rgb = Rgb(0x7f, 0x84, 0x9c);
const CONTROL: Rgb = Rgb(0xa6, 0xe3, 0xa1);
const RULE: Rgb = Rgb(0x45, 0x47, 0x5a);

#[derive(Clone, Debug)]
struct Span {
    text: String,
    fg: Rgb,
    attrs: Attrs,
    region: Option<usize>,
}

#[derive(Clone, Debug, Default)]
struct Line {
    indent: usize,
    spans: Vec<Span>,
    /// Full-width rule (`<hr>`).
    rule: bool,
}

#[derive(Clone, Debug)]
pub struct DocRegion {
    pub kind: RegionKind,
    pub href: Option<String>,
    pub label: String,
    pub value: Option<String>,
    pub backend: i64,
    /// (line, x, width)
    pub rects: Vec<(usize, usize, usize)>,
}

#[derive(Debug, Default)]
pub struct TextDoc {
    lines: Vec<Line>,
    pub regions: Vec<DocRegion>,
    pub cols: u16,
    margin: usize,
}

#[derive(Clone, Copy)]
struct Ctx {
    indent: usize,
    quote: usize,
    attrs: Attrs,
    fg: Rgb,
}

struct Builder<'a> {
    nodes: HashMap<&'a str, &'a AxNode>,
    parent: HashMap<&'a str, &'a str>,
    display: &'a HashMap<i64, String>,
    width: usize,
    lines: Vec<Line>,
    cur: Vec<Span>,
    marker: Option<(String, usize)>,
    regions: Vec<DocRegion>,
    link: Option<usize>,
    budget: usize,
}

const MAX_LINES: usize = 30_000;

pub fn build(tree: &AxTree, display: &HashMap<i64, String>, cols: u16) -> TextDoc {
    let width = (cols as usize).saturating_sub(4).clamp(20, 96);
    let margin = (cols as usize).saturating_sub(width) / 2;
    let nodes = tree.nodes.iter().map(|n| (n.id.as_str(), n)).collect();
    let parent = tree
        .nodes
        .iter()
        .flat_map(|n| n.children.iter().map(move |c| (c.as_str(), n.id.as_str())))
        .collect();
    let mut b = Builder {
        nodes,
        parent,
        display,
        width,
        lines: Vec::new(),
        cur: Vec::new(),
        marker: None,
        regions: Vec::new(),
        link: None,
        budget: 400_000,
    };
    if let Some(root) = tree.nodes.first() {
        let ctx = Ctx {
            indent: 0,
            quote: 0,
            attrs: Attrs::default(),
            fg: FG,
        };
        b.walk(root, ctx);
        b.flush(ctx);
    }
    while b
        .lines
        .last()
        .is_some_and(|l| l.spans.is_empty() && !l.rule)
    {
        b.lines.pop();
    }
    // place regions' rects
    let mut regions = b.regions;
    for (y, line) in b.lines.iter().enumerate() {
        let mut x = line.indent;
        for s in &line.spans {
            let w = str_width(&s.text);
            if let Some(r) = s.region {
                match regions[r].rects.last_mut() {
                    Some(last) if last.0 == y && last.1 + last.2 == x => last.2 += w,
                    _ => regions[r].rects.push((y, x, w)),
                }
            }
            x += w;
        }
    }
    regions.retain(|r| !r.rects.is_empty());
    TextDoc {
        lines: b.lines,
        regions,
        cols,
        margin,
    }
}

fn is_block_display(d: &str) -> bool {
    !matches!(
        d,
        "inline"
            | "inline-block"
            | "inline-flex"
            | "inline-grid"
            | "inline-table"
            | "contents"
            | "ruby"
            | ""
    )
}

impl<'a> Builder<'a> {
    fn child(&self, id: &str) -> Option<&'a AxNode> {
        self.nodes.get(id).copied()
    }

    /// Is the next thing after `n` (among its siblings) plain text? Then that text is the visible
    /// label and the control's accessible name would just repeat it.
    fn followed_by_text(&self, n: &AxNode) -> bool {
        let Some(p) = self.parent.get(n.id.as_str()).and_then(|p| self.child(p)) else {
            return false;
        };
        let Some(i) = p.children.iter().position(|c| *c == n.id) else {
            return false;
        };
        for sib in &p.children[i + 1..] {
            let Some(s) = self.child(sib) else { continue };
            return self.leads_with_text(s, 0);
        }
        false
    }

    fn leads_with_text(&self, n: &AxNode, depth: usize) -> bool {
        if n.role() == "StaticText" {
            return !n.name().trim().is_empty();
        }
        if depth > 4 {
            return false;
        }
        n.children
            .iter()
            .filter_map(|c| self.child(c))
            .next()
            .is_some_and(|c| self.leads_with_text(c, depth + 1))
    }

    fn is_block(&self, n: &AxNode) -> bool {
        n.backend
            .and_then(|b| self.display.get(&b))
            .is_some_and(|d| is_block_display(d))
    }

    fn walk_children(&mut self, n: &AxNode, ctx: Ctx) {
        for c in &n.children {
            if let Some(c) = self.child(c) {
                self.walk(c, ctx);
            }
        }
    }

    fn blank(&mut self) {
        if self
            .lines
            .last()
            .is_some_and(|l| !l.spans.is_empty() || l.rule)
        {
            self.lines.push(Line::default());
        }
    }

    fn push_span(&mut self, text: &str, ctx: Ctx, fg: Rgb, attrs: Attrs, region: Option<usize>) {
        if text.is_empty() || self.budget == 0 {
            return;
        }
        self.budget = self.budget.saturating_sub(text.len());
        let attrs = Attrs(ctx.attrs.0 | attrs.0);
        // plain text right after a form control (`[____]text`) needs a blank to stay readable
        let region = region.or(self.link);
        if region.is_none() && !text.starts_with(char::is_whitespace) {
            let after_control = self.cur.last().is_some_and(|p| {
                !p.text.ends_with(char::is_whitespace)
                    && p.region.is_some_and(|r| {
                        self.regions
                            .get(r)
                            .is_some_and(|r| r.kind != RegionKind::Link)
                    })
            });
            if after_control {
                self.cur.push(Span {
                    text: " ".into(),
                    fg,
                    attrs: Attrs::default(),
                    region: None,
                });
            }
        }
        self.cur.push(Span {
            text: text.to_owned(),
            fg,
            attrs,
            region,
        });
    }

    fn text(&mut self, s: &str, ctx: Ctx) {
        let (fg, attrs) = if self.link.is_some() {
            (LINK, Attrs::UNDERLINE)
        } else {
            (ctx.fg, Attrs::default())
        };
        // hard newlines inside text (pre, br-less text) become line breaks
        let mut first = true;
        for part in s.split('\n') {
            if !first {
                self.flush(ctx);
            }
            first = false;
            let part: String = part
                .chars()
                .map(|c| if c.is_whitespace() { ' ' } else { c })
                .collect();
            self.push_span(&part, ctx, fg, attrs, None);
        }
    }

    /// Atomic inline things (links, controls) are often separated only by CSS margins; give them a
    /// blank so they do not run together.
    fn ensure_gap(&mut self, ctx: Ctx, after_region_only: bool) {
        let needs = match self.cur.last() {
            Some(l) => {
                !l.text.ends_with([' ', '(', '[', '“', '"'])
                    && (!after_region_only || l.region.is_some())
            }
            None => false,
        };
        if needs {
            self.push_span(" ", ctx, ctx.fg, Attrs::default(), None);
        }
    }

    fn new_region(
        &mut self,
        kind: RegionKind,
        n: &AxNode,
        href: Option<String>,
        label: &str,
        value: Option<String>,
    ) -> usize {
        self.regions.push(DocRegion {
            kind,
            href,
            label: label.to_owned(),
            value,
            backend: n.backend.unwrap_or(-1),
            rects: Vec::new(),
        });
        self.regions.len() - 1
    }

    fn walk(&mut self, n: &AxNode, ctx: Ctx) {
        if self.budget == 0 {
            return;
        }
        if n.ignored {
            // ignored wrappers are transparent, except block-level ones which still separate text
            let block = self.is_block(n);
            if block {
                self.flush(ctx);
            }
            self.walk_children(n, ctx);
            if block {
                self.flush(ctx);
            }
            return;
        }
        let role = n.role();
        match role.as_str() {
            "InlineTextBox" | "none" | "presentation" if n.children.is_empty() => {}
            "StaticText" => self.text(&n.name(), ctx),
            "LineBreak" => self.flush(ctx),
            "heading" => {
                self.flush(ctx);
                self.blank();
                let level: usize = n.prop("level").and_then(|l| l.parse().ok()).unwrap_or(1);
                let hctx = Ctx {
                    attrs: ctx.attrs.with(Attrs::BOLD),
                    fg: HEADING,
                    ..ctx
                };
                self.push_span(
                    &format!("{} ", "#".repeat(level.clamp(1, 6))),
                    hctx,
                    DIM,
                    Attrs::default(),
                    None,
                );
                self.walk_children(n, hctx);
                self.flush(hctx);
                self.blank();
            }
            "paragraph" | "article" | "section" | "main" | "navigation" | "banner"
            | "contentinfo" | "complementary" | "form" | "region" | "figure" | "group"
            | "search" | "Section" | "DescriptionList" | "term" | "definition" | "figcaption"
            | "caption" | "details" | "dialog" => {
                self.flush(ctx);
                if matches!(role.as_str(), "paragraph" | "details" | "figure" | "dialog") {
                    self.blank();
                }
                self.walk_children(n, ctx);
                self.flush(ctx);
                if matches!(role.as_str(), "paragraph" | "details" | "figure" | "dialog") {
                    self.blank();
                }
            }
            "blockquote" => {
                self.flush(ctx);
                self.blank();
                let q = Ctx {
                    quote: ctx.quote + 1,
                    fg: DIM,
                    ..ctx
                };
                self.walk_children(n, q);
                self.flush(q);
                self.blank();
            }
            "list" | "DescriptionListDetail" => {
                self.flush(ctx);
                if ctx.indent == 0 {
                    self.blank();
                }
                let inner = Ctx {
                    indent: ctx.indent + if ctx.indent == 0 { 0 } else { 2 },
                    ..ctx
                };
                self.walk_children(n, inner);
                self.flush(inner);
                if ctx.indent == 0 {
                    self.blank();
                }
            }
            "listitem" => {
                self.flush(ctx);
                let marker = n
                    .children
                    .iter()
                    .filter_map(|c| self.child(c))
                    .find(|c| c.role() == "ListMarker")
                    .map(|m| m.name())
                    .filter(|m| !m.trim().is_empty())
                    .unwrap_or_else(|| "• ".to_owned());
                self.marker = Some((marker.clone(), ctx.indent));
                let inner = Ctx {
                    indent: ctx.indent + str_width(&marker),
                    ..ctx
                };
                self.walk_children(n, inner);
                self.flush(inner);
                self.marker = None;
            }
            "ListMarker" => {}
            "separator" => {
                self.flush(ctx);
                self.lines.push(Line {
                    indent: 0,
                    spans: Vec::new(),
                    rule: true,
                });
                self.blank();
            }
            "link" => {
                self.ensure_gap(ctx, true);
                let url = n.prop("url");
                let prev = self.link;
                let idx = self.new_region(RegionKind::Link, n, url, &n.name(), None);
                self.link = Some(idx);
                self.walk_children(n, ctx);
                self.link = prev;
            }
            "button" | "DisclosureTriangle" | "menuitem" | "tab" | "switch" | "toggleButton" => {
                self.ensure_gap(ctx, false);
                let label = if n.name().is_empty() {
                    "button".to_owned()
                } else {
                    n.name()
                };
                let idx = self.new_region(RegionKind::Button, n, None, &label, None);
                // non-breaking blanks keep "[ Label ]" in one piece when wrapping
                self.push_span(
                    &format!("[\u{a0}{}\u{a0}]", label.replace(' ', "\u{a0}")),
                    ctx,
                    CONTROL,
                    Attrs::BOLD,
                    Some(idx),
                );
            }
            "textbox" | "searchbox" | "textField" | "TextField" | "spinbutton" => {
                let value = n.value();
                let multiline = n.prop("multiline").as_deref() == Some("true");
                let label = n.name();
                self.ensure_gap(ctx, false);
                // the <label> text was usually just printed: do not say it twice
                let said = self
                    .cur
                    .iter()
                    .rev()
                    .take(2)
                    .any(|sp| sp.text.trim() == label.trim());
                if !label.is_empty() && !said {
                    self.push_span(&format!("{label} "), ctx, DIM, Attrs::default(), None);
                }
                let shown = if value.is_empty() {
                    String::new()
                } else {
                    value.lines().next().unwrap_or("").to_owned()
                };
                let inner = 18usize
                    .max(str_width(&shown) + 1)
                    .min(self.width.saturating_sub(str_width(&label) + 4));
                let pad = inner.saturating_sub(str_width(&shown));
                let idx = self.new_region(
                    if multiline {
                        RegionKind::TextArea
                    } else {
                        RegionKind::Input
                    },
                    n,
                    None,
                    &label,
                    Some(value.clone()),
                );
                self.push_span(
                    &format!("[{shown}{}]", "_".repeat(pad)),
                    ctx,
                    CONTROL,
                    Attrs::default(),
                    Some(idx),
                );
            }
            "checkbox" | "radio" | "menuitemcheckbox" | "menuitemradio" => {
                let on = n
                    .prop("checked")
                    .is_some_and(|c| c == "true" || c == "mixed");
                let mark = match (role.as_str(), on) {
                    ("radio" | "menuitemradio", true) => "(•)",
                    ("radio" | "menuitemradio", false) => "( )",
                    (_, true) => "[x]",
                    (_, false) => "[ ]",
                };
                self.ensure_gap(ctx, false);
                let kind = if role.contains("radio") {
                    RegionKind::Radio
                } else {
                    RegionKind::Checkbox
                };
                let idx = self.new_region(kind, n, None, &n.name(), None);
                // the visible label text is its own node (or sibling); printing the accessible
                // name too would say everything twice
                self.push_span(mark, ctx, CONTROL, Attrs::BOLD, Some(idx));
                if !n.name().trim().is_empty() && !self.followed_by_text(n) {
                    self.push_span(
                        &format!(" {}", n.name().trim()),
                        ctx,
                        ctx.fg,
                        Attrs::default(),
                        Some(idx),
                    );
                }
            }
            "combobox" | "listbox" | "ComboBoxSelect" | "PopUpButton" | "menuListPopup" => {
                let v = if n.value().is_empty() {
                    n.name()
                } else {
                    n.value()
                };
                self.ensure_gap(ctx, false);
                let said = self
                    .cur
                    .iter()
                    .rev()
                    .take(2)
                    .any(|sp| sp.text.trim() == n.name().trim());
                if !n.name().is_empty() && n.name() != v && !said {
                    self.push_span(&format!("{} ", n.name()), ctx, DIM, Attrs::default(), None);
                }
                let idx = self.new_region(RegionKind::Select, n, None, &n.name(), Some(v.clone()));
                self.push_span(
                    &format!("[{} ▾]", v.replace(' ', "\u{a0}")),
                    ctx,
                    CONTROL,
                    Attrs::default(),
                    Some(idx),
                );
            }
            "image" | "img" | "graphics-symbol" | "Canvas" | "canvas" | "video" | "Video"
            | "figureImage" => {
                let alt = n.name();
                if !alt.trim().is_empty() {
                    self.push_span(
                        &format!("[img: {}]", alt.trim()),
                        ctx,
                        DIM,
                        Attrs::ITALIC,
                        None,
                    );
                }
            }
            "table" | "grid" | "treegrid" => self.table(n, ctx),
            "strong" => self.inline_children(n, ctx, Attrs::BOLD),
            "emphasis" | "Emphasis" => self.inline_children(n, ctx, Attrs::ITALIC),
            "deletion" => self.inline_children(n, ctx, Attrs::STRIKE),
            "insertion" | "mark" => self.inline_children(n, ctx, Attrs::UNDERLINE),
            "code" | "Code" | "time" | "abbr" | "subscript" | "superscript" | "Abbr" | "Time"
            | "label" | "LabelText" | "legend" | "Legend" => {
                let block = self.is_block(n);
                if block {
                    self.flush(ctx);
                }
                self.walk_children(n, ctx);
                if block {
                    self.flush(ctx);
                }
            }
            "StaticText " => {}
            "RootWebArea" | "WebArea" | "generic" | "GenericContainer" | "Div" | "div" | "none"
            | "presentation" | "document" | "application" | "toolbar" | "tablist" | "tabpanel"
            | "menu" | "menubar" | "tree" | "treeitem" | "row" | "rowgroup" | "cell"
            | "columnheader" | "rowheader" | "gridcell" => {
                let block = self.is_block(n)
                    || matches!(
                        role.as_str(),
                        "RootWebArea"
                            | "WebArea"
                            | "document"
                            | "toolbar"
                            | "tablist"
                            | "tabpanel"
                            | "menu"
                            | "menubar"
                            | "tree"
                    );
                if block {
                    self.flush(ctx);
                }
                self.walk_children(n, ctx);
                if block {
                    self.flush(ctx);
                }
            }
            _ => {
                // unknown role: keep the content, break on block display
                let block = self.is_block(n);
                if block {
                    self.flush(ctx);
                }
                // prefer the node's own text when it has no children (e.g. exotic text roles)
                if n.children.is_empty()
                    && !n.name().trim().is_empty()
                    && !role.starts_with("Inline")
                {
                    self.text(&n.name(), ctx);
                } else {
                    self.walk_children(n, ctx);
                }
                if block {
                    self.flush(ctx);
                }
            }
        }
    }

    fn inline_children(&mut self, n: &AxNode, ctx: Ctx, a: Attrs) {
        let c = Ctx {
            attrs: ctx.attrs.with(a),
            ..ctx
        };
        self.walk_children(n, c);
    }

    /// Turn the accumulated inline spans into wrapped lines.
    fn flush(&mut self, ctx: Ctx) {
        let mut spans = std::mem::take(&mut self.cur);
        // trim the paragraph's outer whitespace
        while spans.first().is_some_and(|s| s.text.trim().is_empty()) {
            spans.remove(0);
        }
        if let Some(f) = spans.first_mut() {
            f.text = f.text.trim_start().to_owned();
        }
        while spans.last().is_some_and(|s| s.text.trim().is_empty()) {
            spans.pop();
        }
        if let Some(l) = spans.last_mut() {
            l.text = l.text.trim_end().to_owned();
        }
        if spans.is_empty() {
            return;
        }
        let quote_prefix = if ctx.quote > 0 {
            "│ ".repeat(ctx.quote)
        } else {
            String::new()
        };
        let (marker, marker_indent) = match self.marker.take() {
            Some((m, i)) => (Some(m), i),
            None => (None, ctx.indent),
        };
        let first_indent = if marker.is_some() {
            marker_indent
        } else {
            ctx.indent
        };
        let rest_indent = ctx.indent;
        let qw = str_width(&quote_prefix);
        let avail_first = self
            .width
            .saturating_sub(first_indent + qw + marker.as_ref().map_or(0, |m| str_width(m)));
        let avail_rest = self.width.saturating_sub(rest_indent + qw);
        let wrapped = wrap(&spans, avail_first.max(8), avail_rest.max(8));
        for (i, mut ws) in wrapped.into_iter().enumerate() {
            let indent = if i == 0 { first_indent } else { rest_indent };
            let mut prefix = Vec::new();
            if !quote_prefix.is_empty() {
                prefix.push(Span {
                    text: quote_prefix.clone(),
                    fg: DIM,
                    attrs: Attrs::default(),
                    region: None,
                });
            }
            if i == 0 {
                if let Some(m) = &marker {
                    prefix.push(Span {
                        text: m.clone(),
                        fg: DIM,
                        attrs: Attrs::default(),
                        region: None,
                    });
                }
            }
            prefix.append(&mut ws);
            self.lines.push(Line {
                indent,
                spans: prefix,
                rule: false,
            });
            if self.lines.len() >= MAX_LINES {
                self.budget = 0;
                return;
            }
        }
    }

    fn table(&mut self, n: &AxNode, ctx: Ctx) {
        self.flush(ctx);
        self.blank();
        // rows: descendants with role row, cells: direct cell-ish children
        let mut rows: Vec<(bool, Vec<Vec<Span>>)> = Vec::new();
        let mut stack = vec![n];
        let mut order = Vec::new();
        while let Some(x) = stack.pop() {
            if x.role() == "row" {
                order.push(x);
                continue;
            }
            for c in x.children.iter().rev() {
                if let Some(c) = self.child(c) {
                    stack.push(c);
                }
            }
        }
        for r in order {
            let mut cells = Vec::new();
            let mut header = false;
            let kids: Vec<&AxNode> = r.children.iter().filter_map(|c| self.child(c)).collect();
            for c in kids {
                let cr = c.role();
                if matches!(
                    cr.as_str(),
                    "cell" | "gridcell" | "columnheader" | "rowheader"
                ) || c.ignored
                {
                    header |= cr == "columnheader";
                    let saved = std::mem::take(&mut self.cur);
                    self.walk_children(c, ctx);
                    self.walk_flatten();
                    cells.push(std::mem::replace(&mut self.cur, saved));
                }
            }
            rows.push((header, cells));
        }
        let ncols = rows.iter().map(|(_, c)| c.len()).max().unwrap_or(0);
        if ncols == 0 {
            return;
        }
        let plain = |spans: &[Span]| spans.iter().map(|s| str_width(&s.text)).sum::<usize>();
        let mut widths = vec![3usize; ncols];
        for (_, cells) in &rows {
            for (i, c) in cells.iter().enumerate() {
                widths[i] = widths[i].max(plain(c));
            }
        }
        let sep = 3;
        let budget = self.width.saturating_sub(sep * (ncols - 1));
        while widths.iter().sum::<usize>() > budget {
            let (i, _) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .unwrap_or((0, &0));
            if widths[i] <= 4 {
                break;
            }
            widths[i] -= 1;
        }
        for (header, cells) in rows {
            let mut spans = Vec::new();
            for (i, w) in widths.iter().enumerate() {
                let empty = Vec::new();
                let c = cells.get(i).unwrap_or(&empty);
                let mut used = 0;
                for s in c {
                    let avail = w - used;
                    if avail == 0 {
                        break;
                    }
                    let mut s = s.clone();
                    if header {
                        s.attrs = s.attrs.with(Attrs::BOLD);
                    }
                    if str_width(&s.text) > avail {
                        s.text = truncate(&s.text, avail);
                    }
                    used += str_width(&s.text);
                    spans.push(s);
                }
                if used < *w {
                    spans.push(Span {
                        text: " ".repeat(w - used),
                        fg: ctx.fg,
                        attrs: Attrs::default(),
                        region: None,
                    });
                }
                if i + 1 < ncols {
                    spans.push(Span {
                        text: " │ ".into(),
                        fg: RULE,
                        attrs: Attrs::default(),
                        region: None,
                    });
                }
            }
            self.lines.push(Line {
                indent: ctx.indent,
                spans,
                rule: false,
            });
            if header {
                let total = widths.iter().sum::<usize>() + sep * (ncols - 1);
                self.lines.push(Line {
                    indent: ctx.indent,
                    spans: vec![Span {
                        text: "─".repeat(total),
                        fg: RULE,
                        attrs: Attrs::default(),
                        region: None,
                    }],
                    rule: false,
                });
            }
        }
        self.blank();
    }

    /// Collapse whitespace-only runs inside a cell's spans.
    fn walk_flatten(&mut self) {
        for s in &mut self.cur {
            s.text = s.text.replace('\n', " ");
        }
        while self.cur.first().is_some_and(|s| s.text.trim().is_empty()) {
            self.cur.remove(0);
        }
        while self.cur.last().is_some_and(|s| s.text.trim().is_empty()) {
            self.cur.pop();
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
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

/// Greedy word wrap that keeps each span's style and region. A space belongs to the span it
/// was written in, so a link never swallows the blank before it.
fn wrap(spans: &[Span], first: usize, rest: usize) -> Vec<Vec<Span>> {
    type Look = (Rgb, Attrs, Option<usize>);
    let mut lines: Vec<Vec<Span>> = vec![Vec::new()];
    let mut used = 0usize;
    let mut limit = first;
    let mut space: Option<Look> = None;
    let push = |lines: &mut Vec<Vec<Span>>, look: Look, text: String| {
        let line = lines.last_mut().expect("at least one line");
        match line.last_mut() {
            Some(l) if (l.fg, l.attrs, l.region) == look => l.text.push_str(&text),
            _ => line.push(Span {
                text,
                fg: look.0,
                attrs: look.1,
                region: look.2,
            }),
        }
    };
    for s in spans {
        let look: Look = (s.fg, s.attrs, s.region);
        for (i, word) in s.text.split(' ').enumerate() {
            if i > 0 {
                space = Some(look); // a blank inside this span
            }
            if word.is_empty() {
                continue;
            }
            let mut w = word.to_owned();
            let mut ww = str_width(&w);
            let gap = usize::from(space.is_some() && used > 0);
            if used + gap + ww > limit && used > 0 {
                lines.push(Vec::new());
                used = 0;
                limit = rest;
                space = None;
            }
            // words longer than a whole line are hard-split
            while ww > limit {
                let mut head = String::new();
                let mut hw = 0;
                let mut tail_start = 0;
                for g in clusters(&w) {
                    let gw = cluster_width(g) as usize;
                    if hw + gw > limit.saturating_sub(used) {
                        break;
                    }
                    head.push_str(g);
                    hw += gw;
                    tail_start += g.len();
                }
                if head.is_empty() {
                    if used == 0 {
                        break;
                    }
                    lines.push(Vec::new());
                    used = 0;
                    limit = rest;
                    continue;
                }
                if let (Some(sp), true) = (space.take(), used > 0) {
                    push(&mut lines, sp, " ".into());
                }
                push(&mut lines, look, head);
                lines.push(Vec::new());
                used = 0;
                limit = rest;
                w = w[tail_start..].to_owned();
                ww = str_width(&w);
            }
            if let (Some(sp), true) = (space, used > 0) {
                push(&mut lines, sp, " ".into());
                used += 1;
            }
            space = None;
            push(&mut lines, look, w);
            used += ww;
        }
        if s.text.ends_with(' ') {
            space = Some(look); // trailing blank stays with this span
        }
    }
    lines.retain(|l| !l.is_empty());
    lines
}

impl TextDoc {
    /// Left margin (cells) that centres the reader column.
    pub fn margin_width(&self) -> usize {
        self.margin
    }

    pub fn lines(&self) -> usize {
        self.lines.len()
    }

    /// Plain text of document line `y` (for find).
    pub fn line_text(&self, y: usize) -> String {
        let l = &self.lines[y];
        let mut s = " ".repeat(l.indent);
        for sp in &l.spans {
            s.push_str(&sp.text);
        }
        s
    }

    /// Document lines that contain `needle` (case folded unless `case`).
    pub fn find(&self, needle: &str, case: bool) -> Vec<usize> {
        if needle.is_empty() {
            return Vec::new();
        }
        let n = if case {
            needle.to_owned()
        } else {
            needle.to_lowercase()
        };
        (0..self.lines.len())
            .filter(|&y| {
                if case {
                    self.line_text(y).contains(&n)
                } else {
                    self.line_text(y).to_lowercase().contains(&n)
                }
            })
            .collect()
    }

    /// Render `rows` lines starting at `scroll` into a `cols × rows` grid with regions.
    pub fn slice(&self, scroll: usize, rows: u16, highlight: Option<&str>) -> (Grid, Vec<Region>) {
        let cols = self.cols;
        let base = Style::new(FG, BG);
        let mut grid = Grid::new(cols, rows, base);
        let needle = highlight.filter(|h| !h.is_empty()).map(str::to_lowercase);
        // region id = index + 1 in the document, stable while scrolling
        let mut link_at: Vec<Vec<(usize, usize, u32)>> = vec![Vec::new(); rows as usize];
        for (i, r) in self.regions.iter().enumerate() {
            for &(y, x, w) in &r.rects {
                if y >= scroll && y < scroll + rows as usize {
                    link_at[y - scroll].push((self.margin + x, w, i as u32 + 1));
                }
            }
        }
        for (dy, links) in link_at.iter().enumerate() {
            let y = scroll + dy;
            let Some(line) = self.lines.get(y) else { break };
            if line.rule {
                let w = (cols as usize).saturating_sub(self.margin * 2).max(1);
                grid.put_str(
                    self.margin as u16,
                    dy as u16,
                    &"─".repeat(w),
                    Style::new(RULE, BG),
                    cols,
                );
                continue;
            }
            let mut x = self.margin + line.indent;
            for s in &line.spans {
                let mut st = Style {
                    fg: s.fg,
                    bg: BG,
                    attrs: s.attrs,
                    link: 0,
                };
                if let Some(&(_, _, id)) = links.iter().find(|(lx, lw, _)| x >= *lx && x < lx + lw)
                {
                    st.link = id;
                }
                let before = x;
                x = grid.put_str(x.min(cols as usize) as u16, dy as u16, &s.text, st, cols)
                    as usize;
                if let Some(n) = &needle {
                    // reverse-video every match inside this span (approximate: by character position)
                    let lower = s.text.to_lowercase();
                    let mut from = 0;
                    while let Some(p) = lower[from..].find(n.as_str()) {
                        let start = before + str_width(&s.text[..from + p]);
                        let len = str_width(&s.text[from + p..from + p + n.len()]);
                        for cx in start..(start + len).min(cols as usize) {
                            grid.restyle(cx as u16, dy as u16, |s| {
                                s.attrs = s.attrs.with(Attrs::REVERSE)
                            });
                        }
                        from += p + n.len().max(1);
                    }
                }
            }
        }
        // regions visible in this slice, rects translated and clipped
        let mut out = Vec::new();
        for (i, r) in self.regions.iter().enumerate() {
            let rects: Vec<CellRect> = r
                .rects
                .iter()
                .filter(|(y, _, _)| *y >= scroll && *y < scroll + rows as usize)
                .map(|&(y, x, w)| CellRect {
                    x: (self.margin + x) as u16,
                    y: (y - scroll) as u16,
                    w: w.min(cols as usize) as u16,
                    h: 1,
                })
                .collect();
            if !rects.is_empty() {
                out.push(Region {
                    id: i as u32 + 1,
                    kind: r.kind,
                    rects,
                    href: r.href.clone(),
                    label: r.label.clone(),
                    value: r.value.clone(),
                    node: r.backend,
                });
            }
        }
        (grid, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: &str, role: &str, name: &str, kids: &[&str], props: &[(&str, &str)]) -> AxNode {
        let v = |s: &str| {
            Some(AxValue {
                value: Some(Value::String(s.into())),
            })
        };
        AxNode {
            id: id.into(),
            ignored: false,
            role: v(role),
            name: v(name),
            value: None,
            properties: props
                .iter()
                .map(|(k, val)| AxProp {
                    name: (*k).into(),
                    value: AxValue {
                        value: Some(Value::String((*val).into())),
                    },
                })
                .collect(),
            children: kids.iter().map(|k| (*k).into()).collect(),
            backend: id.parse::<i64>().ok().map(|n| n + 100),
        }
    }

    #[test]
    fn wrap_keeps_styles_and_breaks_on_words() {
        let sp = |t: &str, region| Span {
            text: t.into(),
            fg: FG,
            attrs: Attrs::default(),
            region,
        };
        let lines = wrap(
            &[
                sp("hello big ", None),
                sp("link text here", Some(0)),
                sp(" tail", None),
            ],
            12,
            12,
        );
        let texts: Vec<String> = lines
            .iter()
            .map(|l| l.iter().map(|s| s.text.as_str()).collect::<String>())
            .collect();
        assert!(texts.iter().all(|t| str_width(t) <= 12), "{texts:?}");
        assert_eq!(
            texts.join(" ").split_whitespace().collect::<Vec<_>>(),
            ["hello", "big", "link", "text", "here", "tail"]
        );
        // the linked words keep their region
        let linked: String = lines
            .iter()
            .flatten()
            .filter(|s| s.region == Some(0))
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("|");
        assert!(
            linked.contains("link") && linked.contains("here"),
            "{linked}"
        );
    }

    #[test]
    fn long_words_are_hard_split() {
        let sp = Span {
            text: "x".repeat(30),
            fg: FG,
            attrs: Attrs::default(),
            region: None,
        };
        let lines = wrap(&[sp], 10, 10);
        assert!(lines
            .iter()
            .all(|l| l.iter().map(|s| str_width(&s.text)).sum::<usize>() <= 10));
        assert_eq!(
            lines.iter().flatten().map(|s| s.text.len()).sum::<usize>(),
            30
        );
    }

    #[test]
    fn heading_paragraph_link_list() {
        let t = AxTree {
            nodes: vec![
                mk("1", "RootWebArea", "T", &["2", "3", "5"], &[]),
                mk("2", "heading", "Title", &["20"], &[("level", "1")]),
                mk("3", "paragraph", "", &["21", "4"], &[]),
                mk("4", "link", "go", &["22"], &[("url", "https://x.org/")]),
                mk("5", "list", "", &["6"], &[]),
                mk("6", "listitem", "", &["7", "23"], &[]),
                mk("7", "ListMarker", "• ", &[], &[]),
                mk("20", "StaticText", "Title", &[], &[]),
                mk("21", "StaticText", "See ", &[], &[]),
                mk("22", "StaticText", "here", &[], &[]),
                mk("23", "StaticText", "item one", &[], &[]),
            ],
        };
        let doc = build(&t, &HashMap::new(), 60);
        let (g, regions) = doc.slice(0, 10, None);
        let text = g.dump_text();
        assert!(text.contains("# Title"), "{text}");
        assert!(text.contains("See here"), "{text}");
        assert!(text.contains("• item one"), "{text}");
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].href.as_deref(), Some("https://x.org/"));
        let r = &regions[0].rects[0];
        assert_eq!(g.cell(r.x, r.y).style.link, regions[0].id);
        assert_eq!(g.cell(r.x, r.y).g, "h");
    }

    #[test]
    fn text_after_a_control_gets_a_blank_but_links_do_not_add_spaces_inside_words() {
        let mut field = mk("3", "textbox", "", &[], &[]);
        field.value = Some(AxValue {
            value: Some(Value::String("ab".into())),
        });
        let t = AxTree {
            nodes: vec![
                mk("1", "RootWebArea", "", &["2"], &[]),
                mk("2", "paragraph", "", &["3", "4", "5", "7"], &[]),
                field,
                mk("4", "StaticText", "typed", &[], &[]),
                mk("5", "link", "x", &["6"], &[("url", "https://x.org/")]),
                mk("6", "StaticText", "link", &[], &[]),
                mk("7", "StaticText", "s", &[], &[]),
            ],
        };
        let text = build(&t, &HashMap::new(), 60)
            .slice(0, 3, None)
            .0
            .dump_text();
        assert!(text.contains("] typed"), "{text}");
        // a link ends and plain text continues without inventing a gap inside a word ("link" + "s")
        assert!(text.contains("linkS") || text.contains("links"), "{text}");
    }

    #[test]
    fn find_and_highlight() {
        let t = AxTree {
            nodes: vec![
                mk("1", "RootWebArea", "", &["2"], &[]),
                mk("2", "paragraph", "", &["3"], &[]),
                mk("3", "StaticText", "alpha Beta gamma beta", &[], &[]),
            ],
        };
        let doc = build(&t, &HashMap::new(), 60);
        assert_eq!(doc.find("beta", false), vec![0]);
        assert!(doc.find("beta", true).len() == 1); // "beta" lower-case occurs once
        assert!(doc.find("Zeta", false).is_empty());
        let (g, _) = doc.slice(0, 3, Some("beta"));
        let rev: usize = (0..60)
            .filter(|&x| g.cell(x, 0).style.attrs.contains(Attrs::REVERSE))
            .count();
        assert_eq!(rev, 8, "two matches of four letters are highlighted");
    }
}
