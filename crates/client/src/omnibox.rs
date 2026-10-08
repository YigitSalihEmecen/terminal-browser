//! Single-line editor (omnibox, find bar) and URL-or-search resolution.

use glyph_proto::width::{clusters, str_width};

#[derive(Clone, Debug, Default)]
pub struct LineEdit {
    pub text: String,
    /// Byte offset, always on a grapheme boundary.
    cursor: usize,
}

impl LineEdit {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            cursor: text.len(),
        }
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    /// Cursor position in terminal cells from the start of the text.
    pub fn cursor_col(&self) -> usize {
        str_width(&self.text[..self.cursor])
    }

    pub fn insert_str(&mut self, s: &str) {
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    pub fn insert(&mut self, c: char) {
        if !c.is_control() {
            self.text.insert(self.cursor, c);
            self.cursor += c.len_utf8();
        }
    }

    fn prev_boundary(&self) -> usize {
        clusters(&self.text[..self.cursor])
            .next_back()
            .map_or(0, |g| self.cursor - g.len())
    }

    fn next_boundary(&self) -> usize {
        clusters(&self.text[self.cursor..])
            .next()
            .map_or(self.cursor, |g| self.cursor + g.len())
    }

    pub fn backspace(&mut self) {
        let p = self.prev_boundary();
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
    }

    pub fn delete(&mut self) {
        let n = self.next_boundary();
        self.text.replace_range(self.cursor..n, "");
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary();
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    pub fn kill_to_start(&mut self) {
        self.text.replace_range(..self.cursor, "");
        self.cursor = 0;
    }

    pub fn kill_to_end(&mut self) {
        self.text.truncate(self.cursor);
    }

    /// Ctrl-W: delete the word before the cursor.
    pub fn kill_word(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        let start = trimmed
            .rfind(|c: char| c.is_whitespace() || c == '/')
            .map_or(0, |i| i + 1);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }
}

fn has_scheme(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once(':') else {
        return false;
    };
    let valid = !scheme.is_empty()
        && scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    if !valid {
        return false;
    }
    // "host:8080" is a host and port, not a scheme
    rest.starts_with("//")
        || matches!(
            scheme.to_ascii_lowercase().as_str(),
            "about" | "data" | "file" | "mailto" | "view-source" | "javascript" | "blob"
        )
}

fn looks_like_host(s: &str) -> bool {
    let end = s.find(['/', '?', '#']).unwrap_or(s.len());
    let authority = &s[..end];
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host_port.starts_with('[') {
        return host_port.contains(']'); // IPv6 literal
    }
    let (host, port) = match host_port.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (h, Some(p)),
        Some(_) => return false,
        None => (host_port, None),
    };
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() == 4 && labels.iter().all(|l| l.parse::<u8>().is_ok()) {
        return true;
    }
    let valid_label =
        |l: &&str| !l.is_empty() && l.chars().all(|c| c.is_alphanumeric() || c == '-');
    let tld_ok = labels
        .last()
        .is_some_and(|t| t.chars().count() >= 2 && t.chars().all(|c| c.is_alphabetic()));
    let _ = port;
    labels.len() >= 2 && labels.iter().all(valid_label) && tld_ok
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Turn omnibox text into a URL: explicit URLs pass through, bare hosts get a scheme, anything
/// else becomes a search (`search` contains `%s`).
pub fn resolve(input: &str, search: &str) -> String {
    let s = input.trim();
    if s.is_empty() {
        return "about:blank".into();
    }
    if has_scheme(s) {
        return s.to_owned();
    }
    if !s.contains(char::is_whitespace) && looks_like_host(s) {
        let host = s.split(['/', '?', '#']).next().unwrap_or(s);
        let local = host.starts_with("localhost")
            || host.starts_with('[')
            || host.bytes().next().is_some_and(|b| b.is_ascii_digit());
        return format!("{}://{s}", if local { "http" } else { "https" });
    }
    search.replace("%s", &percent_encode(s))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH: &str = "https://duckduckgo.com/?q=%s";

    #[test]
    fn resolves_urls_hosts_and_searches() {
        assert_eq!(
            resolve("https://example.org/a", SEARCH),
            "https://example.org/a"
        );
        assert_eq!(resolve("example.org", SEARCH), "https://example.org");
        assert_eq!(
            resolve("example.org/a?b=1", SEARCH),
            "https://example.org/a?b=1"
        );
        assert_eq!(
            resolve("localhost:3000/x", SEARCH),
            "http://localhost:3000/x"
        );
        assert_eq!(resolve("127.0.0.1:8080", SEARCH), "http://127.0.0.1:8080");
        assert_eq!(resolve("about:blank", SEARCH), "about:blank");
        assert_eq!(
            resolve("rust lang", SEARCH),
            "https://duckduckgo.com/?q=rust%20lang"
        );
        assert_eq!(
            resolve("what is 1.5?", SEARCH),
            "https://duckduckgo.com/?q=what%20is%201.5%3F"
        );
        assert_eq!(
            resolve("café", SEARCH),
            "https://duckduckgo.com/?q=caf%C3%A9"
        );
        assert_eq!(resolve("", SEARCH), "about:blank");
        assert_eq!(
            resolve("  rust  ", SEARCH),
            "https://duckduckgo.com/?q=rust"
        );
        // single words and version-like strings are searches, not hosts
        assert_eq!(resolve("rust", SEARCH), "https://duckduckgo.com/?q=rust");
        assert_eq!(resolve("v1.2", SEARCH), "https://duckduckgo.com/?q=v1.2");
    }

    #[test]
    fn host_with_port_is_not_a_scheme() {
        assert_eq!(
            resolve("example.org:8080", SEARCH),
            "https://example.org:8080"
        );
    }

    #[test]
    fn editor_handles_graphemes_and_wide_text() {
        let mut e = LineEdit::new("");
        e.insert_str("a日e\u{301}");
        assert_eq!(e.cursor_col(), 1 + 2 + 1);
        e.backspace(); // removes é as one unit
        assert_eq!(e.text, "a日");
        e.left();
        assert_eq!(e.cursor_col(), 1);
        e.insert('x');
        assert_eq!(e.text, "ax日");
        e.home();
        e.delete();
        assert_eq!(e.text, "x日");
        e.end();
        e.kill_to_start();
        assert_eq!(e.text, "");
    }

    #[test]
    fn kill_word_and_end() {
        let mut e = LineEdit::new("hello big world");
        e.kill_word();
        assert_eq!(e.text, "hello big ");
        e.left();
        e.left();
        e.kill_to_end();
        assert_eq!(e.text, "hello bi");
        let mut e = LineEdit::new("https://a.org/path");
        e.kill_word();
        assert_eq!(e.text, "https://a.org/");
    }

    #[test]
    fn control_chars_are_not_inserted() {
        let mut e = LineEdit::new("");
        e.insert_str("a\nb\tc");
        assert_eq!(e.text, "abc");
    }
}
