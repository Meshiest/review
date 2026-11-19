//! The diff list: a virtualized gpui element over `diff` rows.
//!
//! [`DiffView`] is an entity that renders a [`DiffElement`], which shapes and
//! paints only the rows in view, and selects and copies their text.
//! [`FrameClock`] measures frame CPU time for the scroll benchmarks.

mod blocks;
mod frame_clock;
mod permalink;
mod select;
mod shape;
mod stats;
mod style;
mod view;

pub use blocks::{HeaderPlace, HeaderSlots, Rail, RenderBlock, RenderHeader};
pub use frame_clock::FrameClock;
pub use permalink::LineAnchor;
pub use stats::{BAR_BLOCKS, BarBlock, change_bar, line_counts};
pub use style::{DiffStyle, SyntaxPalette};
pub use view::{ContextTarget, DiffElement, DiffView, DiffViewEvent, FrameStats, Step};
