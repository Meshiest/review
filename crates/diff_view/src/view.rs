//! The diff list view and the element that paints its visible rows.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::ops::Range;
use std::sync::Arc;

use diff::{
    BlobId, Block, BlockId, Diff, DiffList, Expand, FileIx, Highlighter, Highlights, Language,
    Layout, LineKind, Pairs, Row, RowHeights, Side, SideLine, Text, WordDiff, word_diff,
};
use gpui::{
    AnyElement, App, AvailableSpace, BorderStyle, Bounds, ContentMask, Context, Corners,
    CursorStyle, DispatchPhase, Edges, Element, ElementId, Entity, Font, GlobalElementId, Hitbox,
    HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PathBuilder, Pixels, Point, Render, ScrollEvent, Style,
    Subscription, Task, TextAlign, Window, fill, font, point, px, quad, relative, size,
};

use scroll::{Scroller, now};

mod cursor;
mod divider;
mod expand;
mod files;
mod find;
mod menu;
mod selecting;

pub use expand::ExpandArrow;
pub use files::DiffViewEvent;
pub use menu::ContextTarget;

use crate::DiffStyle;
use crate::blocks::{HeaderSlots, Rail, RenderBlock, RenderHeader, assign_lanes};
use crate::find::FindAt;
use crate::select::{CodeLine, Selection, blob as line_blob, char_count, display_col};
use crate::shape::{
    Code, CodeStyle, LineCache, LineKey, Quality, ShapeStyle, push_code, run, shaped_columns,
};
use crate::stats::{BAR_BLOCKS, BarBlock, change_bar, line_counts};
use files::{Pinned, chevron_zone, path_left};

/// Extra rows shaped and painted above and below the viewport.
const OVERSCAN: usize = 2;

/// Word diffs kept past the shaped-line cache, so scrolling back over a pair
/// does not diff it again. Cleared when it grows past this many pairs.
const WORD_CACHE_PAIRS: usize = 4096;

/// Measure-and-anchor rounds per frame. Each round measures the blocks that
/// came into view after the last one moved the rows; heights still changing
/// after that are picked up by the next frame.
const BLOCK_PASSES: usize = 4;

/// Rail geometry: the marker on commented rows, the rail line, and how far
/// apart side-by-side rails sit.
const MARKER_WIDTH: f32 = 6.0;
const RAIL_WIDTH: f32 = 2.0;
const LANE_STEP: f32 = 4.0;
const ELBOW_RADIUS: f32 = 6.0;

/// The viewed checkbox: its corner radius as a share of its side, and its
/// tick as three points and a stroke width in shares of the side.
const CHECKBOX_RADIUS_DIVISOR: f32 = 5.0;
const TICK: [(f32, f32); 3] = [(0.24, 0.52), (0.43, 0.72), (0.78, 0.30)];
const TICK_WIDTH: f32 = 0.14;

/// The change bar's blocks: their side and the gap between them, in shares
/// of a column, and their corner radius as a share of their side.
const BAR_BLOCK: f32 = 1.0;
const BAR_GAP: f32 = 0.25;
const BAR_RADIUS_DIVISOR: f32 = 4.0;

/// The collapse chevron as three points in shares of its box's side.
const CHEVRON_RIGHT: [(f32, f32); 3] = [(0.36, 0.2), (0.66, 0.5), (0.36, 0.8)];
const CHEVRON_DOWN: [(f32, f32); 3] = [(0.2, 0.36), (0.5, 0.66), (0.8, 0.36)];

/// The expand arrows as strokes in shares of their box's side: a chevron
/// pointing the way the lines come from and a bar on the side of the hunk,
/// or for the whole gap a chevron each way.
const EXPAND_UP: [&[(f32, f32)]; 2] = [
    &[(0.2, 0.56), (0.5, 0.26), (0.8, 0.56)],
    &[(0.2, 0.82), (0.8, 0.82)],
];
const EXPAND_DOWN: [&[(f32, f32)]; 2] = [
    &[(0.2, 0.44), (0.5, 0.74), (0.8, 0.44)],
    &[(0.2, 0.18), (0.8, 0.18)],
];
const EXPAND_ALL: [&[(f32, f32)]; 2] = [
    &[(0.22, 0.4), (0.5, 0.14), (0.78, 0.4)],
    &[(0.22, 0.6), (0.5, 0.86), (0.78, 0.6)],
];
/// The arrows' box as a share of the row's height.
const EXPAND_ICON: f32 = 0.6;

/// Which way to move among a sorted list of rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Next,
    Prev,
}

/// What the last frame did, for benchmarks.
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameStats {
    /// Rows laid out and painted, overscan included.
    pub rows: usize,
    /// Lines shaped because they were not in the cache.
    pub shaped: usize,
    /// Block elements laid out and painted.
    pub blocks: usize,
    /// Blocks whose measured height differed from the height index.
    pub resized: usize,
}

/// A block element laid out this frame, waiting for its paint.
struct LaidBlock {
    id: BlockId,
    element: AnyElement,
    /// Left edge, relative to the view.
    x: Pixels,
}

/// A file header's controls laid out this frame, waiting for their paint.
struct LaidHeader {
    file: FileIx,
    pinned: bool,
    element: AnyElement,
}

/// A block's place across the row.
#[derive(Clone, Copy)]
struct BlockFrame {
    /// Left edge and width, relative to the view.
    x: Pixels,
    width: Pixels,
    /// Where its rail runs.
    rail_x: Pixels,
}

#[derive(Clone, Copy)]
struct VisibleRow {
    row: usize,
    /// Relative to the element's top.
    top: Pixels,
    height: Pixels,
}

/// Column widths for the current font.
#[derive(Clone, Copy)]
struct Columns {
    ch: Pixels,
    number: Pixels,
    marker: Pixels,
    /// Usable width, excluding the scrollbar.
    width: Pixels,
    /// Where split layout's new pane starts, which is where its divider is.
    split: Pixels,
}

impl Columns {
    /// The line-number gutter of a pane: both columns in unified, one per
    /// half in split.
    fn gutter(&self, layout: Layout) -> Pixels {
        match layout {
            Layout::Unified => self.number * 2.0,
            Layout::Split => self.number,
        }
    }

    fn pane_left(&self, layout: Layout, side: Side) -> Pixels {
        match (layout, side) {
            (Layout::Split, Side::New) => self.split,
            _ => px(0.0),
        }
    }

    /// Where the code of `side` starts, after the gutter and the marker.
    fn text_left(&self, layout: Layout, side: Side) -> Pixels {
        self.pane_left(layout, side) + self.gutter(layout) + self.marker
    }
}

/// What a row shows, to find the same row after a layout switch.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowTarget {
    File(FileIx),
    Hunk(FileIx, u32),
    Expander(FileIx),
    Line(FileIx, Side, u32),
    Block(BlockId),
}

impl RowTarget {
    fn of(row: Row) -> Self {
        match row {
            Row::FileHeader { file } => Self::File(file),
            Row::HunkHeader { file, hunk } => Self::Hunk(file, hunk),
            Row::Expander { file } => Self::Expander(file),
            Row::Line {
                file,
                new: Some(line),
                ..
            }
            | Row::Pair {
                file,
                right: Some(SideLine { line, .. }),
                ..
            } => Self::Line(file, Side::New, line),
            Row::Line { file, old, .. } => Self::Line(file, Side::Old, old.unwrap_or(0)),
            Row::Pair { file, left, .. } => Self::Line(file, Side::Old, left.map_or(0, |s| s.line)),
            Row::Block { id } => Self::Block(id),
        }
    }

    /// The file the target is in; blocks are not tracked by file.
    fn file(self) -> Option<FileIx> {
        match self {
            Self::File(file)
            | Self::Hunk(file, _)
            | Self::Expander(file)
            | Self::Line(file, ..) => Some(file),
            Self::Block(_) => None,
        }
    }

    fn shown_by(self, row: Row) -> bool {
        match (self, row) {
            (Self::Line(f, side, line), Row::Line { file, old, new }) => {
                f == file && line_on(side, old, new) == Some(line)
            }
            (Self::Line(f, side, line), Row::Pair { file, left, right }) => {
                f == file
                    && line_on(side, left.map(|s| s.line), right.map(|s| s.line)) == Some(line)
            }
            (Self::Line(..), _) => false,
            (target, row) => target == Self::of(row),
        }
    }
}

/// The blob a code line of `side` lives in.
fn blob(diff: &Diff, file: FileIx, side: Side) -> &Text {
    match side {
        Side::Old => &diff.file(file).old,
        Side::New => &diff.file(file).new,
    }
}

/// The blobs whose lines `row` paints.
fn row_blobs(row: Row) -> impl Iterator<Item = (FileIx, Side)> {
    let (first, second) = match row {
        Row::Line {
            file, new: Some(_), ..
        } => (Some((file, Side::New)), None),
        Row::Line {
            file, old: Some(_), ..
        } => (Some((file, Side::Old)), None),
        Row::Pair { file, left, right } => (
            left.map(|_| (file, Side::Old)),
            right.map(|_| (file, Side::New)),
        ),
        _ => (None, None),
    };
    first.into_iter().chain(second)
}

fn file_languages(diff: &Diff, language: Option<Language>) -> Vec<Option<Language>> {
    diff.files
        .iter()
        .map(|file| language.or_else(|| Language::detect(&file.path)))
        .collect()
}

/// The line a code row shows on `side`.
fn row_line(row: Row, side: Side) -> Option<u32> {
    match row {
        Row::Line { old, new, .. } => line_on(side, old, new),
        Row::Pair { left, right, .. } => line_on(side, left.map(|s| s.line), right.map(|s| s.line)),
        _ => None,
    }
}

fn line_on(side: Side, old: Option<u32>, new: Option<u32>) -> Option<u32> {
    match side {
        Side::Old => old,
        Side::New => new,
    }
}

/// A scrollable diff: one continuous, virtualized list across all files.
///
/// Render it as a child; it fills its parent. Scrolls with the mouse wheel
/// and trackpad, a draggable scrollbar and middle-click autoscroll.
pub struct DiffView {
    list: DiffList,
    style: DiffStyle,
    font: Font,
    scroll: Scroller,
    number_digits: usize,
    cache: LineCache,
    /// Each file's paired changed lines, for word emphasis.
    pairs: Vec<Pairs>,
    /// Each file's added and removed lines, for its header.
    counts: Vec<(u32, u32)>,
    words: WordCache,
    visible: Vec<VisibleRow>,
    columns: Option<Columns>,
    stats: FrameStats,
    highlighter: Highlighter,
    /// Each file's language, from its path unless `set_language` overrides it.
    file_languages: Vec<Option<Language>>,
    /// Highlights of the blobs painted in the last frame.
    slots: HashMap<(FileIx, Side), Slot>,
    /// What was last asked of the highlighter, to ask again only on change.
    wanted: Vec<(BlobId, Language)>,
    frame: u64,
    /// By file: the app's viewed marks.
    viewed: Vec<bool>,
    /// Files that collapse when they become viewed.
    collapse_viewed: bool,
    /// By file: the bodies folded away, leaving the header. View-local.
    collapsed: Vec<bool>,
    /// The left pane's share of the width in split layout.
    split_ratio: f32,
    divider_drag: Option<divider::DividerDrag>,
    /// Keeps the wake-up listener alive.
    listener: Option<Task<()>>,
    render_block: Option<RenderBlock>,
    render_header: Option<RenderHeader>,
    header_slots: HeaderSlots,
    /// The inline headers' controls, then the pinned one's.
    headers: Vec<LaidHeader>,
    /// By block id.
    rails: Vec<Option<Rail>>,
    lanes: Vec<u8>,
    laid: Vec<LaidBlock>,
    resized: usize,
    /// The selected text, if any; see [`DiffView::press_text`].
    selection: Option<Selection>,
    /// The mouse-driven selection in progress.
    text_drag: Option<selecting::TextDrag>,
    /// Escape and copy keys, watched from the first selection on.
    keys: Option<Subscription>,
    /// The expand arrow under the pointer.
    hovered_arrow: Option<ExpandArrow>,
    /// The matches of a find.
    marks: find::Marks,
}

/// Where one blob's highlights stand.
struct Slot {
    state: SlotState,
    seen: u64,
}

enum SlotState {
    Ready(Arc<Highlights>),
    /// Asked for, not yet cached; painted plain meanwhile.
    Pending,
    /// No language or nothing to color.
    Plain,
}

impl DiffView {
    #[must_use]
    pub fn new(diff: Diff, layout: Layout, style: DiffStyle) -> Self {
        let heights = RowHeights {
            code: style.line_height.into(),
            file_header: style.file_header_height.into(),
        };
        let max_lines = diff
            .files
            .iter()
            .map(|f| f.old.line_count().max(f.new.line_count()))
            .max()
            .unwrap_or(0);
        let files = diff.files.len();
        let file_languages = file_languages(&diff, None);
        let pairs = diff.files.iter().map(Pairs::of).collect();
        let counts = diff.files.iter().map(line_counts).collect();
        let rails = vec![None; diff.blocks.len()];
        Self {
            list: DiffList::new(diff, layout, heights),
            font: font(style.font_family.clone()),
            scroll: Scroller::new(style.scroll_style()),
            style,
            number_digits: max_lines.max(1).ilog10() as usize + 1,
            cache: LineCache::default(),
            pairs,
            counts,
            words: WordCache::default(),
            visible: Vec::new(),
            columns: None,
            stats: FrameStats::default(),
            highlighter: Highlighter::with_default_workers(),
            file_languages,
            slots: HashMap::new(),
            wanted: Vec::new(),
            viewed: vec![false; files],
            collapse_viewed: false,
            collapsed: vec![false; files],
            split_ratio: 0.5,
            divider_drag: None,
            frame: 0,
            listener: None,
            lanes: vec![0; rails.len()],
            rails,
            render_block: None,
            render_header: None,
            header_slots: HeaderSlots::default(),
            headers: Vec::new(),
            laid: Vec::new(),
            resized: 0,
            selection: None,
            text_drag: None,
            keys: None,
            hovered_arrow: None,
            marks: find::Marks::default(),
        }
    }

    /// Renders each block in view with `render` instead of an empty card.
    /// The heights the blocks came with are estimates: each block is laid out
    /// at its width while it is in view, and when its height differs the
    /// height index is corrected with the row at the top of the view held in
    /// place.
    pub fn set_block_renderer(&mut self, render: RenderBlock) {
        self.render_block = Some(render);
    }

    /// Replaces the blocks and their rails (by block id, `None` for no
    /// rail), keeping the line at the top of the view in place.
    pub fn set_blocks(&mut self, blocks: Vec<Block>, rails: Vec<Option<Rail>>) {
        let top = self.top_line_target();
        self.list.set_blocks(blocks);
        self.rebase_selection();
        self.set_rails(rails);
        if let Some((target, offset)) = top {
            self.restore_target(target, offset);
        }
    }

    /// Changes the rails without moving any rows.
    pub fn set_rails(&mut self, mut rails: Vec<Option<Rail>>) {
        rails.resize(self.list.diff().blocks.len(), None);
        self.rails = rails;
        self.update_lanes();
    }

    fn update_lanes(&mut self) {
        let spans: Vec<_> = (0..self.rails.len())
            .map(|ix| {
                let id = BlockId(ix as u32);
                let span = self.list.block_span(id);
                let side = self.list.diff().block(id).anchor.side()?;
                (self.rails[ix].is_some() && !span.is_empty()).then_some((span, side))
            })
            .collect();
        self.lanes = assign_lanes(&spans);
    }

    /// What the top of the view shows, skipping blocks (their ids may not
    /// survive a change of blocks), and how far the view's top is from it.
    fn top_line_target(&self) -> Option<(RowTarget, f64)> {
        if self.list.is_empty() {
            return None;
        }
        let top = self.list.row_at(self.scroll_y()).row;
        let row = (top..self.list.len())
            .find(|&row| !matches!(self.list.rows()[row], Row::Block { .. }))?;
        let target = RowTarget::of(self.list.rows()[row]);
        Some((target, self.scroll_y() - self.list.top_of(row)))
    }

    fn restore_target(&mut self, target: RowTarget, offset: f64) {
        let (row, exact) = self.target_row(target);
        let offset = if exact { offset } else { 0.0 };
        self.set_scroll_y(self.list.top_of(row) + offset);
    }

    /// The row showing `target`, and whether it is exactly that. For one that
    /// is no longer shown: the first row of its file at or after its line, or
    /// the file's header.
    fn target_row(&self, target: RowTarget) -> (usize, bool) {
        let rows = self.list.rows();
        if let Some(row) = rows.iter().position(|row| target.shown_by(*row)) {
            return (row, true);
        }
        let Some(header) = target.file().and_then(|file| self.list.file_row(file)) else {
            return (0, false);
        };
        if let RowTarget::Line(_, side, line) = target {
            let mut own = rows[header + 1..]
                .iter()
                .take_while(|row| !matches!(row, Row::FileHeader { .. }));
            if let Some(ix) = own.position(|row| row_line(*row, side).is_some_and(|at| at >= line))
            {
                return (header + 1 + ix, false);
            }
        }
        (header, false)
    }

    /// Scrolls to the next (or previous) of `rows`, which are sorted
    /// ascending: the first whose top, less `margin`, is below the view's
    /// top, or the last one above it. The row ends up `margin` below the
    /// top. Returns it, or `None` when there is none that way.
    pub fn step_to(
        &mut self,
        rows: impl IntoIterator<Item = usize>,
        step: Step,
        margin: f64,
    ) -> Option<usize> {
        let here = self.scroll_y();
        let list = &self.list;
        let target = |row: usize| (list.top_of(row) - margin).max(0.0);
        let mut rows = rows.into_iter();
        // Half a pixel of slack, so the row already at the top is skipped.
        let found = match step {
            Step::Next => rows.find(|&row| target(row) > here + 0.5),
            Step::Prev => rows.take_while(|&row| target(row) < here - 0.5).last(),
        }?;
        let y = target(found);
        self.set_scroll_y(y);
        Some(found)
    }

    /// Scrolls so `row` sits `margin` below the top of the view.
    pub fn scroll_to_row(&mut self, row: usize, margin: f64) -> bool {
        self.set_scroll_y(self.list.top_of(row) - margin)
    }

    /// The rows the viewport of the last frame shows, at least in part.
    pub fn rows_in_view(&self) -> Range<usize> {
        if self.list.is_empty() {
            return 0..0;
        }
        let top = self.scroll_y();
        let first = self.list.row_at(top).row;
        let last = self
            .list
            .row_at(top + (self.scroll.state.y.viewport - 1.0).max(0.0))
            .row;
        first..last + 1
    }

    /// Applies a new style live. Shaped lines are dropped (their colors and
    /// font are baked in) and shaped again on the next frame; the scroll
    /// position, rows, highlights and word diffs are kept. When the line
    /// height changes, the row at the top of the view stays there.
    pub fn set_style(&mut self, style: DiffStyle, cx: &mut Context<Self>) {
        if self.apply_style(style) {
            cx.notify();
        }
    }

    /// [`DiffView::set_style`] without the repaint; whether the style changed.
    /// The selection is kept: it is in source lines and chars.
    fn apply_style(&mut self, style: DiffStyle) -> bool {
        if style == self.style {
            return false;
        }
        let heights = RowHeights {
            code: style.line_height.into(),
            file_header: style.file_header_height.into(),
        };
        if heights != self.list.heights() {
            let top = (!self.list.is_empty()).then(|| {
                let at = self.list.row_at(self.scroll_y());
                (at.row, at.offset, self.list.row_height(at.row))
            });
            self.list.set_heights(heights);
            if let Some((row, offset, old_height)) = top {
                let scale = if old_height > 0.0 {
                    self.list.row_height(row) / old_height
                } else {
                    1.0
                };
                self.set_scroll_y(self.list.top_of(row) + offset * scale);
            }
        }
        self.font = font(style.font_family.clone());
        self.scroll.set_style(style.scroll_style());
        self.style = style;
        self.columns = None;
        self.cache.clear();
        true
    }

    /// Colors every file as `language` instead of by its path; `None` goes
    /// back to the path. For synthetic text, whose paths say nothing.
    pub fn set_language(&mut self, language: Option<Language>) {
        self.file_languages = file_languages(self.list.diff(), language);
        self.slots.clear();
        self.wanted.clear();
        self.cache.clear();
    }

    /// Uses `highlighter` (and its cache) instead of this view's own, so
    /// several views can share one pool.
    pub fn set_highlighter(&mut self, highlighter: Highlighter) {
        self.highlighter = highlighter;
        self.listener = None;
        self.slots.clear();
        self.wanted.clear();
        self.cache.clear();
    }

    /// Queues every blob for highlighting at low priority, behind whatever
    /// is on screen. Not needed for normal use; benchmarks use it to measure
    /// scrolling with every row colored.
    pub fn prefetch_highlights(&self) {
        let diff = self.list.diff();
        let blobs = diff.files.iter().enumerate().flat_map(|(ix, file)| {
            let language = self.file_languages[ix];
            [&file.old, &file.new]
                .into_iter()
                .filter_map(move |text| Some((text.clone(), language?)))
        });
        self.highlighter.prefetch(blobs);
    }

    /// True when no highlights are queued, running or waiting to be picked up.
    pub fn highlights_settled(&self) -> bool {
        self.highlighter.is_idle()
            && self
                .slots
                .values()
                .all(|slot| !matches!(slot.state, SlotState::Pending))
    }

    pub fn list(&self) -> &DiffList {
        &self.list
    }

    /// Switches layout, keeping the row at the top of the view in place.
    pub fn set_layout(&mut self, layout: Layout) {
        if layout == self.list.layout() || self.list.is_empty() {
            return;
        }
        let top = self.list.row_at(self.scroll_y());
        let target = RowTarget::of(self.list.rows()[top.row]);
        self.clear_selection();
        self.list.set_layout(layout);
        self.update_lanes();
        let row = self
            .list
            .rows()
            .iter()
            .position(|row| target.shown_by(*row))
            .unwrap_or(0);
        self.set_scroll_y(self.list.top_of(row) + top.offset);
    }

    pub fn scroll_y(&self) -> f64 {
        self.scroll.state.y.offset
    }

    /// The largest scroll offset for the viewport of the last frame.
    pub fn max_scroll_y(&self) -> f64 {
        (self.list.total_height() - self.scroll.state.y.viewport).max(0.0)
    }

    /// Sets the scroll offset, clamped; returns whether it changed.
    pub fn set_scroll_y(&mut self, y: f64) -> bool {
        self.scroll.state.y.content = self.list.total_height();
        self.scroll.state.y.set(y)
    }

    pub fn scroll_by(&mut self, dy: f64) -> bool {
        self.set_scroll_y(self.scroll_y() + dy)
    }

    /// True while middle-click autoscroll is running.
    pub fn is_autoscrolling(&self) -> bool {
        self.scroll.is_autoscrolling()
    }

    /// Sets the hook that vetoes middle-click autoscroll at a point, for a
    /// view that embeds a text input: middle click pastes there.
    pub fn set_autoscroll_filter(
        &mut self,
        filter: impl Fn(Point<Pixels>, &Window, &App) -> bool + 'static,
    ) {
        self.scroll.set_start_filter(filter);
    }

    pub fn last_frame(&self) -> FrameStats {
        self.stats
    }

    /// The code line under `point` (relative to the view's top left) as of
    /// the last frame: its row, the side it is on and its 0-based line.
    /// Unified rows name their new line, or the old one for a removed line;
    /// in split, each half names its own side. `None` over headers, blocks,
    /// an empty half, past the content, and before the first frame.
    #[must_use]
    pub fn hit_test(&self, point: Point<Pixels>) -> Option<(usize, Side, u32)> {
        if self.pinned_covers(point) || self.on_divider(point) {
            return None;
        }
        hit_at(&self.list, self.scroll_y(), self.columns?, point)
    }

    /// Width of the line-number gutter at the left of each pane: both
    /// number columns in unified, one per half in split. `None` before the
    /// first frame.
    #[must_use]
    pub fn gutter_width(&self) -> Option<Pixels> {
        Some(self.columns?.gutter(self.list.layout()))
    }

    /// The left edge of the pane that shows `side`: 0, or the middle of the
    /// view for the new side in split. `None` before the first frame.
    #[must_use]
    pub fn pane_left(&self, side: Side) -> Option<Pixels> {
        Some(self.columns?.pane_left(self.list.layout(), side))
    }

    fn update_columns(&mut self, bounds: Bounds<Pixels>, window: &Window) -> Columns {
        let ch = self.columns.map_or_else(
            || {
                let text_system = window.text_system();
                let font_id = text_system.resolve_font(&self.font);
                text_system
                    .ch_advance(font_id, self.style.font_size)
                    .unwrap_or(self.style.font_size * 0.6)
            },
            |columns| columns.ch,
        );
        let width = (bounds.size.width - self.style.scrollbar.width).max(px(0.0));
        let (number, marker) = (ch * (self.number_digits as f32 + 2.0), ch * 2.0);
        let min_pane = divider::min_pane(width, number, marker, ch);
        let columns = Columns {
            ch,
            number,
            marker,
            width,
            split: divider::split_x(width, self.split_ratio, min_pane),
        };
        self.columns = Some(columns);
        columns
    }

    /// Starts the task that repaints when highlights land.
    fn listen(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = async_channel::bounded::<()>(1);
        // A full channel already has a wake-up pending, so a failed send is
        // fine; a closed one means the view is gone.
        self.highlighter.on_ready(move || {
            let _ = tx.try_send(());
            !tx.is_closed()
        });
        self.listener = Some(cx.spawn(async move |this, cx| {
            while rx.recv().await.is_ok() {
                let alive = this.update(cx, |view, cx| {
                    view.poll_pending();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        }));
    }

    /// Picks up highlights that landed for blobs painted plain so far.
    fn poll_pending(&mut self) {
        let diff = self.list.diff();
        for (&(file, side), slot) in &mut self.slots {
            if matches!(slot.state, SlotState::Pending)
                && let Some(language) = self.file_languages[file.0 as usize]
                && let Some(done) = self.highlighter.get(blob(diff, file, side).id(), language)
            {
                slot.state = SlotState::Ready(done);
            }
        }
    }

    /// Looks up the highlights of what is visible and asks the highlighter
    /// for what is missing, so the next frames paint it colored.
    fn sync_highlights(&mut self) {
        self.frame += 1;
        let frame = self.frame;
        let diff = self.list.diff();
        let mut wanted: Vec<(Text, Language)> = Vec::new();
        for visible in &self.visible {
            for (file, side) in row_blobs(self.list.rows()[visible.row]) {
                let slot = self.slots.entry((file, side)).or_insert_with(|| Slot {
                    state: SlotState::Plain,
                    seen: 0,
                });
                if slot.seen == frame {
                    continue;
                }
                let language = self.file_languages[file.0 as usize];
                if slot.seen == 0 {
                    slot.state = match language {
                        None => SlotState::Plain,
                        Some(language) => self
                            .highlighter
                            .get(blob(diff, file, side).id(), language)
                            .map_or(SlotState::Pending, SlotState::Ready),
                    };
                }
                slot.seen = frame;
                if matches!(slot.state, SlotState::Pending)
                    && let Some(language) = language
                {
                    wanted.push((blob(diff, file, side).clone(), language));
                }
            }
        }
        self.slots.retain(|_, slot| slot.seen == frame);
        let keys: Vec<_> = wanted
            .iter()
            .map(|(text, language)| (text.id(), *language))
            .collect();
        if keys != self.wanted {
            self.highlighter.set_visible(&wanted);
            self.wanted = keys;
            // Oversize blobs are answered plain at once.
            self.poll_pending();
        }
    }

    fn prepaint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        if self.listener.is_none() {
            self.listen(cx);
        }
        self.scroll.state.y.content = self.list.total_height();
        self.scroll.frame(bounds, window);
        if self.edge_scroll(now()) {
            window.request_animation_frame();
        }
        let columns = self.update_columns(bounds, window);
        self.visible.clear();
        self.laid.clear();
        self.resized = 0;
        self.cache.begin_frame();
        if !self.list.is_empty() {
            self.layout_blocks(columns, window, cx);
            self.prepaint_blocks(bounds, window, cx);
            self.prepaint_headers(bounds, window, cx);
            self.sync_highlights();
            let pinned = self.pinned().map(|pinned| pinned.file);
            let mut shaper = Shaper {
                code_columns: code_columns(self.list.layout(), columns),
                viewed: &self.viewed,
                counts: (self.header_slots.stats > px(0.0)).then_some(self.counts.as_slice()),
                slots: &self.slots,
                cache: &mut self.cache,
                list: &self.list,
                diff: self.list.diff(),
                pairs: &self.pairs,
                words: &mut self.words,
                style: &self.style,
                shape: ShapeStyle {
                    font: &self.font,
                    font_size: self.style.font_size,
                    tab_width: self.style.tab_width,
                },
                window,
            };
            shaper.digits();
            shaper.viewed_label();
            for visible in &self.visible {
                shaper.row(self.list.rows()[visible.row]);
            }
            if let Some(file) = pinned {
                shaper.file_header(file);
            }
        }
        self.cache.end_frame();
        self.stats = FrameStats {
            rows: self.visible.len(),
            shaped: self.cache.shaped_this_frame(),
            blocks: self.laid.len(),
            resized: self.resized,
        };
    }

    /// Where a block sits across the row: right of the gutter, on its own
    /// side in split view.
    fn block_frame(&self, id: BlockId, columns: Columns) -> BlockFrame {
        let number = columns.number;
        let half = columns.split;
        let (x, right) = match (self.list.layout(), self.list.diff().block(id).anchor.side()) {
            (Layout::Unified, _) => (number * 2.0, columns.width),
            (Layout::Split, Some(Side::Old)) => (number, half),
            (Layout::Split, Some(Side::New)) => (half + number, columns.width),
            (Layout::Split, None) => (number, columns.width),
        };
        BlockFrame {
            x,
            width: (right - x).max(px(0.0)),
            rail_x: x - columns.ch * 0.25,
        }
    }

    /// Collects the visible rows and lays out the blocks among them. A block
    /// whose height differs from the index gets its new height, with the row
    /// at the top of the view held in place, and the rows are collected
    /// again; blocks already laid out keep their element.
    fn layout_blocks(&mut self, columns: Columns, window: &mut Window, cx: &mut App) {
        for _ in 0..BLOCK_PASSES {
            self.visible.clear();
            self.collect_visible();
            let Some(render) = self.render_block.clone() else {
                return;
            };
            let mut changed = Vec::new();
            for ix in 0..self.visible.len() {
                let Row::Block { id } = self.list.rows()[self.visible[ix].row] else {
                    continue;
                };
                if self.laid.iter().any(|laid| laid.id == id) {
                    continue;
                }
                let frame = self.block_frame(id, columns);
                let available = size(
                    AvailableSpace::Definite(frame.width),
                    AvailableSpace::MinContent,
                );
                let (element, measured) = window.with_element_namespace(block_key(id), |window| {
                    let mut element = render(id, frame.width, window, cx);
                    let measured = element.layout_as_root(available, window, cx);
                    (element, measured)
                });
                let height = f32::from(measured.height.ceil());
                if (height - self.list.diff().block(id).height).abs() >= 0.5 {
                    changed.push((id, height));
                }
                self.laid.push(LaidBlock {
                    id,
                    element,
                    x: frame.x,
                });
            }
            if changed.is_empty() {
                break;
            }
            self.resized += changed.len();
            self.resize_blocks(&changed);
        }
        let visible = &self.visible;
        let rows = self.list.rows();
        self.laid.retain(|laid| {
            visible
                .iter()
                .any(|v| rows[v.row] == Row::Block { id: laid.id })
        });
    }

    /// New block heights, keeping the row at the top of the view where it
    /// is: blocks above it move it by their change, which the scroll offset
    /// takes back.
    fn resize_blocks(&mut self, heights: &[(BlockId, f32)]) {
        let anchor = self.list.row_at(self.scroll_y());
        for &(id, height) in heights {
            self.list.set_block_height(id, height);
        }
        let offset = anchor.offset.min(self.list.row_height(anchor.row));
        self.set_scroll_y(self.list.top_of(anchor.row) + offset);
    }

    fn prepaint_blocks(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        let mask = ContentMask { bounds };
        for laid in &mut self.laid {
            let row = self.list.block_row(laid.id);
            let Some(visible) = self.visible.iter().find(|v| v.row == row) else {
                continue;
            };
            let origin = bounds.origin + point(laid.x, visible.top);
            window.with_content_mask(Some(mask), |window| {
                window.with_element_namespace(block_key(laid.id), |window| {
                    laid.element.prepaint_at(origin, window, cx);
                });
            });
        }
    }

    /// Lays out and prepaints the controls of each file header in view.
    fn prepaint_headers(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        self.headers.clear();
        let Some(render) = self.render_header.clone() else {
            return;
        };
        let viewport = bounds.size.height;
        for ix in 0..self.visible.len() {
            let visible = self.visible[ix];
            let Row::FileHeader { file } = self.list.rows()[visible.row] else {
                continue;
            };
            if visible.top + visible.height <= px(0.0) || visible.top >= viewport {
                continue;
            }
            let origin = bounds.origin + point(px(0.0), visible.top);
            if let Some(laid) = self.lay_header(&render, file, false, origin, bounds, window, cx) {
                self.headers.push(laid);
            }
        }
    }

    /// The pinned header's controls. Laid out after the pinned header's own
    /// hitbox, so they sit over it while it hides the rows under it.
    fn prepaint_pinned_header(
        &mut self,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let (Some(render), Some(pinned), Some(clip)) = (
            self.render_header.clone(),
            self.pinned(),
            self.pinned_bounds(bounds),
        ) else {
            return;
        };
        let origin = bounds.origin + point(px(0.0), px(pinned.top as f32));
        if let Some(laid) = self.lay_header(&render, pinned.file, true, origin, clip, window, cx) {
            self.headers.push(laid);
        }
    }

    #[expect(clippy::too_many_arguments, reason = "one prepaint step")]
    fn lay_header(
        &self,
        render: &RenderHeader,
        file: FileIx,
        pinned: bool,
        origin: Point<Pixels>,
        clip: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<LaidHeader> {
        let place = self.header_place(file, pinned)?;
        let available = size(
            AvailableSpace::Definite(place.size.width),
            AvailableSpace::Definite(place.size.height),
        );
        let element = window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
            window.with_element_namespace(header_key(file, pinned), |window| {
                let mut element = render(place, window, cx);
                element.layout_as_root(available, window, cx);
                element.prepaint_at(origin, window, cx);
                element
            })
        });
        Some(LaidHeader {
            file,
            pinned,
            element,
        })
    }

    fn collect_visible(&mut self) {
        let (scroll_y, viewport) = (self.scroll_y(), self.scroll.state.y.viewport);
        let first = self.list.row_at(scroll_y).row.saturating_sub(OVERSCAN);
        let mut top = self.list.top_of(first) - scroll_y;
        let mut below = 0;
        for row in first..self.list.len() {
            let height = self.list.row_height(row);
            self.visible.push(VisibleRow {
                row,
                top: px(top as f32),
                height: px(height as f32),
            });
            top += height;
            if top >= viewport {
                below += 1;
                if below > OVERSCAN {
                    break;
                }
            }
        }
    }

    fn paint(&mut self, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
        window.paint_quad(fill(bounds, self.style.background));
        let Some(columns) = self.columns else { return };
        let painter = RowPainter {
            view: self,
            bounds,
            columns,
        };
        for visible in &self.visible {
            painter.row(*visible, window, cx);
        }
        painter.rails(window);
        for laid in &mut self.laid {
            window.with_element_namespace(block_key(laid.id), |window| {
                laid.element.paint(window, cx);
            });
        }
        self.laid.clear();
        let mut headers = std::mem::take(&mut self.headers);
        let paint_header = |laid: &mut LaidHeader, window: &mut Window, cx: &mut App| {
            window.with_element_namespace(header_key(laid.file, laid.pinned), |window| {
                laid.element.paint(window, cx);
            });
        };
        for laid in headers.iter_mut().filter(|laid| !laid.pinned) {
            paint_header(laid, window, cx);
        }
        let painter = RowPainter {
            view: self,
            bounds,
            columns,
        };
        if let Some(pinned) = self.pinned() {
            painter.pinned_header(pinned, window, cx);
        }
        for laid in headers.iter_mut().filter(|laid| laid.pinned) {
            paint_header(laid, window, cx);
        }
        self.scroll.paint(window);
    }
}

/// See [`DiffView::hit_test`].
fn hit_at(
    list: &DiffList,
    scroll_y: f64,
    columns: Columns,
    point: Point<Pixels>,
) -> Option<(usize, Side, u32)> {
    let (x, y) = (f32::from(point.x), f32::from(point.y));
    if list.is_empty() || x < 0.0 || y < 0.0 || x >= f32::from(columns.width) {
        return None;
    }
    let at = scroll_y + f64::from(y);
    if at >= list.total_height() {
        return None;
    }
    let row = list.row_at(at).row;
    match list.rows()[row] {
        Row::Line {
            new: Some(line), ..
        } => Some((row, Side::New, line)),
        Row::Line {
            old: Some(line),
            new: None,
            ..
        } => Some((row, Side::Old, line)),
        Row::Pair { left, right, .. } => {
            if x < f32::from(columns.split) {
                Some((row, Side::Old, left?.line))
            } else {
                Some((row, Side::New, right?.line))
            }
        }
        _ => None,
    }
}

/// The element id namespace of a block, the same in every phase so its
/// elements keep their state from frame to frame.
fn block_key(id: BlockId) -> ElementId {
    ("diff-block", id.0 as usize).into()
}

/// The element id namespace of a file header's controls, one for the inline
/// header and one for its pinned copy.
fn header_key(file: FileIx, pinned: bool) -> ElementId {
    let name = if pinned {
        "diff-pinned-header"
    } else {
        "diff-header"
    };
    (name, file.0 as usize).into()
}

/// Word diffs of paired lines, by `(file, old line, new line)`.
#[derive(Default)]
struct WordCache {
    pairs: HashMap<(FileIx, u32, u32), Option<Arc<WordDiff>>>,
}

impl WordCache {
    /// The word diff of `line` of `side` against its partner line, computed
    /// on first use. `None` for a line without a partner or one too
    /// different from it.
    fn of(
        &mut self,
        diff: &Diff,
        pairs: &Pairs,
        file: FileIx,
        side: Side,
        line: u32,
    ) -> Option<Arc<WordDiff>> {
        let partner = pairs.partner(side, line)?;
        let (old, new) = match side {
            Side::Old => (line, partner),
            Side::New => (partner, line),
        };
        if self.pairs.len() >= WORD_CACHE_PAIRS {
            self.pairs.clear();
        }
        self.pairs
            .entry((file, old, new))
            .or_insert_with(|| {
                let file = diff.file(file);
                word_diff(&file.old.line(old), &file.new.line(new)).map(Arc::new)
            })
            .clone()
    }
}

/// How many columns of code to shape: what the widest code pane of `layout`
/// shows, see [`shaped_columns`].
fn code_columns(layout: Layout, columns: Columns) -> usize {
    let gutter = match layout {
        Layout::Unified => columns.number * 2.0,
        Layout::Split => columns.number,
    };
    let pane = columns.widest_pane(layout);
    let code = (pane - gutter - columns.marker).max(px(0.0));
    let visible = if columns.ch > px(0.0) {
        (code / columns.ch).ceil() as usize
    } else {
        usize::MAX
    };
    shaped_columns(visible)
}

/// Shapes what one row needs into the cache.
struct Shaper<'a> {
    /// See [`code_columns`].
    code_columns: usize,
    /// By file.
    viewed: &'a [bool],
    /// By file, when the headers show line counts.
    counts: Option<&'a [(u32, u32)]>,
    slots: &'a HashMap<(FileIx, Side), Slot>,
    cache: &'a mut LineCache,
    list: &'a DiffList,
    diff: &'a Diff,
    pairs: &'a [Pairs],
    words: &'a mut WordCache,
    style: &'a DiffStyle,
    shape: ShapeStyle<'a>,
    window: &'a Window,
}

impl Shaper<'_> {
    fn row(&mut self, row: Row) {
        match row {
            Row::FileHeader { file } => self.file_header(file),
            Row::HunkHeader { file, hunk } => self.hunk_header(file, hunk),
            Row::Expander { .. } => {}
            Row::Line { file, old, new } => {
                let kind = Row::line_kind(old, new);
                self.marker(kind);
                match (old, new) {
                    (_, Some(line)) => self.code(file, Side::New, line),
                    (Some(line), None) => self.code(file, Side::Old, line),
                    (None, None) => {}
                }
            }
            Row::Pair { file, left, right } => {
                for (side, side_line) in [(Side::Old, left), (Side::New, right)] {
                    if let Some(SideLine { line, kind }) = side_line {
                        self.marker(kind);
                        self.code(file, side, line);
                    }
                }
            }
            Row::Block { .. } => {}
        }
    }

    /// Text that is never cut: headers, digits and markers.
    fn plain(&mut self, key: LineKey, color: Hsla, write: impl FnOnce(&mut String)) {
        let font = self.shape.font;
        self.cache.ensure(
            key,
            Quality::Plain,
            0,
            &self.shape,
            self.window,
            |text, runs| {
                write(text);
                runs.push(run(text.len(), font, color, None));
                None
            },
        );
    }

    /// Line numbers are painted digit by digit, so no number is ever shaped
    /// after the first frame.
    fn digits(&mut self) {
        let color = self.style.gutter_text;
        for digit in 0..10u8 {
            self.plain(LineKey::Digit(digit), color, |text| {
                text.push(char::from(b'0' + digit));
            });
        }
    }

    fn marker(&mut self, kind: LineKind) {
        let color = self.style.gutter_text;
        let sign = match kind {
            LineKind::Context => return,
            LineKind::Added => "+",
            LineKind::Removed => "-",
        };
        self.plain(LineKey::Marker(kind), color, |text| text.push_str(sign));
    }

    fn code(&mut self, file: FileIx, side: Side, line: u32) {
        let text = blob(self.diff, file, side);
        let (quality, spans) = match self.slots.get(&(file, side)).map(|slot| &slot.state) {
            Some(SlotState::Ready(highlights)) => (Quality::Highlighted, highlights.line(line)),
            _ => (Quality::Plain, [].as_slice()),
        };
        let code_style = CodeStyle {
            font: self.shape.font,
            palette: &self.style.palette,
            tab_width: self.shape.tab_width,
            emphasis_background: match side {
                Side::Old => self.style.remove_emphasis,
                Side::New => self.style.add_emphasis,
            },
        };
        let (diff, pairs, words) = (self.diff, self.pairs, &mut *self.words);
        let columns = self.code_columns;
        self.cache.ensure(
            LineKey::Code { file, side, line },
            quality,
            columns,
            &self.shape,
            self.window,
            |out, runs| {
                let word_diff = words.of(diff, &pairs[file.0 as usize], file, side, line);
                let code = Code {
                    line: &text.line(line),
                    spans,
                    emphasis: word_diff.as_deref().map_or(&[], |w| w.side(side)),
                };
                push_code(&code, &code_style, columns, out, runs)
            },
        );
    }

    fn file_header(&mut self, file: FileIx) {
        let diff = self.diff;
        let path = &diff.file(file).path;
        let dim = self.viewed.get(file.0 as usize).copied().unwrap_or(false);
        let color = if dim {
            self.style.file_header_dim
        } else {
            self.style.file_header_text
        };
        self.plain(LineKey::FileHeader { file, dim }, color, |text| {
            text.push_str(path);
        });
        if let Some(&(added, removed)) = self.counts.and_then(|counts| counts.get(file.0 as usize))
        {
            self.stats(file, added, removed);
        }
    }

    /// `+38 -9`, each count in its own color.
    fn stats(&mut self, file: FileIx, added: u32, removed: u32) {
        let font = self.shape.font;
        let (plus, minus) = (self.style.stat_added, self.style.stat_removed);
        self.cache.ensure(
            LineKey::Stats(file),
            Quality::Plain,
            0,
            &self.shape,
            self.window,
            |text, runs| {
                let _ = write!(text, "+{added}");
                let first = text.len();
                runs.push(run(first, font, plus, None));
                let _ = write!(text, " -{removed}");
                runs.push(run(text.len() - first, font, minus, None));
                None
            },
        );
    }

    fn viewed_label(&mut self) {
        let color = self.style.file_header_text;
        self.plain(LineKey::ViewedLabel, color, |text| text.push_str("Viewed"));
    }

    /// `@@ -176,7 +197,9 @@`: the lines down to the next header, revealed
    /// context included.
    fn hunk_header(&mut self, file: FileIx, hunk: u32) {
        let span = self.list.header_span(file, hunk);
        let color = self.style.hunk_header_text;
        self.plain(LineKey::HunkHeader { file, span }, color, |text| {
            let _ = write!(
                text,
                "@@ -{},{} +{},{} @@",
                span.old_start + 1,
                span.old_len,
                span.new_start + 1,
                span.new_len
            );
        });
    }
}

/// A code row and the file it is in.
#[derive(Clone, Copy)]
struct Place {
    row: usize,
    file: FileIx,
}

/// What is painted under a line's glyphs, over its background.
#[derive(Default)]
struct Marked {
    selected: Option<Bounds<Pixels>>,
    /// Find matches, and whether each is the current one.
    found: Vec<(Bounds<Pixels>, bool)>,
}

/// Paints rows from the cache that prepaint filled.
struct RowPainter<'a> {
    view: &'a DiffView,
    bounds: Bounds<Pixels>,
    columns: Columns,
}

impl RowPainter<'_> {
    fn row(&self, visible: VisibleRow, window: &mut Window, cx: &mut App) {
        let style = &self.view.style;
        let origin = self.bounds.origin + point(px(0.0), visible.top);
        let full = Bounds::new(origin, size(self.columns.width, visible.height));
        let pad = self.columns.ch;
        match self.view.list.rows()[visible.row] {
            Row::FileHeader { file } => {
                self.file_header(file, origin, visible.height, false, window, cx);
            }
            row @ Row::HunkHeader { file, hunk } => {
                window.paint_quad(fill(full, style.hunk_header_background));
                self.arrows(row, origin, visible.height, window);
                let span = self.view.list.header_span(file, hunk);
                let gutter = self.columns.gutter(self.view.list.layout());
                self.text(
                    LineKey::HunkHeader { file, span },
                    origin + point(gutter + pad, px(0.0)),
                    visible.height,
                    window,
                    cx,
                );
            }
            row @ Row::Expander { .. } => {
                window.paint_quad(fill(full, style.hunk_header_background));
                self.arrows(row, origin, visible.height, window);
            }
            Row::Line { file, old, new } => {
                let kind = Row::line_kind(old, new);
                let (background, gutter) = self.kind_colors(kind);
                let number = self.columns.number;
                if let Some(background) = background {
                    window.paint_quad(fill(full, background));
                }
                window.paint_quad(fill(
                    Bounds::new(origin, size(number * 2.0, visible.height)),
                    gutter,
                ));
                for (ix, line) in [old, new].into_iter().enumerate() {
                    if let Some(line) = line {
                        self.number(
                            line,
                            origin.x + number * (ix as f32 + 1.0),
                            origin.y,
                            visible.height,
                            window,
                            cx,
                        );
                    }
                }
                let marker_x = origin.x + number * 2.0;
                self.text(
                    LineKey::Marker(kind),
                    point(marker_x + pad * 0.5, origin.y),
                    visible.height,
                    window,
                    cx,
                );
                let (side, line) = match (old, new) {
                    (_, Some(line)) => (Side::New, line),
                    (Some(line), None) => (Side::Old, line),
                    (None, None) => return,
                };
                let text_origin = point(marker_x + self.columns.marker, origin.y);
                let at = CodeLine { file, side, line };
                let selected =
                    self.selected(visible.row, at, text_origin, visible.height, full.right());
                let found = self.found(at, text_origin, visible.height, full.right());
                self.code(
                    LineKey::Code { file, side, line },
                    text_origin,
                    visible.height,
                    Marked { selected, found },
                    window,
                    cx,
                );
            }
            Row::Pair { file, left, right } => {
                let half = self.columns.split;
                let sides = [
                    (
                        Side::Old,
                        left,
                        Bounds::new(origin, size(half, visible.height)),
                    ),
                    (
                        Side::New,
                        right,
                        Bounds::new(
                            origin + point(half, px(0.0)),
                            size(self.columns.width - half, visible.height),
                        ),
                    ),
                ];
                let place = Place {
                    row: visible.row,
                    file,
                };
                for (side, side_line, side_bounds) in sides {
                    self.split_side(place, side, side_line, side_bounds, window, cx);
                }
                self.divider(origin, visible.height, window);
            }
            Row::Block { .. } if self.view.render_block.is_some() => {
                if self.view.list.layout() == Layout::Split {
                    self.divider(origin, visible.height, window);
                }
            }
            Row::Block { .. } => {
                let inset = match self.view.list.layout() {
                    Layout::Unified => self.columns.number * 2.0,
                    Layout::Split => self.columns.number,
                };
                let card = Bounds::new(
                    origin + point(inset, px(4.0)),
                    size(
                        (full.size.width - inset - pad).max(px(0.0)),
                        (visible.height - px(8.0)).max(px(0.0)),
                    ),
                );
                window.paint_quad(quad(
                    card,
                    px(6.0),
                    style.block_background,
                    px(1.0),
                    style.block_border,
                    BorderStyle::Solid,
                ));
            }
        }
    }

    /// The divider between the panes, for one row at `origin`.
    fn divider(&self, origin: Point<Pixels>, height: Pixels, window: &mut Window) {
        let style = &self.view.style;
        let color = if self.view.is_resizing() {
            style.divider_active
        } else {
            style.divider
        };
        let line = Bounds::new(
            origin + point(self.columns.split, px(0.0)),
            size(px(1.0), height),
        );
        window.paint_quad(fill(line, color));
    }

    /// A file header at `origin`: the path, dimmed when viewed, and the
    /// viewed checkbox at the right. Pinned, it takes its hairline at the
    /// bottom, where it meets the code, instead of the top.
    fn file_header(
        &self,
        file: FileIx,
        origin: Point<Pixels>,
        height: Pixels,
        pinned: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        let style = &self.view.style;
        let viewed = self.view.is_viewed(file);
        let width = self.columns.width;
        window.paint_quad(fill(
            Bounds::new(origin, size(width, height)),
            style.file_header_background,
        ));
        let edge_y = if pinned { height - px(1.0) } else { px(0.0) };
        window.paint_quad(fill(
            Bounds::new(origin + point(px(0.0), edge_y), size(width, px(1.0))),
            style.divider,
        ));
        let zone = self.view.viewed_zone(self.columns);
        let clip = ContentMask {
            bounds: Bounds::new(origin, size(self.view.path_clip(self.columns), height)),
        };
        window.with_content_mask(Some(clip), |window| {
            let key = LineKey::FileHeader { file, dim: viewed };
            let at = origin + point(path_left(self.columns, height), px(0.0));
            let path = &self.view.list.diff().file(file).path;
            let right = clip.bounds.right();
            let found = self.found_in(path, (file, FindAt::Path), at, height, right);
            let marked = Marked {
                selected: None,
                found,
            };
            self.code(key, at, height, marked, window, cx);
        });
        self.chevron(file, origin, height, viewed, window);
        let checkbox = Bounds::new(origin + zone.checkbox.origin, zone.checkbox.size);
        let side = checkbox.size.width;
        let radius = side / CHECKBOX_RADIUS_DIVISOR;
        if viewed {
            window.paint_quad(quad(
                checkbox,
                radius,
                style.checkbox_checked,
                px(1.0),
                style.checkbox_checked,
                BorderStyle::Solid,
            ));
            let at = |(x, y): (f32, f32)| checkbox.origin + point(side * x, side * y);
            let mut tick = PathBuilder::stroke((side * TICK_WIDTH).max(px(1.0)));
            tick.move_to(at(TICK[0]));
            tick.line_to(at(TICK[1]));
            tick.line_to(at(TICK[2]));
            if let Ok(path) = tick.build() {
                window.paint_path(path, style.checkbox_mark);
            }
        } else {
            window.paint_quad(quad(
                checkbox,
                radius,
                gpui::transparent_black(),
                px(1.0),
                style.checkbox_border,
                BorderStyle::Solid,
            ));
        }
        let label = origin + point(zone.label_x, px(0.0));
        self.text(LineKey::ViewedLabel, label, height, window, cx);
        if self.view.header_slots.stats > px(0.0) {
            self.stats(file, origin + point(zone.left, px(0.0)), height, window, cx);
        }
    }

    /// The line counts and change bar of a header, ending at `right` (the
    /// left of the viewed zone, at the header's top).
    fn stats(
        &self,
        file: FileIx,
        right: Point<Pixels>,
        height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let style = &self.view.style;
        let ch = self.columns.ch;
        let side = (ch * BAR_BLOCK).round();
        let gap = (ch * BAR_GAP).round().max(px(1.0));
        let bar_width = side * BAR_BLOCKS as f32 + gap * (BAR_BLOCKS - 1) as f32;
        let bar_left = right.x - ch * 0.5 - bar_width;
        let top = right.y + ((height - side) / 2.0).round();
        let (added, removed) = self
            .view
            .counts
            .get(file.0 as usize)
            .copied()
            .unwrap_or((0, 0));
        for (ix, block) in change_bar(added, removed).into_iter().enumerate() {
            let color = match block {
                BarBlock::Added => style.stat_added,
                BarBlock::Removed => style.stat_removed,
                BarBlock::Unchanged => style.stat_empty,
            };
            let at = point(bar_left + (side + gap) * ix as f32, top);
            window.paint_quad(quad(
                Bounds::new(at, size(side, side)),
                side / BAR_RADIUS_DIVISOR,
                color,
                px(0.0),
                color,
                BorderStyle::Solid,
            ));
        }
        if let Some((shaped, _)) = self.view.cache.get(LineKey::Stats(file)) {
            let x = bar_left - ch - shaped.width();
            let _ = shaped.paint(point(x, right.y), height, TextAlign::Left, None, window, cx);
        }
    }

    /// The chevron of a file header: right when the body is folded away,
    /// down when it is shown.
    fn chevron(
        &self,
        file: FileIx,
        origin: Point<Pixels>,
        height: Pixels,
        dim: bool,
        window: &mut Window,
    ) {
        let style = &self.view.style;
        let zone = chevron_zone(self.columns, height);
        let side = zone.size.width;
        let corner = zone.origin + origin;
        let at = |(x, y): (f32, f32)| corner + point(side * x, side * y);
        let points = if self.view.is_file_collapsed(file) {
            CHEVRON_RIGHT
        } else {
            CHEVRON_DOWN
        };
        let mut stroke = PathBuilder::stroke((side * TICK_WIDTH).max(px(1.0)));
        stroke.move_to(at(points[0]));
        stroke.line_to(at(points[1]));
        stroke.line_to(at(points[2]));
        if let Ok(path) = stroke.build() {
            let color = if dim {
                style.file_header_dim
            } else {
                style.file_header_text
            };
            window.paint_path(path, color);
        }
    }

    /// The expand arrows of a hunk header or expander row at `origin`, each
    /// centered in its gutter cell, brighter under the pointer.
    fn arrows(&self, row: Row, origin: Point<Pixels>, height: Pixels, window: &mut Window) {
        let view = self.view;
        let Some((file, gap, arrows)) = view.row_arrows(row) else {
            return;
        };
        let layout = view.list.layout();
        for (ix, &expand) in arrows.iter().enumerate() {
            let (left, width) = expand::arrow_cell(self.columns, layout, ix, arrows.len());
            let side = (height * EXPAND_ICON).min(width).round();
            let corner = origin + point(left + (width - side) / 2.0, (height - side) / 2.0);
            let at = |(x, y): (f32, f32)| corner + point(side * x, side * y);
            let strokes = match expand {
                Expand::Up => EXPAND_UP,
                Expand::Down => EXPAND_DOWN,
                Expand::All => EXPAND_ALL,
            };
            let color = if view.is_hovered(ExpandArrow { file, gap, expand }) {
                view.style.text
            } else {
                view.style.hunk_header_text
            };
            for points in strokes {
                let mut stroke = PathBuilder::stroke((side * TICK_WIDTH).max(px(1.0)));
                stroke.move_to(at(points[0]));
                for &next in &points[1..] {
                    stroke.line_to(at(next));
                }
                if let Ok(path) = stroke.build() {
                    window.paint_path(path, color);
                }
            }
        }
    }

    /// The pinned header over the rows, possibly pushed partly out of the
    /// top by the next file's.
    fn pinned_header(&self, pinned: Pinned, window: &mut Window, cx: &mut App) {
        let origin = self.bounds.origin + point(px(0.0), px(pinned.top as f32));
        let height = px(self.view.list.row_height(pinned.row) as f32);
        self.file_header(pinned.file, origin, height, true, window, cx);
    }

    /// Each rail whose block or lines are in view: a marker over the
    /// commented rows and a line from the last of them that turns into the
    /// block's header.
    fn rails(&self, window: &mut Window) {
        let view = self.view;
        let (Some(first), Some(last)) = (view.visible.first(), view.visible.last()) else {
            return;
        };
        let (first, last) = (first.row, last.row);
        let list = &view.list;
        let scroll = view.scroll_y();
        let top = |row: usize| self.bounds.origin.y + px((list.top_of(row) - scroll) as f32);
        let half_code = px(list.heights().code / 2.0);
        for (ix, rail) in view.rails.iter().enumerate() {
            let Some(rail) = rail else { continue };
            let id = BlockId(ix as u32);
            let span = list.block_span(id);
            let block_row = list.block_row(id);
            if span.is_empty() || block_row < first || span.start > last {
                continue;
            }
            let last_line = (span.start..span.end)
                .rev()
                .find(|&row| !matches!(list.rows()[row], Row::Block { .. }))
                .unwrap_or(span.start);
            let frame = view.block_frame(id, self.columns);
            let lane = f32::from(view.lanes.get(ix).copied().unwrap_or(0));
            let x = self.bounds.origin.x + frame.rail_x - px(LANE_STEP * lane);
            let card_x = self.bounds.origin.x + frame.x;
            let first_center = top(span.start) + half_code;
            let last_center = top(last_line) + half_code;
            let elbow = top(block_row) + rail.elbow;
            let line = px(RAIL_WIDTH);
            window.paint_quad(quad(
                Bounds::new(
                    point(x - line / 2.0, last_center),
                    size(
                        (card_x - x + line / 2.0).max(line),
                        (elbow - last_center + line / 2.0).max(line),
                    ),
                ),
                Corners {
                    bottom_left: px(ELBOW_RADIUS),
                    ..Corners::default()
                },
                gpui::transparent_black(),
                Edges {
                    left: line,
                    bottom: line,
                    ..Edges::default()
                },
                rail.color,
                if rail.dashed {
                    BorderStyle::Dashed
                } else {
                    BorderStyle::Solid
                },
            ));
            let marker = px(MARKER_WIDTH);
            window.paint_quad(quad(
                Bounds::new(
                    point(x - marker / 2.0, first_center - marker / 2.0),
                    size(marker, last_center - first_center + marker),
                ),
                marker / 2.0,
                rail.color,
                px(0.0),
                rail.color,
                BorderStyle::Solid,
            ));
        }
    }

    fn split_side(
        &self,
        place: Place,
        side: Side,
        side_line: Option<SideLine>,
        bounds: Bounds<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let style = &self.view.style;
        let Some(SideLine { line, kind }) = side_line else {
            window.paint_quad(fill(bounds, style.empty_background));
            return;
        };
        let (background, gutter) = self.kind_colors(kind);
        if let Some(background) = background {
            window.paint_quad(fill(bounds, background));
        }
        let number = self.columns.number;
        let height = bounds.size.height;
        window.paint_quad(fill(
            Bounds::new(bounds.origin, size(number, height)),
            gutter,
        ));
        self.number(
            line,
            bounds.origin.x + number,
            bounds.origin.y,
            height,
            window,
            cx,
        );
        let marker_x = bounds.origin.x + number;
        let y = bounds.origin.y;
        self.text(
            LineKey::Marker(kind),
            point(marker_x + self.columns.ch * 0.5, y),
            height,
            window,
            cx,
        );
        let text_origin = point(marker_x + self.columns.marker, y);
        let Place { row, file } = place;
        let at = CodeLine { file, side, line };
        let selected = self.selected(row, at, text_origin, height, bounds.right());
        let found = self.found(at, text_origin, height, bounds.right());
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.code(
                LineKey::Code { file, side, line },
                text_origin,
                height,
                Marked { selected, found },
                window,
                cx,
            );
        });
    }

    /// The selected stretch of a code line, from the left of its text at
    /// `origin` and no further than `right`.
    fn selected(
        &self,
        row: usize,
        at: CodeLine,
        origin: Point<Pixels>,
        height: Pixels,
        right: Pixels,
    ) -> Option<Bounds<Pixels>> {
        let selection = self.view.selection.as_ref()?;
        if !selection.covers(row) || selection.lock().is_some_and(|side| side != at.side) {
            return None;
        }
        let line = line_blob(self.view.list.diff(), at).line(at.line);
        let span = selection.span(row, char_count(&line))?;
        let tab_width = self.view.style.tab_width;
        let column = |index| self.columns.ch * display_col(&line, tab_width, index) as f32;
        let start = origin.x + column(span.from);
        let mut end = origin.x + column(span.to);
        if span.newline {
            end += self.columns.ch * 0.5;
        }
        let end = end.min(right);
        (end > start).then(|| Bounds::new(point(start, origin.y), size(end - start, height)))
    }

    /// Row and gutter backgrounds; context rows paint no row background.
    fn kind_colors(&self, kind: LineKind) -> (Option<Hsla>, Hsla) {
        let style = &self.view.style;
        match kind {
            LineKind::Context => (None, style.gutter_background),
            LineKind::Added => (Some(style.added_background), style.added_gutter_background),
            LineKind::Removed => (
                Some(style.removed_background),
                style.removed_gutter_background,
            ),
        }
    }

    /// A line number right-aligned to `right`.
    fn number(
        &self,
        line: u32,
        right: Pixels,
        y: Pixels,
        height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut x = right - self.columns.ch;
        let mut rest = line + 1;
        loop {
            if let Some((shaped, _)) = self.view.cache.get(LineKey::Digit((rest % 10) as u8)) {
                x -= shaped.width();
                // A failed glyph only loses this glyph; keep painting.
                let _ = shaped.paint(point(x, y), height, TextAlign::Left, None, window, cx);
            }
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
    }

    fn text(
        &self,
        key: LineKey,
        origin: Point<Pixels>,
        height: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.code(key, origin, height, Marked::default(), window, cx);
    }

    /// The find matches on a code line, as [`Self::found_in`] gives them.
    fn found(
        &self,
        at: CodeLine,
        origin: Point<Pixels>,
        height: Pixels,
        right: Pixels,
    ) -> Vec<(Bounds<Pixels>, bool)> {
        if self.view.marks.is_empty() {
            return Vec::new();
        }
        let line = line_blob(self.view.list.diff(), at).line(at.line);
        let place = (at.file, FindAt::Line(at.side, at.line));
        self.found_in(&line, place, origin, height, right)
    }

    /// The find matches at `place` over `text`, whose first char starts at
    /// `origin`: each one's stretch, no further than `right`, and whether it
    /// is the current one.
    fn found_in(
        &self,
        text: &str,
        place: (FileIx, FindAt),
        origin: Point<Pixels>,
        height: Pixels,
        right: Pixels,
    ) -> Vec<(Bounds<Pixels>, bool)> {
        let tab_width = self.view.style.tab_width;
        let column = |index| self.columns.ch * display_col(text, tab_width, index) as f32;
        self.view
            .marks
            .at(place)
            .filter_map(|(chars, current)| {
                let start = origin.x + column(chars.start);
                let end = (origin.x + column(chars.end)).min(right);
                (end > start).then(|| {
                    let bounds = Bounds::new(point(start, origin.y), size(end - start, height));
                    (bounds, current)
                })
            })
            .collect()
    }

    /// A shaped line, with the find matches and then the selection between
    /// its emphasized words and its glyphs.
    fn code(
        &self,
        key: LineKey,
        origin: Point<Pixels>,
        height: Pixels,
        marked: Marked,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some((shaped, background)) = self.view.cache.get(key) {
            let style = &self.view.style;
            if background {
                let _ = shaped.paint_background(origin, height, TextAlign::Left, None, window, cx);
            }
            for (bounds, current) in marked.found {
                let color = if current {
                    style.find_current
                } else {
                    style.find_match
                };
                window.paint_quad(fill(bounds, color));
            }
            if let Some(selected) = marked.selected {
                window.paint_quad(fill(selected, style.selection));
            }
            let _ = shaped.paint(origin, height, TextAlign::Left, None, window, cx);
        }
    }
}

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        DiffElement { view: cx.entity() }
    }
}

/// Lays out and paints only the visible rows of a [`DiffView`].
pub struct DiffElement {
    view: Entity<DiffView>,
}

impl IntoElement for DiffElement {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

/// What the element's prepaint leaves for its paint: the hitbox of the view
/// and those of the parts that sit over it.
pub struct Hitboxes {
    view: Hitbox,
    /// The visible part of the pinned header. It takes the mouse (not the
    /// wheel) from the rows and blocks under it.
    pinned: Option<Hitbox>,
    /// The cursor regions under the pinned header, each with its hitbox.
    cursors: Vec<(Hitbox, CursorStyle)>,
    /// The cursor regions of the pinned header, over the rest.
    pinned_cursors: Vec<(Hitbox, CursorStyle)>,
}

impl Element for DiffElement {
    type RequestLayoutState = ();
    type PrepaintState = Hitboxes;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        style.flex_grow = 1.0;
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitboxes {
        // Before the blocks' own hitboxes, so those sit on top of it.
        let view = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        self.view
            .update(cx, |view, cx| view.prepaint(bounds, window, cx));
        // After them, so the cursor regions and the pinned header sit on top
        // of the blocks. The header takes the mouse from what it covers.
        let map = self.view.read(cx).cursor_map(bounds.size);
        let insert = |regions: &[cursor::CursorRegion], window: &mut Window| {
            regions
                .iter()
                .map(|region| {
                    let mut at = region.bounds;
                    at.origin += bounds.origin;
                    (
                        window.insert_hitbox(at, HitboxBehavior::Normal),
                        region.style,
                    )
                })
                .collect::<Vec<_>>()
        };
        let cursors = insert(&map.regions[..map.over_pinned], window);
        let pinned = map
            .pinned
            .map(|at| Bounds::new(bounds.origin + at.origin, at.size))
            .map(|at| window.insert_hitbox(at, HitboxBehavior::BlockMouseExceptScroll));
        self.view.update(cx, |view, cx| {
            view.prepaint_pinned_header(bounds, window, cx);
        });
        let pinned_cursors = insert(&map.regions[map.over_pinned..], window);
        Hitboxes {
            view,
            pinned,
            cursors,
            pinned_cursors,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        hitboxes: &mut Hitboxes,
        window: &mut Window,
        cx: &mut App,
    ) {
        let view = self.view.clone();
        let hitbox_id = hitboxes.view.id;
        scroll::install(&view, &hitboxes.view, |view| &mut view.scroll, window);
        window.on_mouse_event({
            let view = view.clone();
            move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || !hitbox_id.should_handle_scroll(window) {
                    return;
                }
                view.update(cx, |view, cx| {
                    let delta = event.delta.pixel_delta(view.style.line_height);
                    if view.scroll_by(-f64::from(delta.y)) {
                        cx.notify();
                    }
                });
                cx.stop_propagation();
            }
        });
        let pinned_id = hitboxes.pinned.as_ref().map(|hitbox| hitbox.id);
        install_chrome(&view, hitbox_id, pinned_id, bounds.origin, window);
        install_text_selection(&view, hitbox_id, bounds.origin, window);
        install_context_menu(&view, hitbox_id, pinned_id, bounds.origin, window);
        // Later requests win, so the pinned header's come last.
        for (hitbox, style) in hitboxes.cursors.iter().chain(&hitboxes.pinned_cursors) {
            window.set_cursor_style(*style, hitbox);
        }
        if let Some(style) = self.view.read(cx).drag_cursor() {
            window.set_window_cursor_style(style);
        }
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            self.view
                .update(cx, |view, cx| view.paint(bounds, window, cx));
        });
    }
}

/// Registers the mouse listeners of the parts that sit over the rows: the
/// file headers (the viewed checkbox, and the pinned one's path) and the
/// split divider. They take a press before text selection does.
fn install_chrome(
    view: &Entity<DiffView>,
    hitbox: gpui::HitboxId,
    pinned: Option<gpui::HitboxId>,
    origin: Point<Pixels>,
    window: &mut Window,
) {
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !(hitbox.is_hovered(window) || pinned.is_some_and(|id| id.is_hovered(window)))
            {
                return;
            }
            let point = event.position - origin;
            let taken = view.update(cx, |view, cx| {
                let pressed = view
                    .press_header(point)
                    .or_else(|| view.press_arrow(point))
                    .or_else(|| view.press_divider(point, event.click_count))?;
                if let Some(event) = pressed {
                    cx.emit(event);
                }
                cx.notify();
                Some(())
            });
            if taken.is_some() {
                cx.stop_propagation();
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let over = hitbox.is_hovered(window) && event.pressed_button.is_none();
            let point = over.then(|| event.position - origin);
            view.update(cx, |view, cx| {
                if view.hover_arrow(point) {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseMoveEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            view.update(cx, |view, cx| {
                if !view.is_resizing() {
                    return;
                }
                let moved = if event.pressed_button == Some(MouseButton::Left) {
                    view.drag_divider(event.position - origin)
                } else {
                    release_divider(view, cx)
                };
                if moved {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                return;
            }
            view.update(cx, |view, cx| {
                if release_divider(view, cx) {
                    cx.notify();
                }
            });
        }
    });
}

/// Registers the right-click listener: it asks the view what is under the
/// pointer and emits [`DiffViewEvent::ContextMenu`]. It changes nothing in
/// the view, so the selection stays and no drag starts.
fn install_context_menu(
    view: &Entity<DiffView>,
    hitbox: gpui::HitboxId,
    pinned: Option<gpui::HitboxId>,
    origin: Point<Pixels>,
    window: &mut Window,
) {
    let view = view.clone();
    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
        if phase != DispatchPhase::Bubble
            || event.button != MouseButton::Right
            || !(hitbox.is_hovered(window) || pinned.is_some_and(|id| id.is_hovered(window)))
        {
            return;
        }
        let position = event.position;
        let emitted = view.update(cx, |view, cx| {
            let target = view.context_target(position - origin)?;
            cx.emit(DiffViewEvent::ContextMenu { position, target });
            Some(())
        });
        if emitted.is_some() {
            cx.stop_propagation();
        }
    });
}

/// Ends a divider drag, telling the app the ratio it ended at.
fn release_divider(view: &mut DiffView, cx: &mut Context<DiffView>) -> bool {
    let was = view.is_resizing();
    if let Some(event) = view.release_divider() {
        cx.emit(event);
    }
    was
}

/// Registers the mouse listeners of text selection for the frame. Moves and
/// releases follow the window, so a drag goes on outside the view. The press
/// is taken after the scrollbar had its chance at it, and not when another
/// element covers the view.
fn install_text_selection(
    view: &Entity<DiffView>,
    hitbox: gpui::HitboxId,
    origin: Point<Pixels>,
    window: &mut Window,
) {
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble
                || event.button != MouseButton::Left
                || !hitbox.is_hovered(window)
            {
                return;
            }
            view.update(cx, |view, cx| {
                view.watch_keys(cx);
                let extend = event.modifiers.shift;
                if view.press_text(event.position - origin, event.click_count, extend) {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseMoveEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            view.update(cx, |view, cx| {
                if !view.is_selecting() {
                    return;
                }
                // The release happened outside the window.
                let moved = if event.pressed_button == Some(MouseButton::Left) {
                    view.drag_text(event.position - origin)
                } else {
                    view.release_text()
                };
                if moved {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                return;
            }
            view.update(cx, |view, cx| {
                if view.release_text() {
                    cx.notify();
                }
            });
        }
    });
}

#[cfg(test)]
#[expect(clippy::single_range_in_vec_init, reason = "one range is the point")]
pub(super) mod tests {
    use super::*;
    use diff::{FileDiff, Hunk, Segment};

    /// `a b c d` against `a B B2 c d`: one context line, a change of one
    /// line into two, two context lines.
    fn hit_list(layout: Layout) -> DiffList {
        let file = FileDiff {
            path: "a.rs".into(),
            old: Text::new("a\nb\nc\nd\n"),
            new: Text::new("a\nB\nB2\nc\nd\n"),
            hunks: vec![Hunk {
                old_start: 0,
                new_start: 0,
                segments: vec![
                    Segment::Context(1),
                    Segment::Change {
                        removed: 1,
                        added: 2,
                    },
                    Segment::Context(2),
                ],
            }],
        };
        let diff = Diff {
            files: vec![file],
            blocks: Vec::new(),
        };
        let heights = RowHeights {
            code: 20.0,
            file_header: 32.0,
        };
        DiffList::new(diff, layout, heights)
    }

    fn hit_columns() -> Columns {
        Columns {
            ch: px(8.0),
            number: px(24.0),
            marker: px(16.0),
            width: px(600.0),
            split: px(300.0),
        }
    }

    fn mid(list: &DiffList, row: usize) -> f32 {
        (list.top_of(row) + list.row_height(row) / 2.0) as f32
    }

    #[test]
    fn unified_rows_hit_their_new_line_or_the_removed_old_one() {
        let list = hit_list(Layout::Unified);
        let at = |scroll: f64, x: f32, row: usize| {
            hit_at(
                &list,
                scroll,
                hit_columns(),
                point(px(x), px(mid(&list, row))),
            )
        };
        // Rows: file header, hunk header, a, -b, +B, +B2, c, d.
        assert_eq!(at(0.0, 5.0, 0), None);
        assert_eq!(at(0.0, 5.0, 3), Some((3, Side::Old, 1)));
        assert_eq!(at(0.0, 5.0, 4), Some((4, Side::New, 1)));
        assert_eq!(at(0.0, 400.0, 6), Some((6, Side::New, 3)));
        // Scrolled one row down, the same point is the next row.
        assert_eq!(at(20.0, 5.0, 4), Some((5, Side::New, 2)));
        let below = hit_at(&list, 0.0, hit_columns(), point(px(5.0), px(10_000.0)));
        assert_eq!(below, None);
        let outside = hit_at(
            &list,
            0.0,
            hit_columns(),
            point(px(-1.0), px(mid(&list, 4))),
        );
        assert_eq!(outside, None);
    }

    #[test]
    fn split_halves_hit_their_own_side() {
        let list = hit_list(Layout::Split);
        let at = |x: f32, row: usize| {
            hit_at(&list, 0.0, hit_columns(), point(px(x), px(mid(&list, row))))
        };
        // Rows: file header, hunk header, a|a, b|B, -|B2, c|c, d|d.
        assert_eq!(at(5.0, 3), Some((3, Side::Old, 1)));
        assert_eq!(at(305.0, 3), Some((3, Side::New, 1)));
        // The left half of the added-only row has no line.
        assert_eq!(at(5.0, 4), None);
        assert_eq!(at(305.0, 4), Some((4, Side::New, 2)));
    }

    #[test]
    fn the_gutter_is_two_columns_unified_and_one_per_half_split() {
        let style = test_style();
        let mut unified = DiffView::new(
            hit_list(Layout::Unified).diff().clone(),
            Layout::Unified,
            style.clone(),
        );
        assert_eq!(
            unified.gutter_width(),
            None,
            "nothing before the first frame"
        );
        unified.columns = Some(hit_columns());
        assert_eq!(unified.gutter_width(), Some(px(48.0)));
        assert_eq!(unified.pane_left(Side::New), Some(px(0.0)));
        let mut split = DiffView::new(hit_list(Layout::Split).diff().clone(), Layout::Split, style);
        split.columns = Some(hit_columns());
        assert_eq!(split.gutter_width(), Some(px(24.0)));
        assert_eq!(split.pane_left(Side::Old), Some(px(0.0)));
        assert_eq!(split.pane_left(Side::New), Some(px(300.0)));
    }

    #[test]
    fn word_cache_pairs_both_sides_of_a_change() {
        let file = FileDiff {
            path: "a.rs".into(),
            old: Text::new("keep\nlet a = 1;\nlast\n"),
            new: Text::new("keep\nlet a = 2;\nextra\nlast\n"),
            hunks: vec![Hunk {
                old_start: 0,
                new_start: 0,
                segments: vec![
                    Segment::Context(1),
                    Segment::Change {
                        removed: 1,
                        added: 2,
                    },
                    Segment::Context(1),
                ],
            }],
        };
        let pairs = Pairs::of(&file);
        let diff = Diff {
            files: vec![file],
            blocks: Vec::new(),
        };
        let mut cache = WordCache::default();
        let f = FileIx(0);
        let old = cache.of(&diff, &pairs, f, Side::Old, 1).expect("old side");
        let new = cache.of(&diff, &pairs, f, Side::New, 1).expect("new side");
        assert!(Arc::ptr_eq(&old, &new), "one diff serves both lines");
        assert_eq!((old.old.clone(), old.new.clone()), (vec![8..9], vec![8..9]));
        // The unpaired extra added line and context lines have none.
        assert!(cache.of(&diff, &pairs, f, Side::New, 2).is_none());
        assert!(cache.of(&diff, &pairs, f, Side::New, 0).is_none());
        assert_eq!(cache.pairs.len(), 1);
    }

    #[test]
    fn rows_paint_the_blobs_of_their_lines() {
        let file = FileIx(3);
        let side = |line, kind| Some(SideLine { line, kind });
        let blobs = |row| row_blobs(row).collect::<Vec<_>>();
        let line = |old, new| Row::Line { file, old, new };
        assert_eq!(blobs(line(Some(1), Some(1))), [(file, Side::New)]);
        assert_eq!(blobs(line(Some(1), None)), [(file, Side::Old)]);
        assert_eq!(blobs(line(None, Some(2))), [(file, Side::New)]);
        let pair = Row::Pair {
            file,
            left: side(0, LineKind::Removed),
            right: side(0, LineKind::Added),
        };
        assert_eq!(blobs(pair), [(file, Side::Old), (file, Side::New)]);
        let half = Row::Pair {
            file,
            left: None,
            right: side(4, LineKind::Added),
        };
        assert_eq!(blobs(half), [(file, Side::New)]);
        assert!(blobs(Row::FileHeader { file }).is_empty());
    }

    #[test]
    fn languages_follow_paths_unless_overridden() {
        let diff = Diff {
            files: vec![
                diff::FileDiff {
                    path: "a/b.rs".into(),
                    ..Default::default()
                },
                diff::FileDiff {
                    path: "Dockerfile".into(),
                    ..Default::default()
                },
            ],
            blocks: Vec::new(),
        };
        assert_eq!(file_languages(&diff, None), [Some(Language::Rust), None]);
        assert_eq!(
            file_languages(&diff, Some(Language::Go)),
            [Some(Language::Go); 2]
        );
    }

    pub(super) fn test_style() -> DiffStyle {
        DiffStyle {
            font_family: "mono".into(),
            font_size: px(13.0),
            line_height: px(20.0),
            file_header_height: px(30.0),
            tab_width: 4,
            background: Hsla::default(),
            text: Hsla::default(),
            gutter_background: Hsla::default(),
            gutter_text: Hsla::default(),
            added_background: Hsla::default(),
            added_gutter_background: Hsla::default(),
            removed_background: Hsla::default(),
            removed_gutter_background: Hsla::default(),
            add_emphasis: Hsla::default(),
            remove_emphasis: Hsla::default(),
            empty_background: Hsla::default(),
            file_header_background: Hsla::default(),
            file_header_text: Hsla::default(),
            file_header_dim: Hsla::default(),
            checkbox_border: Hsla::default(),
            checkbox_checked: Hsla::default(),
            checkbox_mark: Hsla::default(),
            stat_added: Hsla::default(),
            stat_removed: Hsla::default(),
            stat_empty: Hsla::default(),
            hunk_header_background: Hsla::default(),
            hunk_header_text: Hsla::default(),
            block_background: Hsla::default(),
            block_border: Hsla::default(),
            divider: Hsla::default(),
            divider_active: Hsla::default(),
            selection: Hsla::default(),
            find_match: Hsla::default(),
            find_current: Hsla::default(),
            scrollbar: scroll::ScrollbarStyle::default(),
            autoscroll: scroll::AutoscrollStyle::default(),
            palette: crate::SyntaxPalette {
                keyword: Hsla::default(),
                function: Hsla::default(),
                type_: Hsla::default(),
                string: Hsla::default(),
                number: Hsla::default(),
                comment: Hsla::default(),
                constant: Hsla::default(),
                operator: Hsla::default(),
                punctuation: Hsla::default(),
                property: Hsla::default(),
                variable: Hsla::default(),
                attribute: Hsla::default(),
                tag: Hsla::default(),
                plain: Hsla::default(),
            },
        }
    }

    /// One file of 200 context lines with a block after lines 10 and 100.
    fn tall_view() -> DiffView {
        let text = Text::new((0..200).map(|i| format!("l{i}\n")).collect::<String>());
        let anchor = |line| diff::Anchor::Line {
            file: FileIx(0),
            side: Side::New,
            line,
        };
        let diff = Diff {
            files: vec![FileDiff {
                path: "a.txt".into(),
                old: text.clone(),
                new: text,
                hunks: vec![Hunk {
                    old_start: 0,
                    new_start: 0,
                    segments: vec![Segment::Context(200)],
                }],
            }],
            blocks: vec![Block::new(anchor(10), 50.0), Block::new(anchor(100), 50.0)],
        };
        DiffView::new(diff, Layout::Unified, test_style())
    }

    #[test]
    fn a_block_measured_above_the_top_row_does_not_move_it() {
        let mut view = tall_view();
        // Rows: header, hunk header, lines 0..=10, block 0, ...
        let block_row = view.list.block_row(BlockId(0));
        let below = block_row + 20;
        view.set_scroll_y(view.list.top_of(below) + 7.0);
        let before = view.list.row_at(view.scroll_y());
        view.resize_blocks(&[(BlockId(0), 180.0)]);
        assert_eq!(view.list.row_at(view.scroll_y()), before);
        // A block below the top row leaves the offset alone.
        let y = view.scroll_y();
        view.resize_blocks(&[(BlockId(1), 10.0)]);
        assert_eq!(view.scroll_y(), y);
        // The top row itself: the offset into it is kept, up to its new
        // height, so a card that shrinks past the top leaves the next row
        // there.
        view.set_scroll_y(view.list.top_of(block_row) + 150.0);
        view.resize_blocks(&[(BlockId(0), 40.0)]);
        assert_eq!(view.scroll_y(), view.list.top_of(block_row) + 40.0);
        view.set_scroll_y(view.list.top_of(block_row) + 30.0);
        view.resize_blocks(&[(BlockId(0), 90.0)]);
        assert_eq!(view.scroll_y(), view.list.top_of(block_row) + 30.0);
    }

    #[test]
    fn step_to_moves_between_rows_and_stops_at_the_ends() {
        let mut view = tall_view();
        let rows = [5usize, 40, 120];
        assert_eq!(view.step_to(rows, Step::Next, 0.0), Some(5));
        assert_eq!(view.scroll_y(), view.list.top_of(5));
        assert_eq!(view.step_to(rows, Step::Next, 0.0), Some(40));
        assert_eq!(view.step_to(rows, Step::Prev, 0.0), Some(5));
        assert_eq!(view.step_to(rows, Step::Prev, 0.0), None);
        // A margin leaves room above the row.
        assert_eq!(view.step_to(rows, Step::Next, 40.0), Some(40));
        assert_eq!(view.scroll_y(), view.list.top_of(40) - 40.0);
    }

    #[test]
    fn set_blocks_keeps_the_top_line() {
        let mut view = tall_view();
        let line_row = view.list.block_row(BlockId(1)) + 5;
        view.set_scroll_y(view.list.top_of(line_row) + 3.0);
        let line = RowTarget::of(view.list.rows()[line_row]);
        view.set_blocks(Vec::new(), Vec::new());
        let top = view.list.row_at(view.scroll_y());
        assert!(line.shown_by(view.list.rows()[top.row]));
        assert_eq!(top.offset, 3.0);
        assert_eq!(view.rails.len(), 0);
    }
}
