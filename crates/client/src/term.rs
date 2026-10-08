//! Terminal setup, input conversion and the async event loop.

use std::{future::Future, io::Write, time::Duration};

use anyhow::{anyhow, Result};
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, EventStream, KeyCode as CtKey, KeyEvent as CtKeyEvent, KeyEventKind, KeyModifiers,
        MouseButton as CtButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use glyph_proto::{
    ClientCaps, ClientMsg, GraphicsProto, KeyCode, KeyEvent, Mods, MouseButton, ServerMsg,
};
use ratatui::{backend::CrosstermBackend, layout::Rect, Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::{
    app::{content_size, App, MouseIn, MouseKindIn, Out},
    color::ColorDepth,
    config::Config,
    osc52, ui,
};

/// A live link to a server: in-process channels (`local`) or a WebSocket pump (`connect`).
pub struct Connection {
    pub tx: UnboundedSender<ClientMsg>,
    pub rx: UnboundedReceiver<ServerMsg>,
}

pub fn convert_key(k: CtKeyEvent) -> Option<KeyEvent> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let mut mods = Mods::default();
    if k.modifiers.contains(KeyModifiers::SHIFT) {
        mods.0 |= Mods::SHIFT.0;
    }
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        mods.0 |= Mods::CTRL.0;
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        mods.0 |= Mods::ALT.0;
    }
    if k.modifiers.contains(KeyModifiers::SUPER) || k.modifiers.contains(KeyModifiers::META) {
        mods.0 |= Mods::META.0;
    }
    let code = match k.code {
        CtKey::Char(c) => KeyCode::Char(c),
        CtKey::Enter => KeyCode::Enter,
        CtKey::Tab => KeyCode::Tab,
        CtKey::BackTab => {
            mods.0 |= Mods::SHIFT.0;
            KeyCode::Tab
        }
        CtKey::Backspace => KeyCode::Backspace,
        CtKey::Delete => KeyCode::Delete,
        CtKey::Esc => KeyCode::Esc,
        CtKey::Left => KeyCode::Left,
        CtKey::Right => KeyCode::Right,
        CtKey::Up => KeyCode::Up,
        CtKey::Down => KeyCode::Down,
        CtKey::Home => KeyCode::Home,
        CtKey::End => KeyCode::End,
        CtKey::PageUp => KeyCode::PageUp,
        CtKey::PageDown => KeyCode::PageDown,
        CtKey::Insert => KeyCode::Insert,
        CtKey::F(n) => KeyCode::F(n),
        _ => return None,
    };
    Some(KeyEvent { code, mods })
}

fn convert_button(b: CtButton) -> MouseButton {
    match b {
        CtButton::Left => MouseButton::Left,
        CtButton::Middle => MouseButton::Middle,
        CtButton::Right => MouseButton::Right,
    }
}

pub fn convert_mouse(m: crossterm::event::MouseEvent) -> Option<MouseIn> {
    let kind = match m.kind {
        MouseEventKind::Down(b) => MouseKindIn::Down(convert_button(b)),
        MouseEventKind::Up(b) => MouseKindIn::Up(convert_button(b)),
        MouseEventKind::Drag(b) => MouseKindIn::Drag(convert_button(b)),
        MouseEventKind::Moved => MouseKindIn::Move,
        MouseEventKind::ScrollUp => MouseKindIn::ScrollUp,
        MouseEventKind::ScrollDown => MouseKindIn::ScrollDown,
        MouseEventKind::ScrollLeft => MouseKindIn::ScrollLeft,
        MouseEventKind::ScrollRight => MouseKindIn::ScrollRight,
    };
    let mut mods = Mods::default();
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        mods.0 |= Mods::SHIFT.0;
    }
    if m.modifiers.contains(KeyModifiers::CONTROL) {
        mods.0 |= Mods::CTRL.0;
    }
    if m.modifiers.contains(KeyModifiers::ALT) {
        mods.0 |= Mods::ALT.0;
    }
    Some(MouseIn {
        kind,
        x: m.column,
        y: m.row,
        mods,
    })
}

/// Restores the terminal on drop, including on panic.
struct TermGuard {
    mouse: bool,
}

impl TermGuard {
    fn enter(mouse: bool) -> Result<Self> {
        enable_raw_mode()?;
        execute!(
            std::io::stdout(),
            EnterAlternateScreen,
            EnableBracketedPaste
        )?;
        if mouse {
            execute!(std::io::stdout(), EnableMouseCapture)?;
        }
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore(true);
            prev(info);
        }));
        Ok(Self { mouse })
    }
}

fn restore(mouse: bool) -> std::io::Result<()> {
    if mouse {
        execute!(std::io::stdout(), DisableMouseCapture)?;
    }
    execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    disable_raw_mode()
}

impl Drop for TermGuard {
    fn drop(&mut self) {
        let _ = restore(self.mouse);
    }
}

fn detect_caps(cfg: &Config, size: (u16, u16)) -> ClientCaps {
    let (cols, rows) = content_size(size.0, size.1);
    let (cell_px_w, cell_px_h) = match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 && ws.columns > 0 && ws.rows > 0 => {
            (ws.width / ws.columns, ws.height / ws.rows)
        }
        _ => (0, 0),
    };
    let graphics = crate::gfx::detect(&cfg.images.protocol);
    ClientCaps {
        cols,
        rows,
        cell_px_w,
        cell_px_h,
        graphics,
        scheme: crate::color::color_scheme(&cfg.ui.color_scheme),
        page_style: if cfg.ui.page_style == "faithful" {
            glyph_proto::PageStyle::Faithful
        } else {
            glyph_proto::PageStyle::Terminal
        },
        images: graphics != GraphicsProto::None || cfg.images.always_load,
    }
}

/// Run the TUI until quit. `connect` receives the client capabilities and returns the link.
pub async fn run<F, Fut>(cfg: Config, connect: F) -> Result<()>
where
    F: FnOnce(ClientCaps) -> Fut,
    Fut: Future<Output = Result<Connection>>,
{
    let size = crossterm::terminal::size()?;
    let caps = detect_caps(&cfg, size);
    let mut conn = connect(caps).await?;
    let depth = ColorDepth::detect(&cfg.ui.color);
    let _guard = TermGuard::enter(cfg.ui.mouse)?;
    // A fixed viewport avoids ratatui's cursor-position query, which some terminals, multiplexers
    // and slow links never answer.
    let mut terminal = Terminal::with_options(
        CrosstermBackend::new(std::io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, size.0, size.1)),
        },
    )?; // (no terminal.clear(): it queries the cursor; the alternate screen starts blank)
    let in_tmux = std::env::var_os("TMUX").is_some();
    let mut app = App::new(cfg, size);
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut images =
        crate::gfx::Images::new(caps.graphics, (caps.cell_px_w, caps.cell_px_h), depth);
    let mut last_error: Option<String> = None;

    'main: loop {
        tokio::select! {
            ev = events.next() => {
                let Some(ev) = ev else { break };
                let out = match ev? {
                    Event::Key(k) => convert_key(k).map(|k| app.on_key(k)),
                    Event::Mouse(m) => convert_mouse(m).map(|m| app.on_mouse(m)),
                    Event::Paste(s) => Some(app.on_paste(s)),
                    Event::Resize(w, h) => { terminal.resize(Rect::new(0, 0, w, h))?; Some(app.on_resize(w, h)) }
                    _ => None,
                };
                if let Some(out) = out {
                    if apply(out, &conn, &mut terminal, in_tmux)? { break 'main; }
                }
            }
            msg = conn.rx.recv() => {
                let Some(msg) = msg else {
                    return Err(anyhow!("{}", last_error.unwrap_or_else(|| "server closed the connection".into())));
                };
                if let ServerMsg::Error(e) = &msg { last_error = Some(e.clone()); }
                let out = app.on_server(msg);
                if apply(out, &conn, &mut terminal, in_tmux)? { break 'main; }
            }
            _ = tick.tick() => app.on_tick(),
        }
        // fold in everything already waiting before paying for a redraw
        while let Ok(msg) = conn.rx.try_recv() {
            if let ServerMsg::Error(e) = &msg {
                last_error = Some(e.clone());
            }
            let out = app.on_server(msg);
            if apply(out, &conn, &mut terminal, in_tmux)? {
                break 'main;
            }
        }
        if app.dirty {
            app.dirty = false;
            terminal.draw(|f| ui::draw(f, &app, depth))?;
            if std::mem::take(&mut app.images_reset) {
                images.reset(terminal.backend_mut(), &app)?;
            }
            images.draw(terminal.backend_mut(), &mut app)?;
            // ack only once the frame is on the terminal: a slow terminal slows the server down
            if let Some(ack) = app.take_ack() {
                let _ = conn.tx.send(ack);
            }
        }
    }
    Ok(())
}

/// Returns true when the app asked to quit.
fn apply(
    out: Out,
    conn: &Connection,
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    in_tmux: bool,
) -> Result<bool> {
    for m in out.send {
        let _ = conn.tx.send(m);
    }
    for text in out.copy {
        let w = terminal.backend_mut();
        w.write_all(osc52::sequence(&text, in_tmux).as_bytes())?;
        w.flush()?;
    }
    Ok(out.quit)
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;

    use super::*;

    fn ev(code: CtKey, m: KeyModifiers) -> CtKeyEvent {
        CtKeyEvent {
            code,
            modifiers: m,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    #[test]
    fn converts_keys() {
        let k = convert_key(ev(CtKey::Char('d'), KeyModifiers::CONTROL)).unwrap();
        assert_eq!(k.code, KeyCode::Char('d'));
        assert!(k.mods.contains(Mods::CTRL));
        let k = convert_key(ev(CtKey::BackTab, KeyModifiers::SHIFT)).unwrap();
        assert_eq!(k.code, KeyCode::Tab);
        assert!(k.mods.contains(Mods::SHIFT));
        assert!(convert_key(ev(CtKey::Null, KeyModifiers::NONE)).is_none());
        let mut rel = ev(CtKey::Char('a'), KeyModifiers::NONE);
        rel.kind = KeyEventKind::Release;
        assert!(convert_key(rel).is_none());
    }
}
