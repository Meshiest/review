//! Shared scrolling for every scrolling view: a draggable scrollbar and
//! browser-style middle-click autoscroll.
//!
//! The behavior is plain Rust driven by a [`Time`] argument, so it is tested
//! without a window: [`ScrollAxis`] holds the position, [`scrollbar`] maps the
//! thumb and handles drag, paging and hover, [`autoscroll`] is the gesture's
//! state machine. [`Scroller`] glues them to gpui: a view owns one, calls
//! [`Scroller::frame`] while preparing to paint, [`Scroller::paint`] after its
//! content, and [`install`] from its element's paint to receive mouse input.

pub mod autoscroll;
mod axis;
mod glue;
mod handle;
mod marker;
pub mod scrollbar;
mod style;
mod time;

pub use autoscroll::{Autoscroll, AutoscrollConfig};
pub use axis::{ScrollAxis, ScrollState};
pub use glue::{Orientation, Scroller, StartFilter, install};
pub use handle::HandleAutoscroll;
pub use marker::paint_autoscroll_marker;
pub use scrollbar::ScrollbarState;
pub use style::{AutoscrollStyle, ScrollStyle, ScrollbarStyle, mix};
pub use time::{Time, now};
