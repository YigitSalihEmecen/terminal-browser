//! Grapheme-cluster segmentation and terminal cell widths.
//!
//! Both ends of a connection must agree on widths, so this is the single source of truth.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Extended grapheme clusters of `s`.
pub fn clusters(s: &str) -> impl DoubleEndedIterator<Item = &str> {
    s.graphemes(true)
}

/// Cells occupied by one grapheme cluster: `0` (cannot be drawn: controls, lone joiners),
/// `1`, or `2` (CJK, emoji, flags, ZWJ sequences). Never more than 2, so a cluster always
/// fits a head cell plus at most one continuation cell.
pub fn cluster_width(g: &str) -> u8 {
    let Some(first) = g.chars().next() else {
        return 0;
    };
    if first.is_control() {
        return 0;
    }
    if g.is_ascii() {
        return 1;
    }
    match UnicodeWidthStr::width(g) {
        0 => 0,
        1 => 1,
        _ => 2,
    }
}

/// Total cell width of `s` after dropping undrawable clusters.
pub fn str_width(s: &str) -> usize {
    clusters(s).map(|g| cluster_width(g) as usize).sum()
}

/// Would `next` fuse with the final cluster of `text` if the two strings were concatenated and
/// re-segmented? (Regional-indicator pairs, base + combining mark, Hangul jamo, ZWJ chains.)
pub fn fuses(text: &str, next: &str) -> bool {
    let (Some(last_c), Some(next_c)) = (text.chars().next_back(), next.chars().next()) else {
        return false;
    };
    if last_c.is_ascii() && next_c.is_ascii() {
        // ASCII only fuses across CR LF, which never reaches a cell.
        return false;
    }
    let tail = clusters(text).next_back().unwrap_or("");
    let mut joined = String::with_capacity(tail.len() + next.len());
    joined.push_str(tail);
    joined.push_str(next);
    // Must re-segment to exactly [tail, next]; count alone misses flag-pair parity shifts.
    let mut it = clusters(&joined);
    !(it.next() == Some(tail) && it.next() == Some(next) && it.next().is_none())
}

/// Slice `s` by UTF-16 code-unit offsets (what Chromium's DOMSnapshot reports).
pub fn utf16_slice(s: &str, start: usize, len: usize) -> &str {
    let end = start + len;
    let (mut u16_pos, mut byte_start, mut byte_end) = (0usize, None, s.len());
    for (b, c) in s.char_indices() {
        if u16_pos >= start && byte_start.is_none() {
            byte_start = Some(b);
        }
        if u16_pos >= end {
            byte_end = b;
            break;
        }
        u16_pos += c.len_utf16();
    }
    match byte_start {
        Some(bs) if bs <= byte_end => &s[bs..byte_end],
        _ => "",
    }
}
