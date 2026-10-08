//! Client key events → CDP `Input.dispatchKeyEvent` parameter sets.

use glyph_proto::{KeyCode, KeyEvent, Mods};
use serde_json::{json, Value};

/// CDP modifier bitmask: Alt=1, Ctrl=2, Meta=4, Shift=8.
pub fn cdp_mods(m: Mods) -> u8 {
    (m.contains(Mods::ALT) as u8)
        | (m.contains(Mods::CTRL) as u8) << 1
        | (m.contains(Mods::META) as u8) << 2
        | (m.contains(Mods::SHIFT) as u8) << 3
}

fn editing_command(c: char, shift: bool) -> Option<&'static str> {
    Some(match (c.to_ascii_lowercase(), shift) {
        ('a', _) => "selectAll",
        ('c', _) => "copy",
        ('x', _) => "cut",
        ('z', false) => "undo",
        ('z', true) | ('y', _) => "redo",
        _ => return None,
    })
}

/// The sequence of `Input.dispatchKeyEvent` params for one key press.
pub fn key_events(k: KeyEvent) -> Vec<Value> {
    let mods = cdp_mods(k.mods);
    let shortcut =
        k.mods.contains(Mods::CTRL) || k.mods.contains(Mods::ALT) || k.mods.contains(Mods::META);
    let (key, code, vk, text): (String, String, u32, Option<String>) = match k.code {
        KeyCode::Char(c) => {
            let upper = c.to_ascii_uppercase();
            let vk = if c.is_ascii_alphanumeric() {
                upper as u32
            } else {
                oem_vk(c)
            };
            let code = if c.is_ascii_alphabetic() {
                format!("Key{upper}")
            } else if c.is_ascii_digit() {
                format!("Digit{c}")
            } else if c == ' ' {
                "Space".to_owned()
            } else {
                String::new()
            };
            (c.to_string(), code, vk, (!shortcut).then(|| c.to_string()))
        }
        KeyCode::Enter => ("Enter".into(), "Enter".into(), 13, Some("\r".into())),
        KeyCode::Tab => ("Tab".into(), "Tab".into(), 9, None),
        KeyCode::Backspace => ("Backspace".into(), "Backspace".into(), 8, None),
        KeyCode::Delete => ("Delete".into(), "Delete".into(), 46, None),
        KeyCode::Esc => ("Escape".into(), "Escape".into(), 27, None),
        KeyCode::Left => ("ArrowLeft".into(), "ArrowLeft".into(), 37, None),
        KeyCode::Up => ("ArrowUp".into(), "ArrowUp".into(), 38, None),
        KeyCode::Right => ("ArrowRight".into(), "ArrowRight".into(), 39, None),
        KeyCode::Down => ("ArrowDown".into(), "ArrowDown".into(), 40, None),
        KeyCode::Home => ("Home".into(), "Home".into(), 36, None),
        KeyCode::End => ("End".into(), "End".into(), 35, None),
        KeyCode::PageUp => ("PageUp".into(), "PageUp".into(), 33, None),
        KeyCode::PageDown => ("PageDown".into(), "PageDown".into(), 34, None),
        KeyCode::Insert => ("Insert".into(), "Insert".into(), 45, None),
        KeyCode::F(n) => (
            format!("F{n}"),
            format!("F{n}"),
            111 + n.clamp(1, 12) as u32,
            None,
        ),
    };
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "modifiers": mods, "key": key, "code": code,
        "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk,
    });
    if let Some(t) = &text {
        down["text"] = json!(t);
        down["unmodifiedText"] = json!(t);
    }
    if let (KeyCode::Char(c), true) = (k.code, shortcut) {
        if k.mods.contains(Mods::CTRL) || k.mods.contains(Mods::META) {
            if let Some(cmd) = editing_command(c, k.mods.contains(Mods::SHIFT)) {
                down["commands"] = json!([cmd]);
            }
        }
    }
    let up = json!({ "type": "keyUp", "modifiers": mods, "key": key, "code": code, "windowsVirtualKeyCode": vk, "nativeVirtualKeyCode": vk });
    vec![down, up]
}

fn oem_vk(c: char) -> u32 {
    match c {
        ' ' => 32,
        ';' | ':' => 186,
        '=' | '+' => 187,
        ',' | '<' => 188,
        '-' | '_' => 189,
        '.' | '>' => 190,
        '/' | '?' => 191,
        '`' | '~' => 192,
        '[' | '{' => 219,
        '\\' | '|' => 220,
        ']' | '}' => 221,
        '\'' | '"' => 222,
        _ => c as u32 & 0xff,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_char_inserts_text() {
        let ev = key_events(KeyEvent {
            code: KeyCode::Char('a'),
            mods: Mods::default(),
        });
        assert_eq!(ev[0]["type"], "keyDown");
        assert_eq!(ev[0]["text"], "a");
        assert_eq!(ev[0]["windowsVirtualKeyCode"], 65);
        assert_eq!(ev[1]["type"], "keyUp");
    }

    #[test]
    fn ctrl_a_is_select_all_not_text() {
        let ev = key_events(KeyEvent {
            code: KeyCode::Char('a'),
            mods: Mods::CTRL,
        });
        assert_eq!(ev[0]["type"], "rawKeyDown");
        assert!(ev[0].get("text").is_none());
        assert_eq!(ev[0]["commands"][0], "selectAll");
        assert_eq!(ev[0]["modifiers"], 2);
    }

    #[test]
    fn special_keys() {
        let ev = key_events(KeyEvent {
            code: KeyCode::Enter,
            mods: Mods::default(),
        });
        assert_eq!(ev[0]["text"], "\r");
        let ev = key_events(KeyEvent {
            code: KeyCode::Backspace,
            mods: Mods::SHIFT,
        });
        assert_eq!(ev[0]["windowsVirtualKeyCode"], 8);
        assert_eq!(ev[0]["modifiers"], 8);
    }
}
