//! Shared types for glyph: cell model, grid, diffs and the wire messages.

pub mod cell;
pub mod codec;
pub mod diff;
pub mod msg;
pub mod width;

pub use cell::{Attrs, Cell, Grid, Rgb, Style};
pub use diff::{apply_runs, diff_runs, full_runs, Run, Span};
pub use msg::*;
