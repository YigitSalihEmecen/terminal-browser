//! Terminal graphics (Kitty / Sixel / iTerm2). Filled in at M5; until then a no-op shim so the
//! event loop has its final shape.

use std::io::Write;

use anyhow::Result;
use glyph_proto::{GraphicsProto, ServerMsg};

use crate::app::App;

/// Which protocol to use: config override first, then environment hints.
pub fn detect(config: &str) -> GraphicsProto {
    match config {
        "kitty" => GraphicsProto::Kitty,
        "sixel" => GraphicsProto::Sixel,
        "iterm2" => GraphicsProto::Iterm2,
        _ => GraphicsProto::None,
    }
}

pub struct Images {
    _proto: GraphicsProto,
}

impl Images {
    pub fn new(proto: GraphicsProto) -> Self {
        Self { _proto: proto }
    }
    pub fn on_server(&mut self, _m: &ServerMsg) {}
    pub fn draw(&mut self, _out: &mut impl Write, _app: &App) -> Result<()> {
        Ok(())
    }
}
