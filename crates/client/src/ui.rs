//! Drawing: tab bar, omnibox, page grid with overlays, status bar, help.

use std::collections::BTreeMap;

use glyph_proto::{width::str_width, Attrs, Rgb};
use ratatui::{
    layout::{Position, Rect},
    style::{Modifier, Style},
    Frame,
};

use crate::{
    app::{tab_boxes, tab_label, App, Mode, CONTENT_TOP, OMNI_ROW, TAB_ROW},
    color::ColorDepth,
    keymap::KeyMode,
};

fn style(d: ColorDepth, fg: Rgb, bg: Rgb) -> Style {
    Style::default().fg(d.convert(fg)).bg(d.convert(bg))
}

/// Write `s` at `(x, y)` clipped to `max_x`; returns the next x.
fn text(f: &mut Frame, x: u16, y: u16, s: &str, st: Style, max_x: u16) -> u16 {
    let buf = f.buffer_mut();
    let (mut cx, area) = (x, buf.area);
    for g in glyph_proto::width::clusters(s) {
        let w = glyph_proto::width::cluster_width(g) as u16;
        if w == 0 {
            continue;
        }
        if cx + w > max_x.min(area.right()) || y >= area.bottom() {
            break;
        }
        buf[(cx, y)].set_symbol(g).set_style(st);
        for dx in 1..w {
            buf[(cx + dx, y)].reset();
            buf[(cx + dx, y)].set_style(st);
        }
        cx += w;
    }
    cx
}

fn fill(f: &mut Frame, y: u16, st: Style) {
    let buf = f.buffer_mut();
    for x in buf.area.left()..buf.area.right() {
        buf[(x, y)].set_symbol(" ").set_style(st);
    }
}

pub fn draw(f: &mut Frame, app: &App, d: ColorDepth) {
    let area = f.area();
    if area.width < 10 || area.height < 5 {
        return;
    }
    draw_tabs(f, app, d);
    draw_omnibox(f, app, d);
    draw_page(f, app, d, area);
    draw_status(f, app, d, area);
    if app.mode == Mode::Help {
        draw_help(f, app, d, area);
    }
    // caret
    match &app.mode {
        Mode::Omnibox { .. } => {
            f.set_cursor_position(Position::new((3 + app.omni.cursor_col()) as u16, OMNI_ROW))
        }
        Mode::Find => f.set_cursor_position(Position::new(
            (1 + app.find.cursor_col()) as u16,
            area.height - 1,
        )),
        Mode::Insert => {
            if let Some(c) = app.cursor {
                f.set_cursor_position(Position::new(c.col, CONTENT_TOP + c.row));
            }
        }
        _ => {}
    }
}

fn draw_tabs(f: &mut Frame, app: &App, d: ColorDepth) {
    let th = app.cfg.theme;
    let width = f.area().width;
    fill(f, TAB_ROW, style(d, th.tab_fg, th.tab_bar_bg));
    let (boxes, plus) = tab_boxes(&app.tabs, width);
    for (i, (b, t)) in boxes.iter().zip(&app.tabs).enumerate() {
        let active = t.id == app.active;
        let (fg, bg) = if active {
            (th.tab_active_fg, th.tab_active_bg)
        } else {
            (th.tab_fg, th.tab_bar_bg)
        };
        let mut st = style(d, fg, bg);
        if active {
            st = st.add_modifier(Modifier::BOLD);
        }
        let label = tab_label(i, t, (b.x1 - b.x0) as usize - 5);
        let mut x = b.x0;
        while x < b.x1.min(width) {
            f.buffer_mut()[(x, TAB_ROW)].set_symbol(" ").set_style(st);
            x += 1;
        }
        text(f, b.x0, TAB_ROW, &label, st, b.x1);
        text(f, b.close_x - 1, TAB_ROW, "×", st, b.x1);
    }
    text(
        f,
        plus,
        TAB_ROW,
        " + ",
        style(d, th.accent, th.tab_bar_bg).add_modifier(Modifier::BOLD),
        width,
    );
}

fn draw_omnibox(f: &mut Frame, app: &App, d: ColorDepth) {
    let th = app.cfg.theme;
    let w = f.area().width;
    fill(f, OMNI_ROW, style(d, th.omnibox_fg, th.omnibox_bg));
    match &app.mode {
        Mode::Omnibox { new_tab } => {
            let prompt = if *new_tab { "+ " } else { "> " };
            text(
                f,
                1,
                OMNI_ROW,
                prompt,
                style(d, th.accent, th.omnibox_bg).add_modifier(Modifier::BOLD),
                w,
            );
            text(
                f,
                3,
                OMNI_ROW,
                &app.omni.text,
                style(d, th.omnibox_fg, th.omnibox_bg),
                w,
            );
        }
        _ => {
            let mark = if app.url.starts_with("https://") {
                "▪ "
            } else {
                "▫ "
            };
            text(f, 1, OMNI_ROW, mark, style(d, th.accent, th.omnibox_bg), w);
            let shown = if app.url.is_empty() {
                "about:blank"
            } else {
                &app.url
            };
            let end = text(
                f,
                3,
                OMNI_ROW,
                shown,
                style(d, th.omnibox_fg, th.omnibox_bg),
                w,
            );
            if !app.title.is_empty() && end + 6 < w {
                text(
                    f,
                    end + 2,
                    OMNI_ROW,
                    &format!("— {}", app.title),
                    style(d, th.tab_fg, th.omnibox_bg),
                    w,
                );
            }
        }
    }
}

fn draw_page(f: &mut Frame, app: &App, d: ColorDepth, area: Rect) {
    let th = app.cfg.theme;
    let (cols, rows) = app.content();
    let Some(g) = &app.grid else {
        let st = style(d, th.tab_fg, th.omnibox_bg);
        for y in 0..rows {
            let buf = f.buffer_mut();
            for x in 0..cols.min(area.width) {
                buf[(x, CONTENT_TOP + y)].set_symbol(" ").set_style(st);
            }
        }
        text(
            f,
            2,
            CONTENT_TOP + 1,
            "waiting for the browser…",
            st,
            area.width,
        );
        return;
    };
    let buf = f.buffer_mut();
    for y in 0..rows.min(g.rows()) {
        for x in 0..cols.min(g.cols()).min(area.width) {
            let c = g.cell(x, y);
            let cell = &mut buf[(x, CONTENT_TOP + y)];
            if c.is_cont() {
                cell.reset();
                cell.set_style(Style::default().bg(d.convert(c.style.bg)));
                continue;
            }
            let mut m = Modifier::empty();
            let a = c.style.attrs;
            if a.contains(Attrs::BOLD) {
                m |= Modifier::BOLD;
            }
            if a.contains(Attrs::ITALIC) {
                m |= Modifier::ITALIC;
            }
            if a.contains(Attrs::UNDERLINE) {
                m |= Modifier::UNDERLINED;
            }
            if a.contains(Attrs::STRIKE) {
                m |= Modifier::CROSSED_OUT;
            }
            if a.contains(Attrs::DIM) {
                m |= Modifier::DIM;
            }
            if app.selection.is_some_and(|s| s.contains(x, y)) {
                m |= Modifier::REVERSED;
            }
            cell.set_symbol(&c.g)
                .set_style(style(d, c.style.fg, c.style.bg).add_modifier(m));
        }
    }
    if let Mode::Hint(h) = &app.mode {
        let hint_st = style(d, th.hint_fg, th.hint_bg).add_modifier(Modifier::BOLD);
        let typed_st = style(d, th.hint_fg, Rgb(0xa6, 0xe3, 0xa1)).add_modifier(Modifier::BOLD);
        for (label, id) in &h.items {
            if !label.starts_with(&h.typed) {
                continue;
            }
            let Some(r) = app.region_by_id(*id).and_then(|r| r.rects.first()) else {
                continue;
            };
            let up = label.to_uppercase();
            let n = h.typed.chars().count();
            let (a, b) = (
                up.chars().take(n).collect::<String>(),
                up.chars().skip(n).collect::<String>(),
            );
            let mut x = r.x;
            x = text(f, x, CONTENT_TOP + r.y, &a, typed_st, cols);
            text(f, x, CONTENT_TOP + r.y, &b, hint_st, cols);
        }
    }
}

fn draw_status(f: &mut Frame, app: &App, d: ColorDepth, area: Rect) {
    let th = app.cfg.theme;
    let y = area.height - 1;
    let w = area.width;
    let bar = style(d, th.status_fg, th.status_bg);
    fill(f, y, bar);
    let (tag, tag_bg) = match &app.mode {
        Mode::Normal => (
            if app.text_mode {
                " READER "
            } else {
                " NORMAL "
            },
            th.accent,
        ),
        Mode::Insert => (" INSERT ", Rgb(0xa6, 0xe3, 0xa1)),
        Mode::Omnibox { .. } => (" OPEN ", Rgb(0xf9, 0xe2, 0xaf)),
        Mode::Find => (" FIND ", Rgb(0xf9, 0xe2, 0xaf)),
        Mode::Hint(_) => (" HINT ", th.hint_bg),
        Mode::Visual => (" VISUAL ", Rgb(0xcb, 0xa6, 0xf7)),
        Mode::Help => (" HELP ", th.accent),
    };
    let mut x = text(
        f,
        0,
        y,
        tag,
        style(d, Rgb(0x11, 0x11, 0x1b), tag_bg).add_modifier(Modifier::BOLD),
        w,
    );
    x += 1;

    // right side first, so the left can be clipped against it
    let mut right = String::new();
    if app.load.loading {
        let filled = (app.load.progress as usize * 10 / 100).min(10);
        right.push_str(&format!(
            "{}{} {:>3}%  ",
            "█".repeat(filled),
            "░".repeat(10 - filled),
            app.load.progress
        ));
    }
    right.push_str(&format!("{:>3}%", app.scroll_permille / 10));
    let rw = str_width(&right) as u16 + 1;
    let rx = w.saturating_sub(rw);
    let st_right = if app.load.loading {
        style(d, th.progress, th.status_bg)
    } else {
        bar
    };
    text(f, rx, y, &right, st_right, w);

    let left = match &app.mode {
        Mode::Find => {
            let m = app
                .find_matches
                .map(|n| format!("   {n} match{}", if n == 1 { "" } else { "es" }))
                .unwrap_or_default();
            let end = text(f, x, y, &format!("/{}", app.find.text), bar, rx);
            text(f, end, y, &m, style(d, th.tab_fg, th.status_bg), rx);
            return;
        }
        Mode::Hint(h) => format!("follow link: type a label ({})", h.typed.to_uppercase()),
        _ => app
            .message
            .as_ref()
            .map(|(m, _)| m.clone())
            .or_else(|| {
                app.hover
                    .and_then(|(cx, cy)| app.region_at(cx, cy))
                    .and_then(|r| r.href.clone())
            })
            .unwrap_or_else(|| {
                if let Some(m) = app.find_matches {
                    format!("{m} match{}", if m == 1 { "" } else { "es" })
                } else {
                    String::new()
                }
            }),
    };
    text(f, x, y, &left, bar, rx);
}

fn draw_help(f: &mut Frame, app: &App, d: ColorDepth, area: Rect) {
    let th = app.cfg.theme;
    let mut by_action: BTreeMap<usize, (String, Vec<String>)> = BTreeMap::new();
    for (spec, a) in app.cfg.keymap.list(KeyMode::Normal) {
        by_action
            .entry(a.order())
            .or_insert_with(|| (a.describe(), Vec::new()))
            .1
            .push(spec);
    }
    let entries: Vec<(String, String)> = by_action
        .into_values()
        .map(|(desc, keys)| (keys.join(" "), desc))
        .collect();
    let key_w = entries
        .iter()
        .map(|(k, _)| str_width(k))
        .max()
        .unwrap_or(8)
        .min(22);
    let ncols = (area.width / 44).clamp(1, 3) as usize;
    let col_w = (area.width as usize - 4) / ncols;
    let cap_rows = (area.height as usize).saturating_sub(7).max(1);
    let per_col = entries.len().div_ceil(ncols).min(cap_rows);
    let h = (per_col + 4) as u16;
    let top = (area.height.saturating_sub(h)) / 2;
    let bg = style(d, th.help_fg, th.help_bg);
    for y in top..top + h {
        let buf = f.buffer_mut();
        for x in 1..area.width - 1 {
            buf[(x, y)].set_symbol(" ").set_style(bg);
        }
    }
    text(
        f,
        3,
        top + 1,
        "glyph — keys (any key closes)   configure in ~/.config/glyph/config.toml",
        bg.add_modifier(Modifier::BOLD),
        area.width - 2,
    );
    for (i, (keys, desc)) in entries.iter().enumerate() {
        let (c, r) = (i / per_col, i % per_col);
        if c >= ncols {
            break;
        }
        let x = 3 + (c * col_w) as u16;
        let y = top + 3 + r as u16;
        let kw = key_w.min(col_w / 2);
        text(
            f,
            x,
            y,
            keys,
            style(d, th.accent, th.help_bg).add_modifier(Modifier::BOLD),
            x + kw as u16,
        );
        text(f, x + kw as u16 + 1, y, desc, bg, x + col_w as u16 - 1);
    }
}
