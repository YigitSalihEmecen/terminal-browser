//! Terminal graphics: Kitty, iTerm2 and Sixel. Half-block cells are always in the grid as the
//! fallback; when a protocol is available, real pixels are drawn over them.
//!
//! None of the emitters can be checked against a real terminal in CI, so each is a small pure
//! function with byte-level tests (the Sixel encoder is round-tripped through a test decoder).

use std::{collections::HashMap, io::Write};

use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{
        Attribute, Color as CtColor, Print, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
};
use glyph_proto::{Attrs, CellRect, GraphicsProto, ImageMsg, Rgb};

use crate::{
    app::{App, Mode, CONTENT_TOP},
    color::{rgb_to_16, rgb_to_256, ColorDepth},
};

// ---------------------------------------------------------------- detection

/// Which protocol to use. `config` is `images.protocol`; `env` looks up an environment variable.
pub fn detect_with(config: &str, env: &dyn Fn(&str) -> Option<String>) -> GraphicsProto {
    match config {
        "none" | "off" => return GraphicsProto::None,
        "kitty" => return GraphicsProto::Kitty,
        "sixel" => return GraphicsProto::Sixel,
        "iterm2" | "iterm" => return GraphicsProto::Iterm2,
        _ => {}
    }
    // multiplexers swallow or mangle graphics escapes unless configured; do not guess
    if env("TMUX").is_some() || env("STY").is_some() {
        return GraphicsProto::None;
    }
    let term = env("TERM").unwrap_or_default();
    let prog = env("TERM_PROGRAM").unwrap_or_default();
    if term == "xterm-kitty"
        || env("KITTY_WINDOW_ID").is_some()
        || prog == "ghostty"
        || term == "xterm-ghostty"
    {
        return GraphicsProto::Kitty;
    }
    if prog == "iTerm.app" || prog == "WezTerm" || env("LC_TERMINAL").as_deref() == Some("iTerm2") {
        return GraphicsProto::Iterm2;
    }
    if term.contains("sixel")
        || term == "foot"
        || term == "foot-extra"
        || term.starts_with("mlterm")
        || term == "contour"
    {
        return GraphicsProto::Sixel;
    }
    GraphicsProto::None
}

pub fn detect(config: &str) -> GraphicsProto {
    detect_with(config, &|k| std::env::var(k).ok())
}

// ---------------------------------------------------------------- emitters

pub fn cursor_to(x: u16, y: u16) -> String {
    format!("\x1b[{};{}H", y + 1, x + 1)
}

fn b64(data: &[u8]) -> String {
    crate::osc52::base64(data)
}

/// Kitty graphics: transmit raw RGB and display it over `cols × rows` cells at the cursor.
/// `q=2` silences terminal replies (they would arrive as keyboard input), `C=1` keeps the cursor.
pub fn kitty_transmit(id: u32, w: u16, h: u16, rgb: &[u8], cols: u16, rows: u16) -> Vec<u8> {
    let enc = b64(rgb);
    let mut out = Vec::with_capacity(enc.len() + enc.len() / 4096 * 16 + 64);
    let chunks: Vec<&[u8]> = enc.as_bytes().chunks(4096).collect();
    for (i, c) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            out.extend_from_slice(
                format!("\x1b_Ga=T,f=24,s={w},v={h},i={id},c={cols},r={rows},C=1,q=2,m={more};")
                    .as_bytes(),
            );
        } else {
            out.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
        }
        out.extend_from_slice(c);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

pub fn kitty_delete(id: u32) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

pub fn kitty_delete_all() -> String {
    "\x1b_Ga=d,d=A,q=2\x1b\\".to_owned()
}

/// iTerm2 inline image (the file is sent as is: JPEG/PNG), scaled to `cols × rows` cells.
pub fn iterm2(file: &[u8], cols: u16, rows: u16) -> String {
    format!(
        "\x1b]1337;File=inline=1;size={};width={cols};height={rows};preserveAspectRatio=0:{}\x07",
        file.len(),
        b64(file)
    )
}

/// Sixel with a fixed 6×6×6 colour cube (no dithering): small, fast, and deterministic.
pub fn sixel(rgb: &[u8], w: usize, h: usize) -> String {
    const LEVEL: [u8; 6] = [0, 51, 102, 153, 204, 255];
    let idx = |p: &[u8]| -> u8 {
        let q = |v: u8| ((v as u32 + 25) / 51).min(5) as u8;
        36 * q(p[0]) + 6 * q(p[1]) + q(p[2])
    };
    let mut out = String::with_capacity(w * h / 4 + 1024);
    out.push_str("\x1bPq");
    out.push_str(&format!("\"1;1;{w};{h}"));
    let pix: Vec<u8> = rgb.as_chunks::<3>().0.iter().map(|p| idx(p)).collect();
    let mut used = [false; 216];
    for &p in &pix {
        used[p as usize] = true;
    }
    for (i, u) in used.iter().enumerate() {
        if *u {
            let (r, g, b) = (LEVEL[i / 36], LEVEL[i / 6 % 6], LEVEL[i % 6]);
            out.push_str(&format!(
                "#{i};2;{};{};{}",
                r as u32 * 100 / 255,
                g as u32 * 100 / 255,
                b as u32 * 100 / 255
            ));
        }
    }
    for band in (0..h).step_by(6) {
        let rows = (h - band).min(6);
        let mut first_color = true;
        for (c, _) in used.iter().enumerate().filter(|(_, u)| **u) {
            // sixel column bytes for this colour in this band
            let mut cols: Vec<u8> = Vec::with_capacity(w);
            let mut any = false;
            for x in 0..w {
                let mut bits = 0u8;
                for r in 0..rows {
                    if pix[(band + r) * w + x] as usize == c {
                        bits |= 1 << r;
                    }
                }
                any |= bits != 0;
                cols.push(63 + bits);
            }
            if !any {
                continue;
            }
            if !first_color {
                out.push('$'); // back to the left edge, same band, next colour
            }
            first_color = false;
            out.push_str(&format!("#{c}"));
            let mut i = 0;
            while i < cols.len() {
                let mut run = 1;
                while i + run < cols.len() && cols[i + run] == cols[i] {
                    run += 1;
                }
                if run > 3 {
                    out.push_str(&format!("!{run}{}", cols[i] as char));
                } else {
                    for _ in 0..run {
                        out.push(cols[i] as char);
                    }
                }
                i += run;
            }
        }
        out.push('-'); // next band
    }
    out.push_str("\x1b\\");
    out
}

// ---------------------------------------------------------------- pixels

pub fn decode_jpeg(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    use zune_jpeg::{
        zune_core::{colorspace::ColorSpace, options::DecoderOptions},
        JpegDecoder,
    };
    let mut d = JpegDecoder::new_with_options(
        data,
        DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB),
    );
    let rgb = d.decode().ok()?;
    let info = d.info()?;
    (rgb.len() >= info.width as usize * info.height as usize * 3).then_some((
        info.width as usize,
        info.height as usize,
        rgb,
    ))
}

/// Bilinear resample of an RGB8 image.
pub fn scale(src: &[u8], sw: usize, sh: usize, dw: usize, dh: usize) -> Vec<u8> {
    if (sw, sh) == (dw, dh) || sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return src.to_vec();
    }
    let mut out = vec![0u8; dw * dh * 3];
    for y in 0..dh {
        let fy = ((y as f32 + 0.5) * sh as f32 / dh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let (y0, ty) = (fy as usize, fy.fract());
        let y1 = (y0 + 1).min(sh - 1);
        for x in 0..dw {
            let fx = ((x as f32 + 0.5) * sw as f32 / dw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let (x0, tx) = (fx as usize, fx.fract());
            let x1 = (x0 + 1).min(sw - 1);
            for c in 0..3 {
                let p = |xx: usize, yy: usize| src[(yy * sw + xx) * 3 + c] as f32;
                let top = p(x0, y0) * (1.0 - tx) + p(x1, y0) * tx;
                let bot = p(x0, y1) * (1.0 - tx) + p(x1, y1) * tx;
                out[(y * dw + x) * 3 + c] = (top * (1.0 - ty) + bot * ty).round() as u8;
            }
        }
    }
    out
}

fn ct_color(d: ColorDepth, c: Rgb) -> CtColor {
    match d {
        ColorDepth::True => CtColor::Rgb {
            r: c.0,
            g: c.1,
            b: c.2,
        },
        ColorDepth::C256 => CtColor::AnsiValue(rgb_to_256(c)),
        ColorDepth::C16 => CtColor::AnsiValue(rgb_to_16(c)),
    }
}

// ---------------------------------------------------------------- state & drawing

/// What is currently on screen for image `id`.
struct Shown {
    rect: CellRect,
    data_len: usize,
}

pub struct Images {
    proto: GraphicsProto,
    cell_px: (u16, u16),
    depth: ColorDepth,
    shown: HashMap<u32, Shown>,
    decoded: HashMap<u32, (usize, usize, std::rc::Rc<Vec<u8>>)>,
}

impl Images {
    pub fn new(proto: GraphicsProto, cell_px: (u16, u16), depth: ColorDepth) -> Self {
        Self {
            proto,
            cell_px: if cell_px.0 == 0 || cell_px.1 == 0 {
                (8, 16)
            } else {
                cell_px
            },
            depth,
            shown: HashMap::new(),
            decoded: HashMap::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.proto != GraphicsProto::None
    }

    fn decode(&mut self, m: &ImageMsg) -> Option<(usize, usize, std::rc::Rc<Vec<u8>>)> {
        if let Some(d) = self.decoded.get(&m.id) {
            return Some(d.clone());
        }
        let (w, h, rgb) = decode_jpeg(&m.data)?;
        let d = (w, h, std::rc::Rc::new(rgb));
        self.decoded.insert(m.id, d.clone());
        Some(d)
    }

    /// Bring the terminal in line with `app.images`. Call after the grid has been drawn.
    pub fn draw(&mut self, out: &mut impl Write, app: &mut App) -> Result<()> {
        if !self.active() {
            app.damage.clear();
            app.damage_all = false;
            return Ok(());
        }
        let (cols, rows) = app.content();
        let fits = |r: &CellRect| r.w > 0 && r.h > 0 && r.x + r.w <= cols && r.y + r.h <= rows;
        let hidden = matches!(app.mode, Mode::Help);
        let desired: Vec<ImageMsg> = if hidden {
            Vec::new()
        } else {
            app.images
                .values()
                .filter(|m| fits(&m.rect))
                .cloned()
                .collect()
        };
        let desired_ids: Vec<u32> = desired.iter().map(|m| m.id).collect();
        self.decoded.retain(|id, _| app.images.contains_key(id));

        let mut wrote = false;
        // remove what is gone (or moved: ids are position-derived, so a moved image has a new id)
        let gone: Vec<u32> = self
            .shown
            .keys()
            .filter(|id| !desired_ids.contains(id))
            .copied()
            .collect();
        for id in gone {
            let s = self.shown.remove(&id).expect("listed above");
            match self.proto {
                GraphicsProto::Kitty => out.write_all(kitty_delete(id).as_bytes())?,
                // cell-embedded protocols are erased by repainting the cells underneath
                _ => self.repaint(out, app, s.rect)?,
            }
            wrote = true;
        }
        for m in desired {
            let damaged = app.damage_all || app.damage.iter().any(|d| overlaps(d, &m.rect));
            let up_to_date = self
                .shown
                .get(&m.id)
                .is_some_and(|s| s.rect == m.rect && s.data_len == m.data.len());
            // Kitty keeps images in their own layer, so only (re)send on change; cell-embedded
            // protocols are overwritten whenever the cells under them are redrawn.
            let needs = !up_to_date || (self.proto != GraphicsProto::Kitty && damaged);
            if !needs {
                continue;
            }
            self.emit(out, &m)?;
            self.shown.insert(
                m.id,
                Shown {
                    rect: m.rect,
                    data_len: m.data.len(),
                },
            );
            wrote = true;
        }
        app.damage.clear();
        app.damage_all = false;
        if wrote {
            // the cursor moved while drawing images: put it back where the UI wants it
            match (&app.mode, app.cursor) {
                (Mode::Insert, Some(c)) => queue!(out, MoveTo(c.col, CONTENT_TOP + c.row), Show)?,
                _ => queue!(out, Hide)?,
            }
            out.flush()?;
        }
        Ok(())
    }

    /// Forget everything shown (tab switch, resize, protocol reset).
    pub fn reset(&mut self, out: &mut impl Write, app: &App) -> Result<()> {
        if self.proto == GraphicsProto::Kitty && !self.shown.is_empty() {
            out.write_all(kitty_delete_all().as_bytes())?;
            out.flush()?;
        } else if !self.shown.is_empty() {
            let rects: Vec<CellRect> = self.shown.values().map(|s| s.rect).collect();
            for r in rects {
                self.repaint(out, app, r)?;
            }
            out.flush()?;
        }
        self.shown.clear();
        self.decoded.clear();
        Ok(())
    }

    fn emit(&mut self, out: &mut impl Write, m: &ImageMsg) -> Result<()> {
        let (x, y) = (m.rect.x, CONTENT_TOP + m.rect.y);
        match self.proto {
            GraphicsProto::None => {}
            GraphicsProto::Kitty => {
                if let Some((w, h, rgb)) = self.decode(m) {
                    queue!(out, MoveTo(x, y))?;
                    out.write_all(&kitty_transmit(
                        m.id, w as u16, h as u16, &rgb, m.rect.w, m.rect.h,
                    ))?;
                }
            }
            GraphicsProto::Iterm2 => {
                queue!(out, MoveTo(x, y))?;
                out.write_all(iterm2(&m.data, m.rect.w, m.rect.h).as_bytes())?;
            }
            GraphicsProto::Sixel => {
                if let Some((w, h, rgb)) = self.decode(m) {
                    let (dw, dh) = (
                        m.rect.w as usize * self.cell_px.0 as usize,
                        m.rect.h as usize * self.cell_px.1 as usize,
                    );
                    let scaled = scale(&rgb, w, h, dw.min(2400), dh.min(1600));
                    queue!(out, MoveTo(x, y))?;
                    out.write_all(sixel(&scaled, dw.min(2400), dh.min(1600)).as_bytes())?;
                }
            }
        }
        Ok(())
    }

    /// Redraw the page cells of `r` straight from the grid (erases an embedded image).
    fn repaint(&self, out: &mut impl Write, app: &App, r: CellRect) -> Result<()> {
        let Some(g) = &app.grid else { return Ok(()) };
        for y in r.y..(r.y + r.h).min(g.rows()) {
            queue!(out, MoveTo(r.x, CONTENT_TOP + y))?;
            let mut x = r.x;
            while x < (r.x + r.w).min(g.cols()) {
                let c = g.cell(x, y);
                if c.is_cont() {
                    x += 1;
                    continue;
                }
                let a = c.style.attrs;
                queue!(
                    out,
                    SetAttribute(Attribute::Reset),
                    SetForegroundColor(ct_color(self.depth, c.style.fg)),
                    SetBackgroundColor(ct_color(self.depth, c.style.bg))
                )?;
                for (flag, attr) in [
                    (Attrs::BOLD, Attribute::Bold),
                    (Attrs::ITALIC, Attribute::Italic),
                    (Attrs::UNDERLINE, Attribute::Underlined),
                    (Attrs::STRIKE, Attribute::CrossedOut),
                ] {
                    if a.contains(flag) {
                        queue!(out, SetAttribute(attr))?;
                    }
                }
                queue!(out, Print(c.g.as_str()))?;
                x += 1;
            }
            queue!(out, SetAttribute(Attribute::Reset))?;
        }
        Ok(())
    }
}

fn overlaps(a: &CellRect, b: &CellRect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, Mode};

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detection_table() {
        let d = |cfg: &str, e: &[(&str, &str)]| detect_with(cfg, &env(e));
        assert_eq!(d("auto", &[("TERM", "xterm-kitty")]), GraphicsProto::Kitty);
        assert_eq!(
            d(
                "auto",
                &[("KITTY_WINDOW_ID", "3"), ("TERM", "xterm-256color")]
            ),
            GraphicsProto::Kitty
        );
        assert_eq!(
            d("auto", &[("TERM_PROGRAM", "ghostty")]),
            GraphicsProto::Kitty
        );
        assert_eq!(
            d("auto", &[("TERM_PROGRAM", "iTerm.app")]),
            GraphicsProto::Iterm2
        );
        assert_eq!(
            d("auto", &[("TERM_PROGRAM", "WezTerm")]),
            GraphicsProto::Iterm2
        );
        assert_eq!(d("auto", &[("TERM", "foot")]), GraphicsProto::Sixel);
        assert_eq!(
            d("auto", &[("TERM", "xterm-256color")]),
            GraphicsProto::None
        );
        assert_eq!(d("auto", &[]), GraphicsProto::None);
        // inside tmux/screen we do not guess
        assert_eq!(
            d("auto", &[("TERM", "xterm-kitty"), ("TMUX", "/tmp/x")]),
            GraphicsProto::None
        );
        // config always wins
        assert_eq!(d("sixel", &[("TERM", "xterm-kitty")]), GraphicsProto::Sixel);
        assert_eq!(d("none", &[("TERM", "xterm-kitty")]), GraphicsProto::None);
        assert_eq!(d("kitty", &[("TMUX", "x")]), GraphicsProto::Kitty);
    }

    #[test]
    fn kitty_sequences_are_chunked_and_quiet() {
        let rgb = vec![7u8; 100 * 50 * 3];
        let seq = String::from_utf8(kitty_transmit(42, 100, 50, &rgb, 12, 3)).unwrap();
        assert!(
            seq.starts_with("\x1b_Ga=T,f=24,s=100,v=50,i=42,c=12,r=3,C=1,q=2,m=1;"),
            "{}",
            &seq[..80]
        );
        let parts: Vec<&str> = seq.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert!(parts.len() >= 3, "15 KB of RGB needs several 4 KB chunks");
        assert!(parts[1].starts_with("\x1b_Gm=1;"));
        assert!(parts.last().unwrap().starts_with("\x1b_Gm=0;"));
        // reassembling the payloads gives back the pixels
        let payload: String = parts.iter().map(|p| p.split_once(';').unwrap().1).collect();
        assert_eq!(payload, crate::osc52::base64(&rgb));
        assert_eq!(kitty_delete(42), "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
        assert_eq!(kitty_delete_all(), "\x1b_Ga=d,d=A,q=2\x1b\\");
    }

    #[test]
    fn small_kitty_image_is_one_final_chunk() {
        let seq = String::from_utf8(kitty_transmit(1, 1, 1, &[1, 2, 3], 1, 1)).unwrap();
        assert!(seq.contains("m=0;") && !seq.contains("m=1"), "{seq}");
    }

    #[test]
    fn iterm2_header() {
        let s = iterm2(b"abc", 20, 6);
        assert_eq!(
            s,
            "\x1b]1337;File=inline=1;size=3;width=20;height=6;preserveAspectRatio=0:YWJj\x07"
        );
    }

    /// Minimal Sixel decoder: enough to check the encoder end to end.
    fn decode_sixel(s: &str) -> (usize, usize, Vec<[u8; 3]>) {
        let body = s
            .strip_prefix("\x1bPq")
            .unwrap()
            .strip_suffix("\x1b\\")
            .unwrap();
        let mut chars = body.chars().peekable();
        let (mut w, mut h) = (0usize, 0usize);
        let mut palette: HashMap<usize, [u8; 3]> = HashMap::new();
        let mut cur = 0usize;
        let mut px: Vec<Vec<Option<[u8; 3]>>> = Vec::new();
        let (mut x, mut band) = (0usize, 0usize);
        let num = |it: &mut std::iter::Peekable<std::str::Chars>| {
            let mut n = String::new();
            while let Some(&c) = it.peek() {
                if c.is_ascii_digit() {
                    n.push(c);
                    it.next();
                } else {
                    break;
                }
            }
            n.parse::<usize>().unwrap_or(0)
        };
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    num(&mut chars);
                    chars.next();
                    num(&mut chars);
                    chars.next();
                    w = num(&mut chars);
                    chars.next();
                    h = num(&mut chars);
                    px = vec![vec![None; w]; h];
                }
                '#' => {
                    cur = num(&mut chars);
                    if chars.peek() == Some(&';') {
                        chars.next();
                        assert_eq!(num(&mut chars), 2, "RGB colour space");
                        chars.next();
                        let r = num(&mut chars);
                        chars.next();
                        let g = num(&mut chars);
                        chars.next();
                        let b = num(&mut chars);
                        palette.insert(
                            cur,
                            [
                                (r * 255 / 100) as u8,
                                (g * 255 / 100) as u8,
                                (b * 255 / 100) as u8,
                            ],
                        );
                    }
                }
                '$' => x = 0,
                '-' => {
                    x = 0;
                    band += 1;
                }
                '!' => {
                    let n = num(&mut chars);
                    let ch = chars.next().unwrap();
                    for _ in 0..n {
                        put(&mut px, &palette, cur, x, band, ch);
                        x += 1;
                    }
                }
                '?'..='~' => {
                    put(&mut px, &palette, cur, x, band, c);
                    x += 1;
                }
                _ => panic!("unexpected {c:?}"),
            }
        }
        (
            w,
            h,
            px.into_iter()
                .flatten()
                .map(|p| p.unwrap_or([0, 0, 0]))
                .collect(),
        )
    }

    fn put(
        px: &mut [Vec<Option<[u8; 3]>>],
        pal: &HashMap<usize, [u8; 3]>,
        cur: usize,
        x: usize,
        band: usize,
        ch: char,
    ) {
        let bits = ch as u8 - 63;
        for r in 0..6 {
            if bits & (1 << r) != 0 && band * 6 + r < px.len() && x < px[0].len() {
                px[band * 6 + r][x] = Some(pal[&cur]);
            }
        }
    }

    #[test]
    fn sixel_round_trips_within_palette_error() {
        // a 40×14 image (not a multiple of 6 tall) with flat blocks and a gradient
        let (w, h) = (40usize, 14usize);
        let mut rgb = Vec::new();
        for y in 0..h {
            for x in 0..w {
                rgb.extend_from_slice(&if x < 10 {
                    [255, 0, 0]
                } else if x < 20 {
                    [0, 0, 255]
                } else if y < 7 {
                    [255, 255, 255]
                } else {
                    [(x * 6) as u8, (y * 18) as u8, 100]
                });
            }
        }
        let s = sixel(&rgb, w, h);
        assert!(s.starts_with("\x1bPq\"1;1;40;14") && s.ends_with("\x1b\\"));
        let (dw, dh, dec) = decode_sixel(&s);
        assert_eq!((dw, dh), (w, h));
        for (i, d) in dec.iter().enumerate() {
            let o = &rgb[i * 3..i * 3 + 3];
            for c in 0..3 {
                assert!(
                    (d[c] as i32 - o[c] as i32).abs() <= 26 + 2,
                    "pixel {i} channel {c}: {d:?} vs {o:?}"
                );
            }
        }
        // exact flat colours survive exactly
        assert_eq!(dec[0], [255, 0, 0]);
        assert_eq!(dec[15], [0, 0, 255]);
    }

    #[test]
    fn sixel_runs_are_rle_compressed() {
        let rgb = vec![255u8; 100 * 6 * 3];
        let s = sixel(&rgb, 100, 6);
        assert!(s.contains("!100~"), "{s}");
        assert!(s.len() < 200);
    }

    #[test]
    fn scaling_identity_and_resize() {
        let src: Vec<u8> = (0..4 * 4 * 3).map(|i| i as u8).collect();
        assert_eq!(scale(&src, 4, 4, 4, 4), src);
        let flat = vec![100u8; 4 * 4 * 3];
        let up = scale(&flat, 4, 4, 9, 7);
        assert_eq!(up.len(), 9 * 7 * 3);
        assert!(up.iter().all(|&v| v == 100));
        let down = scale(&flat, 4, 4, 2, 2);
        assert!(down.iter().all(|&v| v == 100));
        // black/white halves stay black and white at the extremes
        let mut bw = vec![0u8; 8 * 2 * 3];
        for y in 0..2 {
            for x in 4..8 {
                bw[(y * 8 + x) * 3..(y * 8 + x) * 3 + 3].copy_from_slice(&[255, 255, 255]);
            }
        }
        let s = scale(&bw, 8, 2, 16, 4);
        assert_eq!(s[0], 0);
        assert_eq!(s[(15) * 3], 255);
    }

    #[test]
    fn overlap_rules() {
        let a = CellRect {
            x: 0,
            y: 0,
            w: 4,
            h: 2,
        };
        assert!(overlaps(
            &a,
            &CellRect {
                x: 3,
                y: 1,
                w: 2,
                h: 2
            }
        ));
        assert!(!overlaps(
            &a,
            &CellRect {
                x: 4,
                y: 0,
                w: 2,
                h: 2
            }
        ));
        assert!(!overlaps(
            &a,
            &CellRect {
                x: 0,
                y: 2,
                w: 2,
                h: 2
            }
        ));
    }

    // ---- the draw loop, against a capturing writer

    fn jpeg(w: u16, h: u16, rgb: [u8; 3]) -> Vec<u8> {
        let px: Vec<u8> = (0..w as usize * h as usize).flat_map(|_| rgb).collect();
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90)
            .encode(&px, w, h, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    fn app_with_image() -> (App, ImageMsg) {
        use glyph_proto::{apply_runs, full_runs, Grid, ImageFormat, ServerMsg, Style};
        let mut a = App::new(crate::Config::default(), (80, 24));
        let mut g = Grid::new(80, 21, Style::default());
        g.put_str(0, 0, "page text", Style::default(), 80);
        a.on_server(ServerMsg::FullFrame {
            tab: 1,
            seq: 1,
            cols: 80,
            rows: 21,
            runs: full_runs(&g),
        });
        let _ = apply_runs;
        let m = ImageMsg {
            tab: 1,
            id: 77,
            rect: CellRect {
                x: 2,
                y: 3,
                w: 10,
                h: 4,
            },
            px_w: 16,
            px_h: 16,
            format: ImageFormat::Jpeg,
            data: jpeg(16, 16, [200, 30, 30]),
        };
        a.on_server(ServerMsg::Image(m.clone()));
        (a, m)
    }

    fn drawn(proto: GraphicsProto, a: &mut App, im: &mut Images) -> String {
        let mut out = Vec::new();
        let _ = proto;
        im.draw(&mut out, a).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn kitty_draws_once_then_only_deletes() {
        let (mut a, m) = app_with_image();
        let mut im = Images::new(GraphicsProto::Kitty, (8, 16), ColorDepth::True);
        let first = drawn(GraphicsProto::Kitty, &mut a, &mut im);
        assert!(
            first.contains("\x1b[6;3H"),
            "cursor to (2, CONTENT_TOP+3): {first:?}"
        );
        assert!(
            first.contains("\x1b_Ga=T,f=24,s=16,v=16,i=77,c=10,r=4"),
            "{first:?}"
        );
        // nothing changed, and damage alone must not retransmit a Kitty image (own layer)
        a.damage.push(m.rect);
        assert_eq!(drawn(GraphicsProto::Kitty, &mut a, &mut im), "");
        // removing it deletes exactly that placement
        a.images.clear();
        let gone = drawn(GraphicsProto::Kitty, &mut a, &mut im);
        assert!(gone.contains("\x1b_Ga=d,d=I,i=77"), "{gone:?}");
        // an image that does not fit the content area is ignored, not drawn off-screen
        let mut bad = m.clone();
        bad.rect = CellRect {
            x: 75,
            y: 3,
            w: 10,
            h: 4,
        };
        a.images.insert(bad.id, bad);
        assert_eq!(drawn(GraphicsProto::Kitty, &mut a, &mut im), "");
    }

    #[test]
    fn iterm2_redraws_when_cells_under_it_change_and_repaints_on_removal() {
        let (mut a, m) = app_with_image();
        let mut im = Images::new(GraphicsProto::Iterm2, (8, 16), ColorDepth::True);
        let first = drawn(GraphicsProto::Iterm2, &mut a, &mut im);
        assert!(first.contains("\x1b]1337;File=inline=1;"), "{first:?}");
        assert!(first.contains("width=10;height=4"));
        assert_eq!(
            drawn(GraphicsProto::Iterm2, &mut a, &mut im),
            "",
            "idle: no repeat"
        );
        a.damage.push(CellRect {
            x: 4,
            y: 4,
            w: 2,
            h: 1,
        }); // text redrawn inside the image
        assert!(drawn(GraphicsProto::Iterm2, &mut a, &mut im).contains("1337;File="));
        a.damage.push(CellRect {
            x: 40,
            y: 10,
            w: 2,
            h: 1,
        }); // elsewhere: leave it alone
        assert_eq!(drawn(GraphicsProto::Iterm2, &mut a, &mut im), "");
        // removal repaints the covered page cells (no delete command exists for this protocol)
        a.images.clear();
        let gone = drawn(GraphicsProto::Iterm2, &mut a, &mut im);
        assert!(
            gone.contains("\x1b[6;3H") && gone.contains("\x1b[0m"),
            "{gone:?}"
        );
        let _ = m;
    }

    #[test]
    fn sixel_output_is_scaled_to_the_cell_box() {
        let (mut a, _) = app_with_image();
        let mut im = Images::new(GraphicsProto::Sixel, (9, 18), ColorDepth::True);
        let s = drawn(GraphicsProto::Sixel, &mut a, &mut im);
        // 10×4 cells at 9×18 px = 90×72
        assert!(
            s.contains("\x1bPq\"1;1;90;72"),
            "{:?}",
            &s[..s.len().min(120)]
        );
    }

    #[test]
    fn help_overlay_hides_graphics_and_none_protocol_is_inert() {
        let (mut a, _) = app_with_image();
        let mut off = Images::new(GraphicsProto::None, (8, 16), ColorDepth::True);
        assert_eq!(drawn(GraphicsProto::None, &mut a, &mut off), "");
        let mut im = Images::new(GraphicsProto::Kitty, (8, 16), ColorDepth::True);
        drawn(GraphicsProto::Kitty, &mut a, &mut im);
        a.mode = Mode::Help;
        assert!(
            drawn(GraphicsProto::Kitty, &mut a, &mut im).contains("a=d,d=I,i=77"),
            "the overlay must not be covered by a picture"
        );
    }

    #[test]
    fn reset_clears_every_placement() {
        let (mut a, _) = app_with_image();
        let mut im = Images::new(GraphicsProto::Kitty, (8, 16), ColorDepth::True);
        drawn(GraphicsProto::Kitty, &mut a, &mut im);
        let mut out = Vec::new();
        im.reset(&mut out, &a).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), kitty_delete_all());
        // after a reset the same image is drawn again
        assert!(drawn(GraphicsProto::Kitty, &mut a, &mut im).contains("a=T"));
    }
}
