//! The terminal pane: a gpui element that paints a `term` grid and sends input to it.

mod blink;
mod element;
mod frame;
mod geometry;
mod glyphs;
mod keys;
mod mouse;
mod select;
mod snapshot;
mod style;
mod view;

pub use glyphs::{SYMBOLS_FAMILY, init};
pub use style::TermStyle;
pub use view::{TerminalEvent, TerminalView};
