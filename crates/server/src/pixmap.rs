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

/// Colour summary of one terminal cell.
#[derive(Clone, Copy, Debug)]
pub struct CellColors {
    pub top: Rgb,
    pub bottom: Rgb,
    /// Most common colour (ignores glyph strokes on a plain background).
    pub dominant: Rgb,
}

impl Pixmap {
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
