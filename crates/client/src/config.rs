//! `~/.config/glyph/config.toml` (XDG): UI behaviour, keybindings, theme.

use std::{collections::HashMap, path::PathBuf};

use glyph_proto::Rgb;
use serde::Deserialize;

use crate::keymap::{Action, KeyMode, Keymap};

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Ui {
    pub scroll_lines: i32,
    pub search: String,
    pub home: String,
    pub hint_chars: String,
    /// "auto" | "truecolor" | "256" | "16"
    pub color: String,
    pub mouse: bool,
    /// Lines scrolled per mouse-wheel notch.
    pub wheel_lines: i32,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            scroll_lines: 2,
            search: "https://duckduckgo.com/?q=%s".into(),
            home: "about:blank".into(),
            hint_chars: "asdfghjklqwertyuiopzxcvbnm".into(),
            color: "auto".into(),
            mouse: true,
            wheel_lines: 3,
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KeysCfg {
    pub normal: HashMap<String, String>,
    pub insert: HashMap<String, String>,
}

/// Hex colours (`#rrggbb`).
#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeCfg {
    pub tab_bar_bg: String,
    pub tab_fg: String,
    pub tab_active_bg: String,
    pub tab_active_fg: String,
    pub omnibox_bg: String,
    pub omnibox_fg: String,
    pub status_bg: String,
    pub status_fg: String,
    pub progress: String,
    pub hint_bg: String,
    pub hint_fg: String,
    pub accent: String,
    pub help_bg: String,
    pub help_fg: String,
}

impl Default for ThemeCfg {
    fn default() -> Self {
        Self {
            tab_bar_bg: "#181825".into(),
            tab_fg: "#a6adc8".into(),
            tab_active_bg: "#313244".into(),
            tab_active_fg: "#cdd6f4".into(),
            omnibox_bg: "#1e1e2e".into(),
            omnibox_fg: "#cdd6f4".into(),
            status_bg: "#11111b".into(),
            status_fg: "#bac2de".into(),
            progress: "#89b4fa".into(),
            hint_bg: "#f9e2af".into(),
            hint_fg: "#11111b".into(),
            accent: "#89b4fa".into(),
            help_bg: "#1e1e2e".into(),
            help_fg: "#cdd6f4".into(),
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesCfg {
    /// "auto" | "kitty" | "sixel" | "iterm2" | "none"
    pub protocol: String,
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub ui: Ui,
    pub keys: KeysCfg,
    pub theme: ThemeCfg,
    pub images: ImagesCfg,
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub tab_bar_bg: Rgb,
    pub tab_fg: Rgb,
    pub tab_active_bg: Rgb,
    pub tab_active_fg: Rgb,
    pub omnibox_bg: Rgb,
    pub omnibox_fg: Rgb,
    pub status_bg: Rgb,
    pub status_fg: Rgb,
    pub progress: Rgb,
    pub hint_bg: Rgb,
    pub hint_fg: Rgb,
    pub accent: Rgb,
    pub help_bg: Rgb,
    pub help_fg: Rgb,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub ui: Ui,
    pub keymap: Keymap,
    pub theme: Theme,
    pub images: ImagesCfg,
    /// Problems found while loading (unknown action, bad colour…); shown once at start.
    pub warnings: Vec<String>,
}

pub fn parse_hex(s: &str) -> Option<Rgb> {
    let h = s.strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some(Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

pub fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("glyph").join("config.toml"))
}

impl Default for Config {
    fn default() -> Self {
        Self::from_file_config(FileConfig::default())
    }
}

impl Config {
    pub fn from_toml(src: &str) -> Result<Config, String> {
        let fc: FileConfig = toml::from_str(src).map_err(|e| e.to_string())?;
        Ok(Self::from_file_config(fc))
    }

    /// Load from `path` (or the XDG default). A missing file is fine; a broken one is an error.
    pub fn load(path: Option<PathBuf>) -> Result<Config, String> {
        let explicit = path.is_some();
        let Some(p) = path.or_else(default_path) else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&p) {
            Ok(s) => Config::from_toml(&s).map_err(|e| format!("{}: {e}", p.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !explicit => {
                Ok(Config::default())
            }
            Err(e) => Err(format!("{}: {e}", p.display())),
        }
    }

    pub fn from_file_config(fc: FileConfig) -> Config {
        let mut warnings = Vec::new();
        let mut keymap = Keymap::defaults();
        for (mode, table) in [
            (KeyMode::Normal, &fc.keys.normal),
            (KeyMode::Insert, &fc.keys.insert),
        ] {
            for (spec, name) in table {
                match Action::from_name(name) {
                    Some(a) => {
                        if let Err(e) = keymap.bind(mode, spec, a) {
                            warnings.push(format!("keys: {e}"));
                        }
                    }
                    None => warnings.push(format!("keys: unknown action {name:?} for {spec:?}")),
                }
            }
        }
        let t = &fc.theme;
        let d = ThemeCfg::default();
        let mut col = |name: &str, v: &str, fallback: &str| -> Rgb {
            parse_hex(v).unwrap_or_else(|| {
                warnings.push(format!("theme.{name}: {v:?} is not #rrggbb"));
                parse_hex(fallback).expect("default colour")
            })
        };
        let theme = Theme {
            tab_bar_bg: col("tab_bar_bg", &t.tab_bar_bg, &d.tab_bar_bg),
            tab_fg: col("tab_fg", &t.tab_fg, &d.tab_fg),
            tab_active_bg: col("tab_active_bg", &t.tab_active_bg, &d.tab_active_bg),
            tab_active_fg: col("tab_active_fg", &t.tab_active_fg, &d.tab_active_fg),
            omnibox_bg: col("omnibox_bg", &t.omnibox_bg, &d.omnibox_bg),
            omnibox_fg: col("omnibox_fg", &t.omnibox_fg, &d.omnibox_fg),
            status_bg: col("status_bg", &t.status_bg, &d.status_bg),
            status_fg: col("status_fg", &t.status_fg, &d.status_fg),
            progress: col("progress", &t.progress, &d.progress),
            hint_bg: col("hint_bg", &t.hint_bg, &d.hint_bg),
            hint_fg: col("hint_fg", &t.hint_fg, &d.hint_fg),
            accent: col("accent", &t.accent, &d.accent),
            help_bg: col("help_bg", &t.help_bg, &d.help_bg),
            help_fg: col("help_fg", &t.help_fg, &d.help_fg),
        };
        let mut ui = fc.ui;
        if !ui.search.contains("%s") {
            warnings.push("ui.search must contain %s; using the default".into());
            ui.search = Ui::default().search;
        }
        if ui.hint_chars.chars().count() < 2 {
            warnings.push("ui.hint_chars needs at least two characters; using the default".into());
            ui.hint_chars = Ui::default().hint_chars;
        }
        Config {
            ui,
            keymap,
            theme,
            images: fc.images,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{parse_spec, Lookup};

    #[test]
    fn empty_config_is_defaults() {
        let c = Config::from_toml("").unwrap();
        assert!(c.warnings.is_empty());
        assert_eq!(c.ui.scroll_lines, 2);
        assert!(matches!(
            c.keymap.lookup(KeyMode::Normal, &parse_spec("j").unwrap()),
            Lookup::Action(Action::ScrollDown)
        ));
    }

    #[test]
    fn keys_and_theme_override() {
        let c = Config::from_toml(
            r##"
            [ui]
            scroll_lines = 5
            search = "https://example.org/?q=%s"
            [keys.normal]
            "j" = "none"
            "<c-n>" = "scroll-down"
            "e" = "bogus-action"
            [theme]
            accent = "#ff0000"
            status_bg = "red"
            "##,
        )
        .unwrap();
        assert_eq!(c.ui.scroll_lines, 5);
        assert!(matches!(
            c.keymap.lookup(KeyMode::Normal, &parse_spec("j").unwrap()),
            Lookup::None
        ));
        assert!(matches!(
            c.keymap
                .lookup(KeyMode::Normal, &parse_spec("<c-n>").unwrap()),
            Lookup::Action(Action::ScrollDown)
        ));
        assert_eq!(c.theme.accent, Rgb(255, 0, 0));
        assert_eq!(c.warnings.len(), 2, "{:?}", c.warnings);
    }

    #[test]
    fn unknown_fields_are_errors() {
        assert!(Config::from_toml("[ui]\nscrol_lines = 5").is_err());
        assert!(Config::from_toml("[nope]\nx = 1").is_err());
    }

    #[test]
    fn hex_colours() {
        assert_eq!(parse_hex("#0a0B0c"), Some(Rgb(10, 11, 12)));
        assert_eq!(parse_hex("0a0b0c"), None);
        assert_eq!(parse_hex("#abc"), None);
    }
}
