//! Colour-depth fallback: truecolor → xterm-256 → ANSI-16.

use glyph_proto::Rgb;
use ratatui::style::Color;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ColorDepth {
    True,
    C256,
    C16,
}

impl ColorDepth {
    /// From `COLORTERM`/`TERM`, unless overridden by config (`"truecolor" | "256" | "16"`).
    pub fn detect(config: &str) -> Self {
        match config {
            "truecolor" | "24bit" => return Self::True,
            "256" => return Self::C256,
            "16" => return Self::C16,
            _ => {}
        }
        let ct = std::env::var("COLORTERM")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ct.contains("truecolor") || ct.contains("24bit") {
            return Self::True;
        }
        let term = std::env::var("TERM").unwrap_or_default();
        if std::env::var_os("WT_SESSION").is_some()
            || std::env::var("TERM_PROGRAM")
                .is_ok_and(|p| matches!(p.as_str(), "iTerm.app" | "WezTerm" | "ghostty" | "vscode"))
            || term.contains("kitty")
            || term.contains("alacritty")
            || term.contains("ghostty")
        {
            return Self::True;
        }
        if term.contains("256") {
            Self::C256
        } else {
            Self::C16
        }
    }

    pub fn convert(self, c: Rgb) -> Color {
        match self {
            Self::True => Color::Rgb(c.0, c.1, c.2),
            Self::C256 => Color::Indexed(rgb_to_256(c)),
            Self::C16 => ANSI16[rgb_to_16(c) as usize].1,
        }
    }
}

/// Typical xterm RGB values for the 16 ANSI colours.
const ANSI16: [(Rgb, Color); 16] = [
    (Rgb(0, 0, 0), Color::Black),
    (Rgb(205, 0, 0), Color::Red),
    (Rgb(0, 205, 0), Color::Green),
    (Rgb(205, 205, 0), Color::Yellow),
    (Rgb(0, 0, 238), Color::Blue),
    (Rgb(205, 0, 205), Color::Magenta),
    (Rgb(0, 205, 205), Color::Cyan),
    (Rgb(229, 229, 229), Color::Gray),
    (Rgb(127, 127, 127), Color::DarkGray),
    (Rgb(255, 0, 0), Color::LightRed),
    (Rgb(0, 255, 0), Color::LightGreen),
    (Rgb(255, 255, 0), Color::LightYellow),
    (Rgb(92, 92, 255), Color::LightBlue),
    (Rgb(255, 0, 255), Color::LightMagenta),
    (Rgb(0, 255, 255), Color::LightCyan),
    (Rgb(255, 255, 255), Color::White),
];

pub fn rgb_to_16(c: Rgb) -> u8 {
    ANSI16
        .iter()
        .enumerate()
        .min_by_key(|(_, (p, _))| c.dist2(*p))
        .map_or(0, |(i, _)| i as u8)
}

/// Nearest xterm-256 index in the 6×6×6 cube or the 24-step grey ramp (16 system colours unused:
/// terminals remap them).
pub fn rgb_to_256(c: Rgb) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, &l)| (v as i32 - l as i32).abs())
            .map_or(0, |(i, _)| i)
    };
    let (r, g, b) = (near(c.0), near(c.1), near(c.2));
    let cube = Rgb(LEVELS[r], LEVELS[g], LEVELS[b]);
    let cube_idx = 16 + 36 * r + 6 * g + b;
    let avg = (c.0 as u32 + c.1 as u32 + c.2 as u32) / 3;
    let gi = ((avg as i32 - 8 + 5) / 10).clamp(0, 23) as u8;
    let gv = 8 + 10 * gi;
    let grey = Rgb(gv, gv, gv);
    if c.dist2(grey) < c.dist2(cube) {
        232 + gi
    } else {
        cube_idx as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_cube_and_grey_colours_map_exactly() {
        assert_eq!(rgb_to_256(Rgb(0, 0, 0)), 16);
        assert_eq!(rgb_to_256(Rgb(255, 255, 255)), 231);
        assert_eq!(rgb_to_256(Rgb(255, 0, 0)), 196);
        assert_eq!(rgb_to_256(Rgb(95, 135, 175)), 16 + 36 + 6 * 2 + 3);
        assert_eq!(rgb_to_256(Rgb(128, 128, 128)), 244);
    }

    #[test]
    fn sixteen_colour_picks_nearest() {
        assert_eq!(rgb_to_16(Rgb(250, 10, 10)), 9);
        assert_eq!(rgb_to_16(Rgb(5, 5, 5)), 0);
        assert_eq!(rgb_to_16(Rgb(250, 250, 250)), 15);
    }

    #[test]
    fn config_overrides_detection() {
        assert_eq!(ColorDepth::detect("16"), ColorDepth::C16);
        assert_eq!(ColorDepth::detect("256"), ColorDepth::C256);
        assert_eq!(ColorDepth::detect("truecolor"), ColorDepth::True);
        assert_eq!(ColorDepth::True.convert(Rgb(1, 2, 3)), Color::Rgb(1, 2, 3));
    }
}
