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
    /// What pages see for `prefers-color-scheme`: "auto" (follow $COLORFGBG, else light),
    /// "light" or "dark".
    pub color_scheme: String,
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
            color_scheme: "auto".into(),
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KeysCfg {
    pub normal: HashMap<String, String>,
    pub insert: HashMap<String, String>,
}

macro_rules! theme {
    ($($f:ident),* $(,)?) => {
        /// Hex colours (`#rrggbb`); anything left out comes from `preset`.
        #[derive(Deserialize, Clone, Debug, Default)]
        #[serde(default, deny_unknown_fields)]
        pub struct ThemeCfg {
            /// "dark" (default), "light" or "high-contrast".
            pub preset: Option<String>,
            $(pub $f: Option<String>,)*
        }

        #[derive(Clone, Copy, Debug)]
        pub struct Theme { $(pub $f: Rgb,)* }
    };
}

theme!(
    tab_bar_bg,
    tab_fg,
    tab_active_bg,
    tab_active_fg,
    omnibox_bg,
    omnibox_fg,
    status_bg,
    status_fg,
    progress,
    hint_bg,
    hint_fg,
    accent,
    help_bg,
    help_fg,
);

impl Theme {
    pub const PRESETS: &'static [&'static str] = &["dark", "light", "high-contrast"];

    pub fn preset(name: &str) -> Option<Theme> {
        let c = |h: &str| parse_hex(h).expect("built-in colour");
        Some(match name {
            "dark" => Theme {
                tab_bar_bg: c("#181825"),
                tab_fg: c("#a6adc8"),
                tab_active_bg: c("#313244"),
                tab_active_fg: c("#cdd6f4"),
                omnibox_bg: c("#1e1e2e"),
                omnibox_fg: c("#cdd6f4"),
                status_bg: c("#11111b"),
                status_fg: c("#bac2de"),
                progress: c("#89b4fa"),
                hint_bg: c("#f9e2af"),
                hint_fg: c("#11111b"),
                accent: c("#89b4fa"),
                help_bg: c("#1e1e2e"),
                help_fg: c("#cdd6f4"),
            },
            "light" => Theme {
                tab_bar_bg: c("#dce0e8"),
                tab_fg: c("#5c5f77"),
                tab_active_bg: c("#eff1f5"),
                tab_active_fg: c("#4c4f69"),
                omnibox_bg: c("#eff1f5"),
                omnibox_fg: c("#4c4f69"),
                status_bg: c("#ccd0da"),
                status_fg: c("#4c4f69"),
                progress: c("#1e66f5"),
                hint_bg: c("#df8e1d"),
                hint_fg: c("#eff1f5"),
                accent: c("#1e66f5"),
                help_bg: c("#eff1f5"),
                help_fg: c("#4c4f69"),
            },
            "high-contrast" => Theme {
                tab_bar_bg: c("#000000"),
                tab_fg: c("#ffffff"),
                tab_active_bg: c("#ffffff"),
                tab_active_fg: c("#000000"),
                omnibox_bg: c("#000000"),
                omnibox_fg: c("#ffffff"),
                status_bg: c("#000000"),
                status_fg: c("#ffffff"),
                progress: c("#00ff00"),
                hint_bg: c("#ffff00"),
                hint_fg: c("#000000"),
                accent: c("#00ffff"),
                help_bg: c("#000000"),
                help_fg: c("#ffffff"),
            },
            _ => return None,
        })
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesCfg {
    /// "auto" | "kitty" | "sixel" | "iterm2" | "none"
    pub protocol: String,
    /// Also load images in the `lean` profile when no graphics protocol is available (they are
    /// then shown as half-blocks). Costs bandwidth; off by default.
    pub always_load: bool,
}

impl Default for ImagesCfg {
    fn default() -> Self {
        Self {
            protocol: "auto".into(),
            always_load: false,
        }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default, deny_unknown_fields)]
pub struct FileConfig {
    pub ui: Ui,
    pub keys: KeysCfg,
    pub theme: ThemeCfg,
    pub images: ImagesCfg,
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
        let mut theme = match t.preset.as_deref() {
            None => Theme::preset("dark").expect("built-in"),
            Some(name) => Theme::preset(name).unwrap_or_else(|| {
                warnings.push(format!(
                    "theme.preset: unknown {name:?} (use one of {})",
                    Theme::PRESETS.join(", ")
                ));
                Theme::preset("dark").expect("built-in")
            }),
        };
        macro_rules! over {
            ($($f:ident),*) => {$(
                if let Some(v) = &t.$f {
                    match parse_hex(v) {
                        Some(c) => theme.$f = c,
                        None => warnings.push(format!("theme.{}: {v:?} is not #rrggbb", stringify!($f))),
                    }
                }
            )*};
        }
        over!(
            tab_bar_bg,
            tab_fg,
            tab_active_bg,
            tab_active_fg,
            omnibox_bg,
            omnibox_fg,
            status_bg,
            status_fg,
            progress,
            hint_bg,
            hint_fg,
            accent,
            help_bg,
            help_fg
        );
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

    #[test]
    fn the_shipped_example_file_is_valid_and_equals_the_defaults() {
        let c =
            Config::from_toml(include_str!("../../../glyph.example.toml")).expect("example parses");
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let d = Config::default();
        assert_eq!(format!("{:?}", c.ui), format!("{:?}", d.ui));
        assert_eq!(format!("{:?}", c.theme), format!("{:?}", d.theme));
        assert_eq!(format!("{:?}", c.images), format!("{:?}", d.images));
        assert_eq!(
            c.keymap.list(KeyMode::Normal),
            d.keymap.list(KeyMode::Normal)
        );
    }

    #[test]
    fn presets_exist_and_overrides_win() {
        for p in Theme::PRESETS {
            assert!(Theme::preset(p).is_some(), "{p}");
        }
        let c = Config::from_toml("[theme]\npreset = \"light\"\naccent = \"#010203\"").unwrap();
        assert_eq!(c.theme.accent, Rgb(1, 2, 3));
        assert_eq!(
            c.theme.tab_bar_bg,
            Theme::preset("light").unwrap().tab_bar_bg
        );
        let bad = Config::from_toml("[theme]\npreset = \"neon\"").unwrap();
        assert_eq!(bad.warnings.len(), 1);
        assert_eq!(bad.theme.accent, Theme::preset("dark").unwrap().accent);
    }

    #[test]
    fn every_action_in_the_example_comment_exists() {
        let text = include_str!("../../../glyph.example.toml");
        let list: String = text
            .lines()
            .skip_while(|l| !l.starts_with("# Actions:"))
            .take_while(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let mut n = 0;
        for w in list
            .trim_start_matches("# Actions:")
            .split_whitespace()
            .filter(|w| *w != "#" && *w != "…")
        {
            let w = w.trim_start_matches('#');
            if w.is_empty() || w.starts_with("tab-9") {
                continue;
            }
            assert!(
                Action::from_name(w).is_some(),
                "documented action {w:?} does not exist"
            );
            n += 1;
        }
        assert!(n > 30, "parsed too few actions ({n})");
    }
}
