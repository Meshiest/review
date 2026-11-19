//! The diff model and its projection into unified or split rows.
//!
//! [`Diff`] holds per-file blobs ([`Text`]) and hunks as line ranges.
//! [`DiffList`] projects it into one continuous list of [`Row`]s with a
//! [`HeightIndex`] for scroll offset to row lookups; switching layout
//! re-projects rows without touching the model. [`highlight`] colors whole
//! blobs with tree-sitter on background threads, and [`word_diff`] finds the
//! changed words inside a replaced line.

mod heights;
pub mod highlight;
mod model;
mod reveal;
mod rows;
pub mod synth;
mod text;
mod words;

pub use heights::{HeightIndex, RowHeight, RowOffset};
pub use highlight::{HighlightKind, Highlighter, Highlights, Language, Span};
pub use model::{Anchor, Block, BlockId, Diff, FileDiff, FileIx, Hunk, Segment, Side};
pub use reveal::{EXPAND_STEP, Expand, Gap, GapReveal, Reveals, arrows};
pub use rows::{ContextMode, DiffList, HunkSpan, Layout, LineKind, Row, RowHeights, SideLine};
pub use text::{BlobId, Text};
pub use words::{MAX_LINE_BYTES, MIN_SHARED, Pairs, WordDiff, word_diff};
