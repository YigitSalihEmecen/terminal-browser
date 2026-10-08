//! Debug aid: draw a cell grid as an HTML page, so Chromium can rasterise it to a PNG that can be
//! compared with the real page. Block-element characters are drawn as geometry (not font glyphs),
//! so the picture matches what a terminal with a proper block-drawing font shows.

use std::fmt::Write;

use glyph_proto::{width::cluster_width, Attrs, Grid, Rgb};

/// Quadrant bit masks (UL, UR, LL, LR) for the block elements we emit.
fn quadrants(g: &str) -> Option<[bool; 4]> {
    Some(match g {
        "▀" => [true, true, false, false],
        "▄" => [false, false, true, true],
        "▌" => [true, false, true, false],
        "▐" => [false, true, false, true],
        "█" => [true; 4],
        "▘" => [true, false, false, false],
        "▝" => [false, true, false, false],
        "▖" => [false, false, true, false],
        "▗" => [false, false, false, true],
        "▚" => [true, false, false, true],
        "▞" => [false, true, true, false],
        "▛" => [true, true, true, false],
        "▜" => [true, true, false, true],
        "▙" => [true, false, true, true],
        "▟" => [false, true, true, true],
        _ => return None,
    })
}

fn css(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Mix for the shade characters ░▒▓ (fg coverage).
fn shade(g: &str) -> Option<f32> {
    match g {
        "░" => Some(0.25),
        "▒" => Some(0.5),
        "▓" => Some(0.75),
        _ => None,
    }
}

pub fn grid_to_html(g: &Grid, cw: u32, ch: u32) -> String {
    let (w, h) = (g.cols() as u32 * cw, g.rows() as u32 * ch);
    let mut s = String::with_capacity(g.cols() as usize * g.rows() as usize * 60);
    let _ = write!(
        s,
        "<div class=g style=\"position:relative;width:{w}px;height:{h}px;overflow:hidden;background:#000\">"
    );
    for y in 0..g.rows() {
        for x in 0..g.cols() {
            let c = g.cell(x, y);
            if c.is_cont() {
                continue;
            }
            let wide = cluster_width(&c.g) == 2;
            let (cwid, st) = (if wide { 2 * cw } else { cw }, c.style);
            let (left, top) = (x as u32 * cw, y as u32 * ch);
            let _ = write!(
                s,
                "<i style=\"left:{left}px;top:{top}px;width:{cwid}px;height:{ch}px;background:{}",
                css(st.bg)
            );
            if let Some(q) = quadrants(&c.g) {
                s.push_str("\">");
                for (i, on) in q.iter().enumerate() {
                    if *on {
                        let (qx, qy) = ((i % 2) as u32 * cw / 2, (i / 2) as u32 * ch / 2);
                        let _ = write!(s, "<b style=\"left:{qx}px;top:{qy}px;width:{}px;height:{}px;background:{}\"></b>", cw / 2, ch / 2, css(st.fg));
                    }
                }
                s.push_str("</i>");
            } else if let Some(f) = shade(&c.g) {
                let mix = |a: u8, b: u8| (a as f32 * f + b as f32 * (1.0 - f)) as u8;
                let m = Rgb(
                    mix(st.fg.0, st.bg.0),
                    mix(st.fg.1, st.bg.1),
                    mix(st.fg.2, st.bg.2),
                );
                let _ = write!(s, ";background:{}\"></i>", css(m));
            } else if c.g == " " {
                s.push_str("\"></i>");
            } else {
                let mut deco = String::new();
                if st.attrs.contains(Attrs::UNDERLINE) {
                    deco.push_str("underline ");
                }
                if st.attrs.contains(Attrs::STRIKE) {
                    deco.push_str("line-through");
                }
                let _ = write!(
                    s,
                    ";color:{};{}{}{}\">{}</i>",
                    css(st.fg),
                    if st.attrs.contains(Attrs::BOLD) {
                        "font-weight:700;"
                    } else {
                        ""
                    },
                    if st.attrs.contains(Attrs::ITALIC) {
                        "font-style:italic;"
                    } else {
                        ""
                    },
                    if deco.is_empty() {
                        String::new()
                    } else {
                        format!("text-decoration:{deco};")
                    },
                    c.g.replace('&', "&amp;").replace('<', "&lt;")
                );
            }
        }
    }
    s.push_str("</div>");
    s
}

/// Page wrapper with the shared CSS (monospace cell font, absolutely placed cells).
pub fn wrap_page(body: &str, _cw: u32, ch: u32) -> String {
    let fs = (ch as f32 * 0.8).round();
    format!(
        "<!doctype html><meta charset=utf-8><style>body{{margin:0;background:#222;color:#ddd;font:12px sans-serif}}\
         .g i{{position:absolute;display:block;overflow:hidden;font:{fs}px/{ch}px Menlo,'DejaVu Sans Mono',monospace;white-space:pre;font-style:normal}}\
         .g b{{position:absolute;display:block}}\
         .panel{{display:inline-block;vertical-align:top;margin:0 6px 0 0}}.panel h4{{margin:2px 4px;font-weight:600}}</style>{body}",
    )
}

#[cfg(test)]
mod tests {
    use glyph_proto::Style;

    use super::*;

    #[test]
    fn draws_text_blocks_and_skips_continuations() {
        let mut g = Grid::new(6, 2, Style::new(Rgb(1, 2, 3), Rgb(9, 9, 9)));
        g.put_str(0, 0, "ab日", Style::new(Rgb(255, 0, 0), Rgb(0, 0, 0)), 6);
        g.put(0, 1, "▀", Style::new(Rgb(10, 20, 30), Rgb(40, 50, 60)));
        g.put(1, 1, "▚", Style::new(Rgb(1, 1, 1), Rgb(2, 2, 2)));
        let h = grid_to_html(&g, 8, 16);
        assert!(h.contains(">a</i>") && h.contains(">日</i>"));
        assert!(h.contains("width:16px"), "wide glyph spans two cells");
        // ▀ = two quadrant rectangles (upper half); ▚ = two diagonal ones
        assert_eq!(h.matches("<b ").count(), 4);
        assert!(h.contains("background:#0a141e"));
        assert_eq!(
            h.matches("<i ").count(),
            12 - 1,
            "continuation cell emits nothing"
        );
    }
}
