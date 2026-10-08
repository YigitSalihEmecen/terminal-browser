//! Typed subset of `DOMSnapshot.captureSnapshot`, plus colour parsing.
//!
//! Every field is `#[serde(default)]`: a missing array degrades to "nothing", never an error,
//! so a Chromium that renames or drops a field costs fidelity rather than the whole frame.

use std::collections::HashMap;

use glyph_proto::Rgb;
use serde::Deserialize;

/// Computed styles requested from Chromium, in this order (indices are the `st::*` consts).
pub const COMPUTED_STYLES: &[&str] = &[
    "color",
    "background-color",
    "font-weight",
    "font-style",
    "text-decoration-line",
    "visibility",
    "opacity",
    "overflow-x",
    "overflow-y",
    "position",
    "background-image",
    "font-size",
    "border-top-width",
    "border-top-style",
    "border-top-color",
    "border-right-width",
    "border-right-style",
    "border-right-color",
    "border-bottom-width",
    "border-bottom-style",
    "border-bottom-color",
    "border-left-width",
    "border-left-style",
    "border-left-color",
    "border-top-left-radius",
    "cursor",
];

pub mod st {
    pub const COLOR: usize = 0;
    pub const BG: usize = 1;
    pub const WEIGHT: usize = 2;
    pub const FONT_STYLE: usize = 3;
    pub const DECORATION: usize = 4;
    pub const VISIBILITY: usize = 5;
    pub const OPACITY: usize = 6;
    pub const OVERFLOW_X: usize = 7;
    pub const OVERFLOW_Y: usize = 8;
    pub const POSITION: usize = 9;
    pub const BG_IMAGE: usize = 10;
    pub const FONT_SIZE: usize = 11;
    /// top, right, bottom, left: each (width, style, color) at `BORDER + 3*side + {0,1,2}`.
    pub const BORDER: usize = 12;
    pub const RADIUS: usize = 24;
    pub const CURSOR: usize = 25;
}

#[derive(Debug, Default, Deserialize)]
pub struct SnapshotResult {
    #[serde(default)]
    pub documents: Vec<Document>,
    #[serde(default)]
    pub strings: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Document {
    #[serde(default, rename = "baseURL")]
    pub base_url: i32,
    #[serde(default)]
    pub nodes: Nodes,
    #[serde(default)]
    pub layout: Layout,
    #[serde(default, rename = "textBoxes")]
    pub text_boxes: TextBoxes,
    #[serde(default, rename = "scrollOffsetX")]
    pub scroll_x: f64,
    #[serde(default, rename = "scrollOffsetY")]
    pub scroll_y: f64,
    #[serde(default, rename = "contentWidth")]
    pub content_w: f64,
    #[serde(default, rename = "contentHeight")]
    pub content_h: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct Nodes {
    #[serde(default, rename = "parentIndex")]
    pub parent: Vec<i32>,
    #[serde(default, rename = "nodeType")]
    pub node_type: Vec<i32>,
    #[serde(default, rename = "nodeName")]
    pub node_name: Vec<i32>,
    #[serde(default, rename = "nodeValue")]
    pub node_value: Vec<i32>,
    #[serde(default, rename = "backendNodeId")]
    pub backend_id: Vec<i64>,
    #[serde(default)]
    pub attributes: Vec<Vec<i32>>,
    #[serde(default, rename = "inputValue")]
    pub input_value: RareString,
    #[serde(default, rename = "inputChecked")]
    pub input_checked: RareBool,
    #[serde(default, rename = "optionSelected")]
    pub option_selected: RareBool,
    #[serde(default, rename = "isClickable")]
    pub is_clickable: RareBool,
    #[serde(default, rename = "contentDocumentIndex")]
    pub content_document: RareInt,
}

#[derive(Debug, Default, Deserialize)]
pub struct RareString {
    #[serde(default)]
    pub index: Vec<i32>,
    #[serde(default)]
    pub value: Vec<i32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RareBool {
    #[serde(default)]
    pub index: Vec<i32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RareInt {
    #[serde(default)]
    pub index: Vec<i32>,
    #[serde(default)]
    pub value: Vec<i32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Layout {
    #[serde(default, rename = "nodeIndex")]
    pub node_index: Vec<i32>,
    #[serde(default)]
    pub styles: Vec<Vec<i32>>,
    #[serde(default)]
    pub bounds: Vec<Vec<f64>>,
    #[serde(default)]
    pub text: Vec<i32>,
    #[serde(default, rename = "paintOrders")]
    pub paint_orders: Vec<i32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct TextBoxes {
    #[serde(default, rename = "layoutIndex")]
    pub layout_index: Vec<i32>,
    #[serde(default)]
    pub bounds: Vec<Vec<f64>>,
    #[serde(default)]
    pub start: Vec<i32>,
    #[serde(default)]
    pub length: Vec<i32>,
}

/// Look-ups over one document.
pub struct DocView<'a> {
    pub strings: &'a [String],
    pub doc: &'a Document,
    pub input_value: HashMap<i32, i32>,
    pub checked: std::collections::HashSet<i32>,
    pub selected: std::collections::HashSet<i32>,
    pub clickable: std::collections::HashSet<i32>,
    pub child_doc: HashMap<i32, i32>,
}

impl<'a> DocView<'a> {
    pub fn new(strings: &'a [String], doc: &'a Document) -> Self {
        let n = &doc.nodes;
        Self {
            strings,
            doc,
            input_value: n
                .input_value
                .index
                .iter()
                .copied()
                .zip(n.input_value.value.iter().copied())
                .collect(),
            checked: n.input_checked.index.iter().copied().collect(),
            selected: n.option_selected.index.iter().copied().collect(),
            clickable: n.is_clickable.index.iter().copied().collect(),
            child_doc: n
                .content_document
                .index
                .iter()
                .copied()
                .zip(n.content_document.value.iter().copied())
                .collect(),
        }
    }

    pub fn s(&self, i: i32) -> &'a str {
        if i < 0 {
            ""
        } else {
            self.strings.get(i as usize).map_or("", String::as_str)
        }
    }

    pub fn tag(&self, node: usize) -> &'a str {
        self.s(self.doc.nodes.node_name.get(node).copied().unwrap_or(-1))
    }

    pub fn attr(&self, node: usize, name: &str) -> Option<&'a str> {
        let a = self.doc.nodes.attributes.get(node)?;
        a.as_chunks::<2>()
            .0
            .iter()
            .find(|p| self.s(p[0]).eq_ignore_ascii_case(name))
            .map(|p| self.s(p[1]))
    }

    pub fn style(&self, layout: usize, idx: usize) -> &'a str {
        self.doc
            .layout
            .styles
            .get(layout)
            .and_then(|s| s.get(idx))
            .map_or("", |&i| self.s(i))
    }

    pub fn bounds(&self, layout: usize) -> Option<[f64; 4]> {
        let b = self.doc.layout.bounds.get(layout)?;
        (b.len() >= 4).then(|| [b[0], b[1], b[2], b[3]])
    }
}

// ---------------------------------------------------------------- colour & number parsing

/// Parse a computed CSS colour (`rgb()`, `rgba()`, `color(srgb …)`). Returns colour and alpha 0..=255.
pub fn parse_color(s: &str) -> Option<(Rgb, u8)> {
    let s = s.trim();
    let (func, rest) = s.split_once('(')?;
    let rest = rest.strip_suffix(')')?;
    let nums: Vec<&str> = rest
        .split([',', ' ', '/'])
        .filter(|t| !t.is_empty())
        .collect();
    let num = |t: &str, scale: f32| -> Option<f32> {
        if let Some(p) = t.strip_suffix('%') {
            p.parse::<f32>().ok().map(|v| v / 100.0 * 255.0)
        } else {
            t.parse::<f32>().ok().map(|v| v * scale)
        }
    };
    let alpha = |t: Option<&&str>| -> Option<u8> {
        match t {
            None => Some(255),
            Some(t) => {
                let v = if let Some(p) = t.strip_suffix('%') {
                    p.parse::<f32>().ok()? / 100.0
                } else {
                    t.parse::<f32>().ok()?
                };
                Some((v.clamp(0.0, 1.0) * 255.0).round() as u8)
            }
        }
    };
    let c = |v: f32| v.clamp(0.0, 255.0).round() as u8;
    match func {
        "rgb" | "rgba" if nums.len() >= 3 => Some((
            Rgb(
                c(num(nums[0], 1.0)?),
                c(num(nums[1], 1.0)?),
                c(num(nums[2], 1.0)?),
            ),
            alpha(nums.get(3))?,
        )),
        "color" if nums.len() >= 4 && nums[0] == "srgb" => Some((
            Rgb(
                c(num(nums[1], 255.0)?),
                c(num(nums[2], 255.0)?),
                c(num(nums[3], 255.0)?),
            ),
            alpha(nums.get(4))?,
        )),
        _ => None,
    }
}

/// `"12.5px"` → `12.5`; anything else → 0.
pub fn parse_px(s: &str) -> f64 {
    s.strip_suffix("px")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some((Rgb(1, 2, 3), 255)));
        assert_eq!(parse_color("rgba(0, 0, 0, 0)"), Some((Rgb(0, 0, 0), 0)));
        assert_eq!(
            parse_color("rgba(10, 20, 30, 0.5)"),
            Some((Rgb(10, 20, 30), 128))
        );
        assert_eq!(
            parse_color("rgb(0 128 255 / 50%)"),
            Some((Rgb(0, 128, 255), 128))
        );
        assert_eq!(
            parse_color("color(srgb 1 0 0.5)"),
            Some((Rgb(255, 0, 128), 255))
        );
        assert_eq!(parse_color("transparent"), None);
        assert_eq!(parse_px("12.5px"), 12.5);
        assert_eq!(parse_px("normal"), 0.0);
    }
}
