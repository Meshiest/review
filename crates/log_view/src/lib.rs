//! The CI log viewer: a virtualized gpui element over ANSI-colored log lines
//! that tails a running job.
//!
//! Bytes go in through a [`LogFeed`], which parses them on a worker thread
//! into a [`LogBuffer`]: complete lines with style spans, `##[group]`
//! sections and GitHub timestamps. A [`LogView`] paints the lines in view
//! from a cache of shaped lines, follows the end of the log until scrolled
//! up, and searches off the UI thread.

mod ansi;
mod buffer;
mod feed;
mod parser;
mod projection;
mod search;
mod shape;
mod style;
mod timestamp;
mod view;

pub use ansi::{Attrs, Color, Span, Style, StyleId};
pub use buffer::{BLOCK_LINES, Batch, Group, LineKind, LogBuffer, LogLine, MAX_BYTES, MAX_LINES};
pub use feed::LogFeed;
pub use parser::{LogParser, MAX_LINE_BYTES};
pub use search::{MAX_MATCHES, Match, Query, find_all};
pub use style::LogStyle;
pub use view::{FrameStats, LogEvent, LogView};
