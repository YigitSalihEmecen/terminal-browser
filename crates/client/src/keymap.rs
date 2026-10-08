//! Key specs, actions and the sequence-aware keymap.
//!
//! Spec syntax (config and help): plain characters are keys (`gg` = g then g); `<c-d>`, `<a-x>`,
//! `<s-tab>`, `<esc>`, `<cr>`, `<space>`, `<up>`, `<f5>`… are single keys; `<lt>` is `<`.

use std::collections::HashMap;

use glyph_proto::{KeyCode, KeyEvent, Mods};

/// A normalised key: `shift` is only kept for non-character keys (`G` already says shift).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Key {
    pub fn from_event(e: KeyEvent) -> Key {
        let shift = e.mods.contains(Mods::SHIFT) && !matches!(e.code, KeyCode::Char(_));
        Key {
            code: e.code,
            ctrl: e.mods.contains(Mods::CTRL),
            alt: e.mods.contains(Mods::ALT),
            shift,
        }
    }
}

macro_rules! actions {
    ($($variant:ident = $name:literal, $desc:literal;)*) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
        pub enum Action { $($variant,)* GotoTab(u8) }

        impl Action {
            /// Declaration order, for the help screen (related actions sit together).
            pub fn order(self) -> usize {
                const ALL: &[Action] = &[$(Action::$variant,)*];
                ALL.iter().position(|a| *a == self).unwrap_or(ALL.len())
            }
            pub fn from_name(s: &str) -> Option<Action> {
                if let Some(n) = s.strip_prefix("tab-").and_then(|n| n.parse::<u8>().ok()) {
                    return (1..=9).contains(&n).then_some(Action::GotoTab(n));
                }
                match s { $($name => Some(Action::$variant),)* _ => None }
            }
            pub fn name(self) -> String {
                match self { $(Action::$variant => $name.to_owned(),)* Action::GotoTab(n) => format!("tab-{n}") }
            }
            pub fn describe(self) -> String {
                match self { $(Action::$variant => $desc.to_owned(),)* Action::GotoTab(n) => format!("go to tab {n}") }
            }
        }
    };
}

actions! {
    Noop = "none", "do nothing (unbind)";
    ScrollDown = "scroll-down", "scroll down";
    ScrollUp = "scroll-up", "scroll up";
    ScrollLeft = "scroll-left", "scroll left";
    ScrollRight = "scroll-right", "scroll right";
    HalfPageDown = "half-page-down", "scroll half a page down";
    HalfPageUp = "half-page-up", "scroll half a page up";
    PageDown = "page-down", "scroll a page down";
    PageUp = "page-up", "scroll a page up";
    ScrollTop = "scroll-top", "scroll to top";
    ScrollBottom = "scroll-bottom", "scroll to bottom";
    Back = "back", "history back";
    Forward = "forward", "history forward";
    Reload = "reload", "reload page";
    Stop = "stop", "stop loading";
    Omnibox = "omnibox", "open URL or search";
    OmniboxCurrent = "omnibox-current", "edit current URL";
    OmniboxNewTab = "omnibox-new-tab", "open URL or search in a new tab";
    NewTab = "new-tab", "new empty tab";
    CloseTab = "close-tab", "close tab";
    NextTab = "next-tab", "next tab";
    PrevTab = "prev-tab", "previous tab";
    FirstTab = "first-tab", "first tab";
    LastTab = "last-tab", "last tab";
    Hints = "hints", "link hints: follow";
    HintsNewTab = "hints-new-tab", "link hints: open in new tab";
    HintsInput = "hints-input", "focus a text field";
    Find = "find", "find in page";
    FindNext = "find-next", "next match";
    FindPrev = "find-prev", "previous match";
    Insert = "insert", "insert mode (keys go to the page)";
    Visual = "visual", "visual mode: select text with the keyboard";
    CopyUrl = "copy-url", "copy page URL";
    CopyTitle = "copy-title", "copy page title";
    ToggleTextMode = "toggle-text-mode", "toggle reader (text) mode";
    Help = "help", "show / hide this help";
    Redraw = "redraw", "redraw screen";
    Escape = "escape", "clear selection / find / focus";
    Quit = "quit", "quit";
}

pub fn parse_spec(spec: &str) -> Result<Vec<Key>, String> {
    let mut out = Vec::new();
    let mut chars = spec.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(plain(c));
            continue;
        }
        let mut name = String::new();
        loop {
            match chars.next() {
                Some('>') => break,
                Some(ch) => name.push(ch),
                None => return Err(format!("unterminated '<' in {spec:?}")),
            }
        }
        out.push(parse_named(&name).ok_or_else(|| format!("unknown key <{name}> in {spec:?}"))?);
    }
    if out.is_empty() {
        return Err("empty key spec".into());
    }
    Ok(out)
}

fn plain(c: char) -> Key {
    Key {
        code: KeyCode::Char(c),
        ctrl: false,
        alt: false,
        shift: false,
    }
}

fn parse_named(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('-').collect();
    let base = parts.pop()?;
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for p in parts {
        match p {
            "c" | "ctrl" => ctrl = true,
            "a" | "m" | "alt" | "meta" => alt = true,
            "s" | "shift" => shift = true,
            _ => return None,
        }
    }
    let code = match base {
        "esc" | "escape" => KeyCode::Esc,
        "cr" | "enter" | "return" => KeyCode::Enter,
        "tab" => KeyCode::Tab,
        "bs" | "backspace" => KeyCode::Backspace,
        "del" | "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "gt" => KeyCode::Char('>'),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "insert" => KeyCode::Insert,
        f if f.starts_with('f') && f[1..].parse::<u8>().is_ok() => KeyCode::F(f[1..].parse().ok()?),
        c if c.chars().count() == 1 => {
            let ch = c.chars().next()?;
            KeyCode::Char(if shift { ch.to_ascii_uppercase() } else { ch })
        }
        _ => return None,
    };
    // for characters shift is folded into the case
    let shift = shift && !matches!(code, KeyCode::Char(_));
    Some(Key {
        code,
        ctrl,
        alt,
        shift,
    })
}

pub fn spec_to_string(keys: &[Key]) -> String {
    keys.iter()
        .map(|k| {
            let base = match k.code {
                KeyCode::Char(' ') => "space".to_owned(),
                KeyCode::Char('<') => "lt".to_owned(),
                KeyCode::Char(c) => c.to_string(),
                KeyCode::Esc => "esc".into(),
                KeyCode::Enter => "cr".into(),
                KeyCode::Tab => "tab".into(),
                KeyCode::Backspace => "bs".into(),
                KeyCode::Delete => "del".into(),
                KeyCode::Up => "up".into(),
                KeyCode::Down => "down".into(),
                KeyCode::Left => "left".into(),
                KeyCode::Right => "right".into(),
                KeyCode::Home => "home".into(),
                KeyCode::End => "end".into(),
                KeyCode::PageUp => "pageup".into(),
                KeyCode::PageDown => "pagedown".into(),
                KeyCode::Insert => "insert".into(),
                KeyCode::F(n) => format!("f{n}"),
            };
            let plain_char = matches!(k.code, KeyCode::Char(c) if c != ' ' && c != '<');
            if plain_char && !k.ctrl && !k.alt {
                return base;
            }
            let mut s = String::from("<");
            if k.ctrl {
                s.push_str("c-");
            }
            if k.alt {
                s.push_str("a-");
            }
            if k.shift {
                s.push_str("s-");
            }
            s.push_str(&base);
            s.push('>');
            s
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum KeyMode {
    Normal,
    Insert,
}

pub enum Lookup {
    Action(Action),
    /// A proper prefix of at least one binding: wait for more keys.
    Pending,
    None,
}

#[derive(Clone, Debug, Default)]
pub struct Keymap {
    bindings: HashMap<(KeyMode, Vec<Key>), Action>,
}

impl Keymap {
    pub fn bind(&mut self, mode: KeyMode, spec: &str, action: Action) -> Result<(), String> {
        let keys = parse_spec(spec)?;
        if action == Action::Noop {
            self.bindings.remove(&(mode, keys));
        } else {
            self.bindings.insert((mode, keys), action);
        }
        Ok(())
    }

    pub fn lookup(&self, mode: KeyMode, seq: &[Key]) -> Lookup {
        if let Some(a) = self.bindings.get(&(mode, seq.to_vec())) {
            return Lookup::Action(*a);
        }
        if self
            .bindings
            .keys()
            .any(|(m, k)| *m == mode && k.len() > seq.len() && k[..seq.len()] == *seq)
        {
            return Lookup::Pending;
        }
        Lookup::None
    }

    /// `(spec, action)` pairs for the help overlay, sorted by action then spec.
    pub fn list(&self, mode: KeyMode) -> Vec<(String, Action)> {
        let mut v: Vec<_> = self
            .bindings
            .iter()
            .filter(|((m, _), _)| *m == mode)
            .map(|((_, k), a)| (spec_to_string(k), *a))
            .collect();
        v.sort_by(|a, b| a.1.name().cmp(&b.1.name()).then(a.0.cmp(&b.0)));
        v
    }

    pub fn defaults() -> Keymap {
        use Action::*;
        let mut m = Keymap::default();
        let normal: &[(&str, Action)] = &[
            ("j", ScrollDown),
            ("<down>", ScrollDown),
            ("k", ScrollUp),
            ("<up>", ScrollUp),
            ("h", ScrollLeft),
            ("<left>", ScrollLeft),
            ("l", ScrollRight),
            ("<right>", ScrollRight),
            ("<c-d>", HalfPageDown),
            ("<c-u>", HalfPageUp),
            ("<c-f>", PageDown),
            ("<pagedown>", PageDown),
            ("<space>", PageDown),
            ("<c-b>", PageUp),
            ("<pageup>", PageUp),
            ("gg", ScrollTop),
            ("<home>", ScrollTop),
            ("G", ScrollBottom),
            ("<end>", ScrollBottom),
            ("H", Back),
            ("<a-left>", Back),
            ("<c-o>", Back),
            ("L", Forward),
            ("<a-right>", Forward),
            ("r", Reload),
            ("<f5>", Reload),
            ("<c-c>", Stop),
            ("o", Omnibox),
            ("<c-l>", Omnibox),
            ("O", OmniboxCurrent),
            ("t", OmniboxNewTab),
            ("<c-t>", NewTab),
            ("x", CloseTab),
            ("<c-w>", CloseTab),
            ("J", PrevTab),
            ("gT", PrevTab),
            ("K", NextTab),
            ("gt", NextTab),
            ("<tab>", NextTab),
            ("<s-tab>", PrevTab),
            ("g0", FirstTab),
            ("g$", LastTab),
            ("f", Hints),
            ("F", HintsNewTab),
            ("gi", HintsInput),
            ("/", Find),
            ("n", FindNext),
            ("N", FindPrev),
            ("i", Insert),
            ("v", Visual),
            ("yy", CopyUrl),
            ("yt", CopyTitle),
            ("M", ToggleTextMode),
            ("?", Help),
            ("<f1>", Help),
            ("<c-r>", Redraw),
            ("<esc>", Escape),
            ("ZZ", Quit),
            ("<c-q>", Quit),
        ];
        for (s, a) in normal {
            m.bind(KeyMode::Normal, s, *a)
                .expect("default binding parses");
        }
        for n in 1..=9u8 {
            m.bind(KeyMode::Normal, &format!("<a-{n}>"), GotoTab(n))
                .expect("default binding parses");
        }
        let insert: &[(&str, Action)] = &[("<esc>", Escape), ("<c-l>", Omnibox)];
        for (s, a) in insert {
            m.bind(KeyMode::Insert, s, *a)
                .expect("default binding parses");
        }
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sequences_and_named_keys() {
        assert_eq!(parse_spec("gg").unwrap().len(), 2);
        let k = &parse_spec("<c-d>").unwrap()[0];
        assert!(k.ctrl && k.code == KeyCode::Char('d'));
        let k = &parse_spec("<s-tab>").unwrap()[0];
        assert!(k.shift && k.code == KeyCode::Tab);
        assert_eq!(parse_spec("g<c-t>").unwrap().len(), 2);
        assert_eq!(parse_spec("<lt>").unwrap()[0].code, KeyCode::Char('<'));
        assert!(parse_spec("<bogus>").is_err());
        assert!(parse_spec("<c-").is_err());
        assert!(parse_spec("").is_err());
    }

    #[test]
    fn spec_round_trips() {
        for s in [
            "gg", "<c-d>", "<s-tab>", "<space>", "<a-left>", "g<c-t>", "<f5>", "yt", "<lt>", "G",
        ] {
            let k = parse_spec(s).unwrap();
            assert_eq!(parse_spec(&spec_to_string(&k)).unwrap(), k, "{s}");
        }
    }

    #[test]
    fn prefix_lookup() {
        let m = Keymap::defaults();
        let g = parse_spec("g").unwrap();
        assert!(matches!(m.lookup(KeyMode::Normal, &g), Lookup::Pending));
        let gg = parse_spec("gg").unwrap();
        assert!(matches!(
            m.lookup(KeyMode::Normal, &gg),
            Lookup::Action(Action::ScrollTop)
        ));
        let z = parse_spec("q").unwrap();
        assert!(matches!(m.lookup(KeyMode::Normal, &z), Lookup::None));
        // insert mode only has its own bindings
        assert!(matches!(
            m.lookup(KeyMode::Insert, &parse_spec("j").unwrap()),
            Lookup::None
        ));
    }

    #[test]
    fn unbinding_and_rebinding() {
        let mut m = Keymap::defaults();
        m.bind(KeyMode::Normal, "j", Action::Noop).unwrap();
        assert!(matches!(
            m.lookup(KeyMode::Normal, &parse_spec("j").unwrap()),
            Lookup::None
        ));
        m.bind(KeyMode::Normal, "J", Action::ScrollDown).unwrap();
        assert!(matches!(
            m.lookup(KeyMode::Normal, &parse_spec("J").unwrap()),
            Lookup::Action(Action::ScrollDown)
        ));
    }

    #[test]
    fn action_names_round_trip() {
        for (_, a) in Keymap::defaults().list(KeyMode::Normal) {
            assert_eq!(Action::from_name(&a.name()), Some(a));
        }
        assert_eq!(Action::from_name("tab-3"), Some(Action::GotoTab(3)));
        assert_eq!(Action::from_name("tab-0"), None);
        assert_eq!(Action::from_name("nope"), None);
    }

    #[test]
    fn uppercase_char_events_ignore_shift_flag() {
        let e = KeyEvent {
            code: KeyCode::Char('G'),
            mods: Mods::SHIFT,
        };
        assert_eq!(Key::from_event(e), parse_spec("G").unwrap()[0]);
    }
}
