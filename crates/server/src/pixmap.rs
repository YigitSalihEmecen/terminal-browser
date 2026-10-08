//! Decoded screencast pixels and per-cell colour statistics.

use anyhow::{anyhow, Result};
use glyph_proto::Rgb;

pub struct Pixmap {
    pub w: usize,
    pub h: usize,
    /// RGB8, row-major.
    pub rgb: Vec<u8>,
}

/// Cell size in page pixels and the pixmap-to-page scale factors.
#[derive(Clone, Copy, Debug)]
pub struct CellGeom {
    pub cw: f64,
    pub ch: f64,
    pub sx: f64,
    pub sy: f64,
}

impl CellGeom {
    /// Pixel rectangle `(x0, y0, x1, y1)` of the cell rectangle `[c0,c1) × [r0,r1)`.
    pub fn px_rect(&self, c0: i32, r0: i32, c1: i32, r1: i32) -> (usize, usize, usize, usize) {
        let f = |v: f64| v.max(0.0) as usize;
        (
            f(c0 as f64 * self.cw * self.sx),
            f(r0 as f64 * self.ch * self.sy),
            f(c1 as f64 * self.cw * self.sx),
            f(r1 as f64 * self.ch * self.sy),
        )
    }
}

/// Colour summary of one terminal cell.
#[derive(Clone, Copy, Debug)]
pub struct CellColors {
    pub top: Rgb,
    pub bottom: Rgb,
    /// Most common colour (ignores glyph strokes on a plain background).
    pub dominant: Rgb,
}

impl Pixmap {
    /// Mean colour of a pixel rectangle (clamped to the image; every pixel for small rectangles).
    pub fn mean_rect(&self, r: (usize, usize, usize, usize)) -> Rgb {
        let (x0, y0, x1, y1) = (
            r.0.min(self.w),
            r.1.min(self.h),
            r.2.min(self.w),
            r.3.min(self.h),
        );
        if x1 <= x0 || y1 <= y0 {
            return Rgb::WHITE;
        }
        let step = if (x1 - x0) * (y1 - y0) > 4096 { 2 } else { 1 };
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        let mut y = y0;
        while y < y1 {
            let row = &self.rgb[(y * self.w + x0) * 3..(y * self.w + x1) * 3];
            #[allow(clippy::chunks_exact_to_as_chunks)]
            for px in row.chunks_exact(3).step_by(step) {
                r += px[0] as u32;
                g += px[1] as u32;
                b += px[2] as u32;
                n += 1;
            }
            y += step;
        }
        let n = n.max(1);
        Rgb((r / n) as u8, (g / n) as u8, (b / n) as u8)
    }

    /// The four sub-cell colours of a cell rectangle: upper-left, upper-right, lower-left, lower-right.
    pub fn quadrants(&self, r: (usize, usize, usize, usize)) -> [Rgb; 4] {
        let (x0, y0, x1, y1) = r;
        let (xm, ym) = ((x0 + x1) / 2, (y0 + y1) / 2);
        [
            self.mean_rect((x0, y0, xm.max(x0 + 1), ym.max(y0 + 1))),
            self.mean_rect((xm, y0, x1.max(xm + 1), ym.max(y0 + 1))),
            self.mean_rect((x0, ym, xm.max(x0 + 1), y1.max(ym + 1))),
            self.mean_rect((xm, ym, x1.max(xm + 1), y1.max(ym + 1))),
        ]
    }

    /// The most common colour in a rectangle, ignoring pixels within `ink_dist2` of `ink` (the
    /// text colour); `prefer` is a colour the caller expects (CSS). Samples a lattice of at most ~400 points, buckets them at 4 bits per channel
    /// and returns the mean of the winning bucket. `None` when nothing but ink was seen.
    pub fn mode_excluding(
        &self,
        r: (usize, usize, usize, usize),
        ink: Option<Rgb>,
        ink_dist2: u32,
        prefer: Option<Rgb>,
    ) -> Option<Rgb> {
        let (x0, y0, x1, y1) = (
            r.0.min(self.w),
            r.1.min(self.h),
            r.2.min(self.w),
            r.3.min(self.h),
        );
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let area = (x1 - x0) * (y1 - y0);
        let step = ((area as f64 / 400.0).sqrt().ceil() as usize).max(1);
        // (bucket key, count, sums)
        let mut buckets: Vec<(u16, u32, [u32; 3])> = Vec::with_capacity(32);
        let mut kept = 0u32;
        let mut y = y0 + step / 2;
        while y < y1 {
            let mut x = x0 + step / 2;
            while x < x1 {
                let i = (y * self.w + x) * 3;
                let px = Rgb(self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]);
                if !ink.is_some_and(|c| c.dist2(px) < ink_dist2) {
                    kept += 1;
                    let key =
                        ((px.0 as u16 >> 4) << 8) | ((px.1 as u16 >> 4) << 4) | (px.2 as u16 >> 4);
                    match buckets.iter_mut().find(|b| b.0 == key) {
                        Some(b) => {
                            b.1 += 1;
                            b.2[0] += px.0 as u32;
                            b.2[1] += px.1 as u32;
                            b.2[2] += px.2 as u32;
                        }
                        None => buckets.push((key, 1, [px.0 as u32, px.1 as u32, px.2 as u32])),
                    }
                }
                x += step;
            }
            y += step;
        }
        if kept < 2 {
            return None;
        }
        // one colour often straddles a bucket boundary (JPEG noise), so score each bucket together
        // with the buckets of similar colour, and answer with that cluster's mean
        let mean = |b: &(u16, u32, [u32; 3])| {
            Rgb(
                (b.2[0] / b.1) as u8,
                (b.2[1] / b.1) as u8,
                (b.2[2] / b.1) as u8,
            )
        };
        let near =
            |a: &(u16, u32, [u32; 3]), b: &(u16, u32, [u32; 3])| mean(a).dist2(mean(b)) < 1500;
        // the CSS-declared colour wins whenever the pixels back it up with a real share: a box that
        // is taller than its text (pill, padding) otherwise loses the vote to the colour around it
        if let Some(want) = prefer {
            let (mut n, mut s) = (0u32, [0u32; 3]);
            for b in buckets.iter().filter(|b| mean(b).dist2(want) < 3000) {
                n += b.1;
                (0..3).for_each(|k| s[k] += b.2[k]);
            }
            if n * 10 >= kept * 3 {
                return Some(Rgb((s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8));
            }
        }
        let best = buckets.iter().max_by_key(|a| {
            buckets
                .iter()
                .filter(|b| near(a, b))
                .map(|b| b.1)
                .sum::<u32>()
        })?;
        let (mut n, mut s) = (0u32, [0u32; 3]);
        for b in buckets.iter().filter(|b| near(best, b)) {
            n += b.1;
            (0..3).for_each(|k| s[k] += b.2[k]);
        }
        Some(Rgb((s[0] / n) as u8, (s[1] / n) as u8, (s[2] / n) as u8))
    }

    pub fn decode_jpeg(data: &[u8]) -> Result<Self> {
        use zune_jpeg::{
            zune_core::{colorspace::ColorSpace, options::DecoderOptions},
            JpegDecoder,
        };
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB);
        let mut dec = JpegDecoder::new_with_options(data, opts);
        let rgb = dec.decode().map_err(|e| anyhow!("jpeg decode: {e:?}"))?;
        let info = dec.info().ok_or_else(|| anyhow!("jpeg without info"))?;
        let (w, h) = (info.width as usize, info.height as usize);
        if rgb.len() < w * h * 3 {
            return Err(anyhow!("short jpeg buffer"));
        }
        Ok(Self { w, h, rgb })
    }

    pub fn from_rgb(w: usize, h: usize, rgb: Vec<u8>) -> Self {
        assert_eq!(rgb.len(), w * h * 3);
        Self { w, h, rgb }
    }

    pub fn solid(w: usize, h: usize, c: Rgb) -> Self {
        Self {
            w,
            h,
            rgb: [c.0, c.1, c.2].repeat(w * h),
        }
    }

    #[inline]
    fn px(&self, x: usize, y: usize) -> Rgb {
        let i = (y.min(self.h - 1) * self.w + x.min(self.w - 1)) * 3;
        Rgb(self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }

    /// Mean colour of the pixel box `[x0,x1)×[y0,y1)` (sampled on a coarse grid for speed).
    fn mean(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> Rgb {
        let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
        let step_y = ((y1 - y0) / 4).max(1);
        let step_x = ((x1 - x0) / 4).max(1);
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                let p = self.px(x, y);
                r += p.0 as u32;
                g += p.1 as u32;
                b += p.2 as u32;
                n += 1;
                x += step_x;
            }
            y += step_y;
        }
        let n = n.max(1);
        Rgb((r / n) as u8, (g / n) as u8, (b / n) as u8)
    }

    /// Colours of cell `(cx, cy)` where one cell covers `cell_w × cell_h` page pixels and this
    /// pixmap may be scaled relative to page pixels by `scale_x/scale_y`.
    pub fn cell(&self, cx: u16, cy: u16, g: &CellGeom, text: Option<Rgb>) -> CellColors {
        let (cell_w, cell_h, scale_x, scale_y) = (g.cw, g.ch, g.sx, g.sy);
        let x0 = (cx as f64 * cell_w * scale_x) as usize;
        let x1 = (((cx as f64 + 1.0) * cell_w * scale_x) as usize).max(x0 + 1);
        let y0 = (cy as f64 * cell_h * scale_y) as usize;
        let y1 = (((cy as f64 + 1.0) * cell_h * scale_y) as usize).max(y0 + 1);
        if x0 >= self.w || y0 >= self.h {
            return CellColors {
                top: Rgb::WHITE,
                bottom: Rgb::WHITE,
                dominant: Rgb::WHITE,
            };
        }
        let (x1, y1) = (x1.min(self.w), y1.min(self.h));
        let ym = (y0 + y1) / 2;
        let top = self.mean(x0, y0, x1, ym.max(y0 + 1).min(y1));
        let bottom = self.mean(x0, ym.min(y1 - 1), x1, y1);
        CellColors {
            top,
            bottom,
            dominant: self.dominant(x0, y0, x1, y1, text),
        }
    }

    /// Mode of a 4×8 sample lattice after 5-bit quantisation, ignoring samples close to `text`
    /// (a large glyph can out-vote its own background). Returns an exact sampled colour.
    fn dominant(&self, x0: usize, y0: usize, x1: usize, y1: usize, text: Option<Rgb>) -> Rgb {
        let mut keys = [0u16; 32];
        let mut cols = [Rgb::BLACK; 32];
        let mut counts = [0u8; 32];
        let mut used = 0usize;
        let (w, h) = (x1 - x0, y1 - y0);
        for sy in 0..8 {
            for sx in 0..4 {
                let p = self.px(x0 + (sx * w + w / 2) / 4, y0 + (sy * h + h / 2) / 8);
                if text.is_some_and(|t| t.dist2(p) < 4000) {
                    continue;
                }
                let key = ((p.0 as u16 >> 3) << 10) | ((p.1 as u16 >> 3) << 5) | (p.2 as u16 >> 3);
                match keys[..used].iter().position(|&k| k == key) {
                    Some(i) => counts[i] += 1,
                    None => {
                        keys[used] = key;
                        cols[used] = p;
                        counts[used] = 1;
                        used += 1;
                    }
                }
            }
        }
        if used == 0 {
            // every sample looked like the text colour (solid-colour cell): fall back to the mean
            return self.mean(x0, y0, x1, y1);
        }
        let best = (0..used).max_by_key(|&i| counts[i]).unwrap_or(0);
        cols[best]
    }
}
