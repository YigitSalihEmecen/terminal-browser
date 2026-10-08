//! glyph terminal client: state machine (`app`), drawing (`ui`) and the terminal loop (`term`).

pub mod app;
pub mod color;
pub mod config;
pub mod gfx;
pub mod hints;
pub mod keymap;
pub mod omnibox;
pub mod osc52;
pub mod term;
pub mod ui;

pub use config::Config;
pub use term::{run, Connection};
