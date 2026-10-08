//! Image regions for clients with a terminal graphics protocol: crop them out of the screenshot,
//! JPEG-encode, and send only what changed since last time.

use std::collections::HashMap;

use glyph_proto::{CellRect, ImageFormat, ImageMsg, TabId};

use crate::{pixmap::Pixmap, render::Metrics};

/// Cap per frame: a page with hundreds of thumbnails must not turn into hundreds of encodes.
pub const MAX_IMAGES: usize = 12;
/// Skip crops bigger than this many pixels (full-screen video, huge canvases).
const MAX_PIXELS: usize = 1_600_000;

pub struct Crop {
    pub rect: CellRect,
    pub id: u32,
    pub hash: u64,
    /// `None`: identical to what the client already has.
    pub msg: Option<ImageMsg>,
}

/// Stable per-position id (never 0: Kitty reserves it).
pub fn image_id(r: CellRect) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for v in [r.x, r.y, r.w, r.h] {
        h = (h ^ v as u32).wrapping_mul(0x0100_0193);
    }
    h.max(1)
}

fn hash_pixels(rgb: &[u8], w: usize, h: usize) -> u64 {
    // FNV-1a over a sparse lattice: cheap, and any visible change moves some sampled pixel
    let mut x: u64 = 0xcbf2_9ce4_8422_2325;
    let (sx, sy) = ((w / 24).max(1), (h / 24).max(1));
    let mut y = 0;
    while y < h {
        let mut xx = 0;
        while xx < w {
            let i = (y * w + xx) * 3;
            for b in &rgb[i..i + 3] {
                x = (x ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
            }
            xx += sx;
        }
        y += sy;
    }
    x ^ ((w as u64) << 32 | h as u64)
}

/// Crop `rects` from `pix`. Rects whose pixels hash the same as `known[id]` are reported without
/// re-encoding.
pub fn crop_images(
    pix: &Pixmap,
    m: &Metrics,
    tab: TabId,
    rects: &[CellRect],
    quality: u8,
    known: &HashMap<u32, u64>,
) -> Vec<Crop> {
    let (sx, sy) = (pix.w as f64 / m.px_w(), pix.h as f64 / m.px_h());
    let mut out: Vec<Crop> = Vec::new();
    for &r in rects.iter().take(MAX_IMAGES * 2) {
        if out.len() >= MAX_IMAGES {
            break;
        }
        let id = image_id(r);
        if out.iter().any(|c| c.id == id) {
            continue;
        }
        let x0 = ((r.x as f64 * m.cw) * sx) as usize;
        let y0 = ((r.y as f64 * m.ch) * sy) as usize;
        let x1 = ((((r.x + r.w) as f64 * m.cw) * sx) as usize).min(pix.w);
        let y1 = ((((r.y + r.h) as f64 * m.ch) * sy) as usize).min(pix.h);
        if x1 <= x0 || y1 <= y0 || (x1 - x0) * (y1 - y0) > MAX_PIXELS {
            continue;
        }
        let (w, h) = (x1 - x0, y1 - y0);
        let mut rgb = Vec::with_capacity(w * h * 3);
        for y in y0..y1 {
            rgb.extend_from_slice(&pix.rgb[(y * pix.w + x0) * 3..(y * pix.w + x1) * 3]);
        }
        let hash = hash_pixels(&rgb, w, h);
        let msg = if known.get(&id) == Some(&hash) {
            None
        } else {
            let mut data = Vec::new();
            if jpeg_encoder::Encoder::new(&mut data, quality.clamp(10, 95))
                .encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)
                .is_err()
            {
                continue;
            }
            Some(ImageMsg {
                tab,
                id,
                rect: r,
                px_w: w as u16,
                px_h: h as u16,
                format: ImageFormat::Jpeg,
                data,
            })
        };
        out.push(Crop {
            rect: r,
            id,
            hash,
            msg,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use glyph_proto::Rgb;

    use super::*;

    fn metrics() -> Metrics {
        Metrics {
            cols: 20,
            rows: 10,
            cw: 8.0,
            ch: 16.0,
        }
    }

    #[test]
    fn ids_are_stable_nonzero_and_position_sensitive() {
        let a = CellRect {
            x: 1,
            y: 2,
            w: 3,
            h: 4,
        };
        assert_eq!(image_id(a), image_id(a));
        assert_ne!(image_id(a), image_id(CellRect { x: 2, ..a }));
        assert_ne!(image_id(CellRect::default()), 0);
    }

    #[test]
    fn crops_the_right_pixels_and_skips_unchanged() {
        let m = metrics();
        // left half red, right half blue
        let mut p = Pixmap::solid(160, 160, Rgb(0, 0, 255));
        for y in 0..160 {
            for x in 0..80 {
                let i = (y * 160 + x) * 3;
                p.rgb[i..i + 3].copy_from_slice(&[255, 0, 0]);
            }
        }
        let red = CellRect {
            x: 1,
            y: 1,
            w: 4,
            h: 3,
        };
        let blue = CellRect {
            x: 12,
            y: 1,
            w: 4,
            h: 3,
        };
        let crops = crop_images(&p, &m, 7, &[red, blue], 80, &HashMap::new());
        assert_eq!(crops.len(), 2);
        let decoded = |c: &Crop| Pixmap::decode_jpeg(&c.msg.as_ref().unwrap().data).unwrap();
        let (r, b) = (decoded(&crops[0]), decoded(&crops[1]));
        assert_eq!((r.w, r.h), (32, 48));
        assert!(
            r.rgb[0] > 200 && r.rgb[2] < 60,
            "red crop: {:?}",
            &r.rgb[..3]
        );
        assert!(
            b.rgb[2] > 200 && b.rgb[0] < 60,
            "blue crop: {:?}",
            &b.rgb[..3]
        );
        assert_eq!(crops[0].msg.as_ref().unwrap().tab, 7);

        // same pixels again with the hashes known: nothing to re-send
        let known: HashMap<u32, u64> = crops.iter().map(|c| (c.id, c.hash)).collect();
        let again = crop_images(&p, &m, 7, &[red, blue], 80, &known);
        assert!(again.iter().all(|c| c.msg.is_none()));
        // change one region: only that one is re-encoded
        let mut p2 = Pixmap::solid(160, 160, Rgb(0, 255, 0));
        p2.rgb[..3].copy_from_slice(&[1, 2, 3]);
        let changed = crop_images(&p2, &m, 7, &[red, blue], 80, &known);
        assert!(changed.iter().all(|c| c.msg.is_some()));
    }

    #[test]
    fn caps_and_degenerate_rects() {
        let m = metrics();
        let p = Pixmap::solid(160, 160, Rgb(9, 9, 9));
        let many: Vec<CellRect> = (0..40)
            .map(|i| CellRect {
                x: i % 18,
                y: i / 18,
                w: 2,
                h: 1,
            })
            .collect();
        assert!(crop_images(&p, &m, 1, &many, 50, &HashMap::new()).len() <= MAX_IMAGES);
        let off = CellRect {
            x: 19,
            y: 9,
            w: 5,
            h: 5,
        }; // runs past the viewport: clipped, not a panic
        let c = crop_images(
            &p,
            &m,
            1,
            &[
                off,
                CellRect {
                    x: 3,
                    y: 3,
                    w: 0,
                    h: 2,
                },
            ],
            50,
            &HashMap::new(),
        );
        assert_eq!(c.len(), 1);
    }
}
