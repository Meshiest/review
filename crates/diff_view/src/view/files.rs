//! What the view knows about files: the pinned (sticky) file header, the
//! viewed checkbox in it, the current file, the file filter and the context
//! mode.

use diff::{ContextMode, DiffList, FileIx, Row};
use gpui::{Bounds, Context, EventEmitter, Pixels, Point, point, px, size};

use super::{Columns, ContextTarget, DiffView};
use crate::blocks::{HeaderPlace, HeaderSlots, RenderHeader};

/// What a [`DiffView`] tells the app. The view never changes its own state in
/// response to these: the app owns the viewed marks and the split ratio.
#[derive(Clone, Debug, PartialEq)]
pub enum DiffViewEvent {
    /// The viewed checkbox of a file header was clicked; the app flips its
    /// mark and calls [`DiffView::set_viewed`].
    ToggleViewed(FileIx),
    /// A drag of the split divider ended (or a double click reset it) at this
    /// ratio of the left pane's width to the view's.
    SplitRatioChanged(f32),
    /// A file's body was collapsed (`true`) or expanded by a click on its
    /// header, [`DiffView::set_file_collapsed`], [`DiffView::collapse_all`],
    /// [`DiffView::expand_all`] or [`DiffView::toggle_current_file`], for the
    /// app to remember. Collapsing that follows a viewed mark (see
    /// [`DiffView::set_collapse_viewed`]) is not announced: the app owns the
    /// marks.
    FileCollapsed(FileIx, bool),
    /// A right-click (a secondary press) on `target`, at `position` in the
    /// window. The view keeps its selection and starts no drag; the app
    /// shows the menu. Thread cards never emit it.
    ContextMenu {
        position: Point<Pixels>,
        target: ContextTarget,
    },
    /// A click on an expand arrow revealed more context lines of a file
    /// (see [`DiffView::reveals`]).
    ContextRevealed(FileIx),
}

impl EventEmitter<DiffViewEvent> for DiffView {}

/// The file header held at the top of the view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Pinned {
    pub file: FileIx,
    /// Its row, where the header really is.
    pub row: usize,
    /// Where it is painted, relative to the view's top: 0, or negative while
    /// the next file's header pushes it out.
    pub top: f64,
}

/// The header to pin when the view is scrolled to `scroll_y`: that of the
/// file whose rows are at the top, once its own header has scrolled off, held
/// at 0 until the next file's header comes up against it and then pushed up
/// with it.
pub(super) fn pinned_at(list: &DiffList, scroll_y: f64) -> Option<Pinned> {
    if list.is_empty() {
        return None;
    }
    let at = list.row_at(scroll_y).row;
    let headers = list.file_rows();
    let ix = headers.partition_point(|&row| row as usize <= at) - 1;
    let row = headers[ix] as usize;
    if scroll_y <= list.top_of(row) {
        return None;
    }
    let height = list.row_height(row);
    let top = headers
        .get(ix + 1)
        .map(|&next| list.top_of(next as usize) - scroll_y - height)
        .map_or(0.0, |pushed| pushed.min(0.0));
    Some(Pinned {
        file: list.file_of_row(at),
        row,
        top,
    })
}

/// Where the viewed checkbox and its label sit in a file header, relative to
/// the header's top left.
#[derive(Clone, Copy)]
pub(super) struct ViewedZone {
    /// The click target spans from `left` to `right`, where the trailing
    /// controls start (the header's right end without them).
    pub left: Pixels,
    pub right: Pixels,
    pub checkbox: Bounds<Pixels>,
    pub label_x: Pixels,
}

/// The collapse chevron at the left of a file header, relative to the
/// header's top left.
pub(super) fn chevron_zone(columns: Columns, height: Pixels) -> Bounds<Pixels> {
    let side = (height * 0.5).round();
    Bounds::new(
        point(columns.ch * 0.5, ((height - side) / 2.0).round()),
        size(side, side),
    )
}

/// Where the path starts in a file header, after the chevron.
pub(super) fn path_left(columns: Columns, height: Pixels) -> Pixels {
    chevron_zone(columns, height).right() + columns.ch * 0.75
}

/// The viewed zone of a header whose last `trailing` pixels hold the app's
/// trailing controls.
pub(super) fn viewed_zone(columns: Columns, height: Pixels, trailing: Pixels) -> ViewedZone {
    let side = (height * 0.5).round();
    let right = (columns.width - trailing).max(px(0.0));
    let label_x = right - columns.ch * 7.0;
    let checkbox_x = label_x - columns.ch - side;
    ViewedZone {
        left: checkbox_x - columns.ch * 0.5,
        right,
        checkbox: Bounds::new(
            point(checkbox_x, ((height - side) / 2.0).round()),
            size(side, side),
        ),
        label_x,
    }
}

/// What a press on a file header does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HeaderClick {
    /// The viewed checkbox.
    Toggle(FileIx),
    /// The pinned header's path.
    ScrollTo(FileIx),
    /// The chevron and the header's empty space.
    ToggleCollapse(FileIx),
}

impl DiffView {
    /// Sets the viewed mark of a file, which the view shows in its header
    /// (dimmed, with a checked box) and, with
    /// [`DiffView::set_collapse_viewed`], collapses the file's rows to.
    pub fn set_viewed(&mut self, file: FileIx, viewed: bool, cx: &mut Context<Self>) {
        if self.apply_viewed(file, viewed) {
            cx.notify();
        }
    }

    /// Sets every file's viewed mark, by file; files past the end of `viewed`
    /// are not viewed.
    pub fn set_viewed_all(&mut self, mut viewed: Vec<bool>, cx: &mut Context<Self>) {
        viewed.resize(self.list.diff().files.len(), false);
        if self.apply_viewed_all(viewed) {
            cx.notify();
        }
    }

    pub(super) fn apply_viewed_all(&mut self, viewed: Vec<bool>) -> bool {
        if viewed == self.viewed {
            return false;
        }
        if self.collapse_viewed {
            let marks = self.viewed.iter().zip(&viewed);
            for (collapsed, (&was, &now)) in self.collapsed.iter_mut().zip(marks) {
                if was != now {
                    *collapsed = now;
                }
            }
        }
        self.viewed = viewed;
        self.sync_collapsed();
        true
    }

    pub fn is_viewed(&self, file: FileIx) -> bool {
        self.viewed.get(file.0 as usize).copied().unwrap_or(false)
    }

    /// Whether a file collapses when it becomes viewed and expands when it
    /// stops being viewed. Off by default. Turning it on collapses the files
    /// viewed now. A viewed file can still be expanded by hand
    /// ([`DiffView::set_file_collapsed`]) and stays so until its mark
    /// changes.
    pub fn set_collapse_viewed(&mut self, collapse: bool, cx: &mut Context<Self>) {
        if self.apply_collapse_viewed(collapse) {
            cx.notify();
        }
    }

    pub(super) fn apply_collapse_viewed(&mut self, collapse: bool) -> bool {
        if collapse == self.collapse_viewed {
            return false;
        }
        self.collapse_viewed = collapse;
        if !collapse {
            return false;
        }
        for (collapsed, &viewed) in self.collapsed.iter_mut().zip(&self.viewed) {
            *collapsed |= viewed;
        }
        self.sync_collapsed();
        true
    }

    pub(super) fn apply_viewed(&mut self, file: FileIx, viewed: bool) -> bool {
        let Some(mark) = self.viewed.get_mut(file.0 as usize) else {
            return false;
        };
        if *mark == viewed {
            return false;
        }
        *mark = viewed;
        if self.collapse_viewed {
            self.collapsed[file.0 as usize] = viewed;
        }
        self.sync_collapsed();
        true
    }

    /// Whether the file's body is folded away, leaving its header.
    #[must_use]
    pub fn is_file_collapsed(&self, file: FileIx) -> bool {
        self.collapsed
            .get(file.0 as usize)
            .copied()
            .unwrap_or(false)
    }

    /// Collapses or expands one file, announcing it with
    /// [`DiffViewEvent::FileCollapsed`] when that changes anything. The rows
    /// above the top of the view stay put, and when the top is inside the
    /// body that goes away, the file's header takes it.
    pub fn set_file_collapsed(&mut self, file: FileIx, collapsed: bool, cx: &mut Context<Self>) {
        if self.apply_collapsed(file, collapsed) {
            cx.emit(DiffViewEvent::FileCollapsed(file, collapsed));
            cx.notify();
        }
    }

    /// Collapses every file.
    pub fn collapse_all(&mut self, cx: &mut Context<Self>) {
        self.set_all_collapsed(true, cx);
    }

    /// Expands every file.
    pub fn expand_all(&mut self, cx: &mut Context<Self>) {
        self.set_all_collapsed(false, cx);
    }

    /// Flips the collapse of [`DiffView::current_file`]; its new state, or
    /// `None` for an empty diff.
    pub fn toggle_current_file(&mut self, cx: &mut Context<Self>) -> Option<bool> {
        let file = self.current_file()?;
        let collapsed = !self.is_file_collapsed(file);
        self.set_file_collapsed(file, collapsed, cx);
        Some(collapsed)
    }

    fn set_all_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        let changed = self.apply_all_collapsed(collapsed);
        if changed.is_empty() {
            return;
        }
        for file in changed {
            cx.emit(DiffViewEvent::FileCollapsed(file, collapsed));
        }
        cx.notify();
    }

    /// Collapses or expands every file; the files that changed.
    pub(super) fn apply_all_collapsed(&mut self, collapsed: bool) -> Vec<FileIx> {
        let changed: Vec<FileIx> = (0..self.collapsed.len() as u32)
            .map(FileIx)
            .filter(|&file| self.is_file_collapsed(file) != collapsed)
            .collect();
        if !changed.is_empty() {
            self.collapsed.fill(collapsed);
            self.sync_collapsed();
        }
        changed
    }

    pub(super) fn apply_collapsed(&mut self, file: FileIx, collapsed: bool) -> bool {
        let Some(mark) = self.collapsed.get_mut(file.0 as usize) else {
            return false;
        };
        if *mark == collapsed {
            return false;
        }
        *mark = collapsed;
        self.sync_collapsed();
        true
    }

    /// Gives the list the files to fold; the line at the top of the view
    /// stays there, or its file's header when its rows are gone.
    fn sync_collapsed(&mut self) {
        let collapsed = self.collapsed.clone();
        self.reproject(|list| list.set_collapsed(collapsed));
    }

    /// The file whose rows fill the top of the view: the pinned header's, or
    /// the header at the top's own. `None` for an empty diff.
    #[must_use]
    pub fn current_file(&self) -> Option<FileIx> {
        if self.list.is_empty() {
            return None;
        }
        Some(self.list.file_of_row(self.list.row_at(self.scroll_y()).row))
    }

    /// Scrolls so the file's header is at the top of the view; false when
    /// the view does not show the file.
    pub fn scroll_to_file(&mut self, file: FileIx) -> bool {
        let Some(row) = self.list.file_row(file) else {
            return false;
        };
        self.set_scroll_y(self.list.top_of(row));
        true
    }

    /// The header held at the top of the view, if any.
    pub(super) fn pinned(&self) -> Option<Pinned> {
        pinned_at(&self.list, self.scroll_y())
    }

    /// The header under `point` (relative to the view's top left) and the
    /// point's x in it; the pinned header covers what is under it.
    pub(super) fn header_at(&self, point: Point<Pixels>) -> Option<(FileIx, Pixels, bool)> {
        let columns = self.columns?;
        let (x, y) = (point.x, f64::from(point.y));
        if self.list.is_empty() || x < px(0.0) || x >= columns.width || y < 0.0 {
            return None;
        }
        if let Some(pinned) = self.pinned()
            && self.pinned_covers(point)
        {
            return Some((pinned.file, x, true));
        }
        let at = self.scroll_y() + y;
        if at >= self.list.total_height() {
            return None;
        }
        match self.list.rows()[self.list.row_at(at).row] {
            Row::FileHeader { file } => Some((file, x, false)),
            _ => None,
        }
    }

    /// The visible part of the pinned header in a view at `bounds`.
    pub(super) fn pinned_bounds(&self, bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        let (columns, pinned) = (self.columns?, self.pinned()?);
        let height = (self.list.row_height(pinned.row) + pinned.top) as f32;
        (height > 0.0).then(|| Bounds::new(bounds.origin, size(columns.width, px(height))))
    }

    /// Whether `point` is on a file header, pinned or not.
    pub(super) fn on_header(&self, point: Point<Pixels>) -> bool {
        self.header_at(point).is_some()
    }

    /// What a press at `point` on a file header means: the checkbox toggles
    /// the viewed mark; the path of the pinned header goes to its file; the
    /// chevron and the space around the path fold the file's body.
    pub(super) fn header_click(&self, point: Point<Pixels>) -> Option<HeaderClick> {
        let (file, x, pinned) = self.header_at(point)?;
        let zone = self.viewed_zone(self.columns?);
        if (zone.left..zone.right).contains(&x) {
            return Some(HeaderClick::Toggle(file));
        }
        let (left, right) = self.path_span(file)?;
        if (left..right).contains(&x) {
            pinned.then_some(HeaderClick::ScrollTo(file))
        } else {
            Some(HeaderClick::ToggleCollapse(file))
        }
    }

    /// The viewed zone of every header, after the trailing controls.
    pub(super) fn viewed_zone(&self, columns: Columns) -> ViewedZone {
        viewed_zone(
            columns,
            self.style.file_header_height,
            self.header_slots.trailing,
        )
    }

    /// Where a header's path is cut short: before the room of the controls
    /// after it and of the stats.
    pub(super) fn path_clip(&self, columns: Columns) -> Pixels {
        let slots = self.header_slots;
        let left = path_left(columns, self.style.file_header_height);
        (self.viewed_zone(columns).left - slots.stats - slots.after_path).max(left)
    }

    /// Where the path of `file` is in its header: from its left to its right
    /// end, in the code font's columns, cut short where [`Self::path_clip`]
    /// says.
    pub(super) fn path_span(&self, file: FileIx) -> Option<(Pixels, Pixels)> {
        let columns = self.columns?;
        let left = path_left(columns, self.style.file_header_height);
        let chars = self.list.diff().file(file).path.chars().count();
        let right = (left + columns.ch * chars as f32).min(self.path_clip(columns));
        Some((left, right))
    }

    /// Where a [`crate::RenderHeader`]'s controls go in `file`'s header.
    pub(super) fn header_place(&self, file: FileIx, pinned: bool) -> Option<HeaderPlace> {
        let columns = self.columns?;
        let (_, path_end) = self.path_span(file)?;
        let zone = self.viewed_zone(columns);
        Some(HeaderPlace {
            file,
            pinned,
            size: size(columns.width, self.style.file_header_height),
            after_path: path_end,
            trailing: zone.right,
            context: self.list.context(file),
        })
    }

    /// Draws `render`'s controls over each file header, in the room `slots`
    /// keeps free for them.
    pub fn set_header_renderer(&mut self, slots: HeaderSlots, render: RenderHeader) {
        self.header_slots = slots;
        self.render_header = Some(render);
    }

    /// Collapses the files marked in `collapsed` (by file; the rest are
    /// expanded) without announcing it: for the app restoring state it
    /// keeps. The line at the top of the view stays where it is.
    pub fn set_collapsed_files(&mut self, mut collapsed: Vec<bool>, cx: &mut Context<Self>) {
        collapsed.resize(self.collapsed.len(), false);
        if collapsed != self.collapsed {
            self.collapsed = collapsed;
            self.sync_collapsed();
            cx.notify();
        }
    }

    /// By file: which are collapsed.
    #[must_use]
    pub fn collapsed_files(&self) -> &[bool] {
        &self.collapsed
    }

    /// A left press at `point`. On a header it does what [`HeaderClick`]
    /// says, returning the event to emit for a toggle; `None` when the press
    /// is not on a header's active part.
    pub(super) fn press_header(&mut self, point: Point<Pixels>) -> Option<Option<DiffViewEvent>> {
        match self.header_click(point)? {
            HeaderClick::Toggle(file) => Some(Some(DiffViewEvent::ToggleViewed(file))),
            HeaderClick::ScrollTo(file) => {
                self.scroll_to_file(file);
                Some(None)
            }
            HeaderClick::ToggleCollapse(file) => {
                let collapsed = !self.is_file_collapsed(file);
                self.apply_collapsed(file, collapsed);
                Some(Some(DiffViewEvent::FileCollapsed(file, collapsed)))
            }
        }
    }

    /// Shows only `file` (from its top), or every file for `None` (at the file
    /// that was shown, if any). Viewed marks and the sticky header are
    /// unaffected; the thread blocks of the other files are not shown.
    pub fn show_file(&mut self, file: Option<FileIx>, cx: &mut Context<Self>) {
        if self.apply_show_file(file) {
            cx.notify();
        }
    }

    pub(super) fn apply_show_file(&mut self, file: Option<FileIx>) -> bool {
        if file.is_some_and(|f| f.0 as usize >= self.list.diff().files.len()) {
            return false;
        }
        let from = self.list.only();
        self.clear_selection();
        self.list.set_only(file);
        self.update_lanes();
        let y = match file {
            Some(_) => 0.0,
            None => from
                .and_then(|f| self.list.file_row(f))
                .map_or(0.0, |row| self.list.top_of(row)),
        };
        self.set_scroll_y(y);
        true
    }

    /// The one file shown, if the view is restricted to one.
    #[must_use]
    pub fn shown_file(&self) -> Option<FileIx> {
        self.list.only()
    }

    /// Shows every file with `mode`. The line at the top of the view stays
    /// there where it is still shown.
    pub fn set_context(&mut self, mode: ContextMode, cx: &mut Context<Self>) {
        self.reproject(|list| list.set_context(None, mode));
        cx.notify();
    }

    /// Shows one file with `mode`.
    pub fn set_file_context(&mut self, file: FileIx, mode: ContextMode, cx: &mut Context<Self>) {
        if file.0 as usize >= self.list.diff().files.len() {
            return;
        }
        self.reproject(|list| list.set_context(Some(file), mode));
        cx.notify();
    }

    /// How `file` is shown.
    #[must_use]
    pub fn context(&self, file: FileIx) -> ContextMode {
        self.list.context(file)
    }

    /// Changes the rows' projection, keeping the line at the top of the view
    /// where it is still shown and the nearest one to it where it is not.
    pub(super) fn reproject(&mut self, change: impl FnOnce(&mut DiffList)) {
        let top = self.top_line_target();
        change(&mut self.list);
        self.update_lanes();
        self.rebase_selection();
        match top {
            Some((target, offset)) => self.restore_target(target, offset),
            None => {
                self.set_scroll_y(0.0);
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use diff::{Anchor, Block, Diff, FileDiff, Hunk, Layout, Segment, Side, Text};
    use gpui::{point, px};

    use super::*;
    use crate::view::row_line;
    use crate::view::tests::test_style;

    /// A 30-line file with one hunk: two context lines, one line changed,
    /// two more context lines. Unified: header, hunk header, 2 + 2 + 2 rows
    /// and the expander of the 15 lines after them.
    pub(in crate::view) fn file(path: &str) -> FileDiff {
        let text = |changed: &str| {
            Text::new(
                (0..30)
                    .map(|i| {
                        if i == 12 {
                            format!("{changed}\n")
                        } else {
                            format!("l{i}\n")
                        }
                    })
                    .collect::<String>(),
            )
        };
        FileDiff {
            path: path.into(),
            old: text("old"),
            new: text("new"),
            hunks: vec![Hunk {
                old_start: 10,
                new_start: 10,
                segments: vec![
                    Segment::Context(2),
                    Segment::Change {
                        removed: 1,
                        added: 1,
                    },
                    Segment::Context(2),
                ],
            }],
        }
    }

    pub(in crate::view) fn view(layout: Layout, files: usize, blocks: Vec<Block>) -> DiffView {
        let diff = Diff {
            files: (0..files).map(|i| file(&format!("f{i}.rs"))).collect(),
            blocks,
        };
        let mut view = DiffView::new(diff, layout, test_style());
        view.columns = Some(Columns {
            ch: px(8.0),
            number: px(24.0),
            marker: px(16.0),
            width: px(600.0),
            split: px(300.0),
        });
        view.scroll.state.y.viewport = 20.0;
        view
    }

    fn header_top(view: &DiffView, file: u32) -> f64 {
        view.list
            .top_of(view.list.file_rows()[file as usize] as usize)
    }

    const HEADER: f64 = 30.0;

    #[test]
    fn the_pinned_header_follows_the_scroll_offset() {
        let view = view(Layout::Unified, 3, Vec::new());
        let list = &view.list;
        let (second, third) = (header_top(&view, 1), header_top(&view, 2));
        let at = |y: f64| pinned_at(list, y);
        // Nothing is pinned while a file's own header is at the top or below.
        assert_eq!(at(0.0), None);
        assert_eq!(at(second), None);
        // Inside the header, which is the row at the top: pinned in place.
        let pinned = at(5.0).expect("pinned");
        assert_eq!((pinned.file, pinned.row, pinned.top), (FileIx(0), 0, 0.0));
        // Through the file's code.
        assert_eq!(
            at(HEADER + 40.0).map(|p| (p.file, p.top)),
            Some((FileIx(0), 0.0))
        );
        // The next header reaches the pinned one: still at the top...
        assert_eq!(at(second - HEADER).map(|p| p.top), Some(0.0));
        // ...then pushes it up, a pixel at a time, until it is gone.
        assert_eq!(at(second - HEADER + 1.0).map(|p| p.top), Some(-1.0));
        assert_eq!(at(second - 10.0).map(|p| p.top), Some(-20.0));
        let nearly = at(second - 0.5).expect("pushed");
        assert_eq!((nearly.file, nearly.top), (FileIx(0), -(HEADER - 0.5)));
        // The next file takes over the moment its header is at the top.
        assert_eq!(at(second), None);
        assert_eq!(
            at(second + 1.0).map(|p| (p.file, p.top)),
            Some((FileIx(1), 0.0))
        );
        assert_eq!(at(third - 1.0).map(|p| p.file), Some(FileIx(1)));
        // The last file stays pinned to the end.
        let end = list.total_height();
        assert_eq!(
            at(end - 1.0).map(|p| (p.file, p.top)),
            Some((FileIx(2), 0.0))
        );
    }

    #[test]
    fn the_pinned_header_ignores_thread_blocks_and_new_heights() {
        let after = |line| Anchor::Line {
            file: FileIx(0),
            side: Side::New,
            line,
        };
        let blocks = vec![Block::new(after(11), 90.0), Block::new(after(13), 40.0)];
        let mut view = view(Layout::Split, 2, blocks);
        let second = header_top(&view, 1);
        assert!(second > 200.0, "the blocks made file 0 taller");
        let pinned = pinned_at(&view.list, second - 10.0).expect("pushed");
        assert_eq!((pinned.file, pinned.top), (FileIx(0), -20.0));
        // A taller header (a new style) moves the push-out zone with it.
        let mut style = test_style();
        style.file_header_height = px(50.0);
        assert!(view.apply_style(style));
        let second = header_top(&view, 1);
        let pinned = pinned_at(&view.list, second - 10.0).expect("pushed");
        assert_eq!((pinned.file, pinned.top), (FileIx(0), -40.0));
        assert_eq!(
            pinned_at(&view.list, second - 50.0).map(|p| p.top),
            Some(0.0)
        );
    }

    #[test]
    fn a_collapsed_file_pins_where_its_header_is() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.collapse_viewed = true;
        view.apply_viewed(FileIx(1), true);
        let second = header_top(&view, 1);
        // Inside the collapsed file's own header the next one is right
        // behind it, so the pinned copy is where the real one is.
        let pinned = pinned_at(&view.list, second + 10.0).expect("pinned");
        assert_eq!((pinned.file, pinned.top), (FileIx(1), -10.0));
    }

    #[test]
    fn the_current_file_is_the_one_at_the_top() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        assert_eq!(view.current_file(), Some(FileIx(0)));
        view.set_scroll_y(header_top(&view, 1) - 1.0);
        assert_eq!(view.current_file(), Some(FileIx(0)));
        view.set_scroll_y(header_top(&view, 1));
        assert_eq!(view.current_file(), Some(FileIx(1)));
        view.set_scroll_y(header_top(&view, 2) + 25.0);
        assert_eq!(view.current_file(), Some(FileIx(2)));
        let empty = DiffView::new(Diff::default(), Layout::Unified, test_style());
        assert_eq!(empty.current_file(), None);
    }

    /// An x inside the click target of the viewed checkbox.
    fn on_checkbox(view: &DiffView) -> Pixels {
        view.viewed_zone(view.columns.unwrap()).left + px(12.0)
    }

    #[test]
    fn a_press_on_the_checkbox_toggles_pinned_or_inline() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        let x = on_checkbox(&view);
        // Inline: the header of file 0 is at the top of the view.
        let toggle = Some(Some(DiffViewEvent::ToggleViewed(FileIx(0))));
        assert_eq!(view.press_header(point(x, px(10.0))), toggle);
        // Elsewhere on an inline header: nothing.
        assert_eq!(view.press_header(point(px(40.0), px(10.0))), None);
        // Pinned: scrolled into file 1's code, the header at the top is
        // file 1's, whatever rows are under it.
        view.set_scroll_y(header_top(&view, 1) + 70.0);
        let toggle = Some(Some(DiffViewEvent::ToggleViewed(FileIx(1))));
        assert_eq!(view.press_header(point(x, px(10.0))), toggle);
        // Below the pinned header it is code, not a header.
        assert_eq!(view.press_header(point(x, px(HEADER as f32 + 20.0))), None);
        // The view does not change its marks itself.
        assert!(!view.is_viewed(FileIx(0)) && !view.is_viewed(FileIx(1)));
    }

    #[test]
    fn a_press_on_the_pinned_path_scrolls_to_the_file() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.set_scroll_y(header_top(&view, 1) + 70.0);
        assert_eq!(view.press_header(point(px(40.0), px(10.0))), Some(None));
        assert_eq!(view.scroll_y(), header_top(&view, 1));
        assert_eq!(view.pinned(), None);
        // A pinned header that is being pushed out takes the press as well.
        view.set_scroll_y(header_top(&view, 2) - 10.0);
        let pinned = view.pinned().expect("pushed");
        assert_eq!(pinned.top, -20.0);
        assert!(view.on_header(point(px(40.0), px(5.0))));
        assert_eq!(view.press_header(point(px(40.0), px(5.0))), Some(None));
        assert_eq!(view.scroll_y(), header_top(&view, 1));
    }

    #[test]
    fn the_pinned_header_covers_hits_and_selection() {
        let mut view = view(Layout::Unified, 2, Vec::new());
        view.set_scroll_y(header_top(&view, 0) + HEADER + 3.0 * 20.0);
        let under = point(px(100.0), px(10.0));
        assert_eq!(view.hit_test(under), None);
        assert!(!view.press_text(under, 1, false));
        assert!(!view.is_selecting());
        let below = point(px(100.0), px(HEADER as f32 + 10.0));
        assert!(view.hit_test(below).is_some());
    }

    #[test]
    fn viewed_marks_are_the_apps_and_collapse_files_only_when_asked() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        let open = view.list.total_height();
        // File 0 is its header alone when collapsed: 6 code rows, a hunk
        // header and the expander after them less.
        let folded = open - 8.0 * 20.0;
        assert!(view.apply_viewed(FileIx(0), true));
        assert!(!view.apply_viewed(FileIx(0), true));
        assert!(!view.apply_viewed(FileIx(9), true));
        assert!(view.is_viewed(FileIx(0)));
        assert_eq!(view.list.total_height(), open, "off by default");
        // Turning it on collapses what is viewed already.
        assert!(view.apply_collapse_viewed(true));
        assert!(!view.apply_collapse_viewed(true));
        assert_eq!(view.list.total_height(), folded);
        assert!(view.list.is_collapsed(FileIx(0)) && !view.list.is_collapsed(FileIx(1)));
        // Unmarking opens it again, and marking another folds it.
        view.apply_viewed(FileIx(0), false);
        assert_eq!(view.list.total_height(), open);
        view.apply_viewed(FileIx(1), true);
        assert!(view.is_file_collapsed(FileIx(1)));
        assert_eq!(view.list.total_height(), folded);
        // Opened by hand, a viewed file stays open, with its mark.
        assert!(view.apply_collapsed(FileIx(1), false));
        assert!(view.is_viewed(FileIx(1)) && view.list.total_height() == open);
        assert!(!view.apply_viewed(FileIx(1), true));
        assert!(!view.is_file_collapsed(FileIx(1)));
        // All at once, by file: only the marks that change act; short lists
        // leave the rest unviewed.
        assert!(view.apply_viewed_all(vec![false, true, true]));
        assert!(!view.is_file_collapsed(FileIx(1)));
        assert!(view.is_file_collapsed(FileIx(2)));
        assert!(!view.apply_viewed_all(vec![false, true, true]));
        // Turned off, nothing already folded opens.
        assert!(!view.apply_collapse_viewed(false));
        assert!(view.is_file_collapsed(FileIx(2)));
        view.apply_viewed(FileIx(2), false);
        assert!(view.is_file_collapsed(FileIx(2)), "marks no longer act");
    }

    #[test]
    fn collapsing_keeps_the_top_line_or_goes_to_its_header() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.collapse_viewed = true;
        // The top row is in file 2; collapsing file 0 above it moves
        // nothing on screen.
        view.set_scroll_y(header_top(&view, 2) + 55.0);
        let at = |view: &DiffView| view.list.row_at(view.scroll_y());
        let top_line = view.list.rows()[at(&view).row];
        view.apply_viewed(FileIx(0), true);
        assert_eq!(view.list.rows()[at(&view).row], top_line);
        assert_eq!(at(&view).offset, 55.0 - HEADER - 20.0);
        // Collapsing the file the top is in puts its header there.
        view.apply_viewed(FileIx(2), true);
        assert_eq!(view.scroll_y(), header_top(&view, 2));
        assert_eq!(view.current_file(), Some(FileIx(2)));
    }

    #[test]
    fn show_file_restricts_the_rows_and_resets_the_scroll() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        let all = view.list.len();
        view.set_scroll_y(header_top(&view, 2) + 25.0);
        assert!(view.apply_show_file(Some(FileIx(1))));
        assert_eq!(view.shown_file(), Some(FileIx(1)));
        assert_eq!(view.list.len(), all / 3);
        assert_eq!(view.scroll_y(), 0.0);
        assert_eq!(view.current_file(), Some(FileIx(1)));
        // The sticky header and the checkbox work on the one file.
        view.set_scroll_y(HEADER + 45.0);
        assert_eq!(view.pinned().map(|p| p.file), Some(FileIx(1)));
        let toggle = Some(Some(DiffViewEvent::ToggleViewed(FileIx(1))));
        assert_eq!(
            view.press_header(point(on_checkbox(&view), px(5.0))),
            toggle
        );
        // Showing another file goes to its top; a file that is not there
        // changes nothing.
        assert!(view.apply_show_file(Some(FileIx(2))));
        assert_eq!(
            (view.scroll_y(), view.current_file()),
            (0.0, Some(FileIx(2)))
        );
        assert!(!view.apply_show_file(Some(FileIx(7))));
        // All files again, at the file that was shown.
        assert!(view.apply_show_file(None));
        assert_eq!(view.list.len(), all);
        assert_eq!(view.scroll_y(), header_top(&view, 2));
    }

    #[test]
    fn show_file_hides_the_blocks_of_other_files() {
        let anchor = |file| Anchor::Line {
            file: FileIx(file),
            side: Side::New,
            line: 11,
        };
        let blocks = vec![Block::new(anchor(0), 50.0), Block::new(anchor(1), 70.0)];
        let mut view = view(Layout::Unified, 2, blocks);
        let (first, second) = (diff::BlockId(0), diff::BlockId(1));
        assert!(view.apply_show_file(Some(FileIx(1))));
        assert!(!view.list.shows_block(first));
        assert!(view.list.shows_block(second));
        view.resize_blocks(&[(first, 90.0), (second, 80.0)]);
        view.apply_show_file(None);
        assert_eq!(view.list.row_height(view.list.block_row(first)), 90.0);
        assert_eq!(view.list.row_height(view.list.block_row(second)), 80.0);
    }

    #[test]
    fn the_context_mode_keeps_the_top_line() {
        let mut view = view(Layout::Unified, 2, Vec::new());
        let line_of = |view: &DiffView| {
            let top = view.list.row_at(view.scroll_y()).row;
            row_line(view.list.rows()[top], Side::New)
        };
        // New line 10 is the first row of file 0's hunk.
        let row = view.list.file_rows()[0] as usize + 2;
        view.set_scroll_y(view.list.top_of(row) + 4.0);
        assert_eq!(line_of(&view), Some(10));
        view.reproject(|list| list.set_context(None, ContextMode::Full));
        assert_eq!(view.context(FileIx(1)), ContextMode::Full);
        assert_eq!(line_of(&view), Some(10));
        assert_eq!(view.list.row_at(view.scroll_y()).offset, 4.0);
        // Back to hunks from a line that is in none: the next one shown.
        let row = view.list.file_rows()[0] as usize + 1 + 5;
        view.set_scroll_y(view.list.top_of(row));
        assert_eq!(line_of(&view), Some(5));
        view.reproject(|list| list.set_context(None, ContextMode::Hunks));
        assert_eq!(line_of(&view), Some(10));
        // One file only.
        view.reproject(|list| list.set_context(Some(FileIx(1)), ContextMode::Full));
        assert_eq!(view.context(FileIx(0)), ContextMode::Hunks);
        assert_eq!(view.context(FileIx(1)), ContextMode::Full);
    }

    /// An x on the empty part of the header, between the path and the
    /// checkbox.
    const EMPTY: f32 = 300.0;

    #[test]
    fn the_chevron_and_the_empty_header_space_toggle_the_body_the_checkbox_does_not() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        let open = view.list.total_height();
        let y = px(10.0);
        for (x, name) in [(10.0, "chevron"), (EMPTY, "empty space")] {
            let collapse = Some(Some(DiffViewEvent::FileCollapsed(FileIx(0), true)));
            assert_eq!(view.press_header(point(px(x), y)), collapse, "{name}");
            assert!(view.is_file_collapsed(FileIx(0)));
            assert!(view.list.is_collapsed(FileIx(0)));
            let expand = Some(Some(DiffViewEvent::FileCollapsed(FileIx(0), false)));
            assert_eq!(view.press_header(point(px(x), y)), expand, "{name}");
            assert_eq!(view.list.total_height(), open);
        }
        // The checkbox marks the file viewed and folds nothing itself.
        let toggle = Some(Some(DiffViewEvent::ToggleViewed(FileIx(0))));
        assert_eq!(view.press_header(point(on_checkbox(&view), y)), toggle);
        assert!(!view.is_file_collapsed(FileIx(0)));
        assert_eq!(view.list.total_height(), open);
        // An inline path does nothing.
        assert_eq!(view.press_header(point(px(40.0), y)), None);
        assert!(!view.is_file_collapsed(FileIx(0)));
    }

    #[test]
    fn collapsing_an_inline_header_leaves_it_where_it_is_on_screen() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.scroll.state.y.viewport = 300.0;
        // File 1's header is 100 px down; the top row is in file 0's body.
        view.set_scroll_y(header_top(&view, 1) - 100.0);
        let screen = |view: &DiffView, file| header_top(view, file) - view.scroll_y();
        assert_eq!(screen(&view, 1), 100.0);
        let press = view.press_header(point(px(EMPTY), px(105.0)));
        assert_eq!(
            press,
            Some(Some(DiffViewEvent::FileCollapsed(FileIx(1), true)))
        );
        assert_eq!(screen(&view, 1), 100.0, "the header stays");
        assert_eq!(screen(&view, 2), 100.0 + HEADER, "what follows moves up");
        // Opening it again does the same.
        view.press_header(point(px(EMPTY), px(105.0)));
        assert_eq!(screen(&view, 1), 100.0);
    }

    #[test]
    fn collapsing_the_pinned_header_brings_the_header_to_the_top() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.set_scroll_y(header_top(&view, 1) + 70.0);
        assert_eq!(view.pinned().map(|p| p.file), Some(FileIx(1)));
        // The chevron of the pinned header.
        let press = view.press_header(point(px(10.0), px(10.0)));
        assert_eq!(
            press,
            Some(Some(DiffViewEvent::FileCollapsed(FileIx(1), true)))
        );
        assert_eq!(view.scroll_y(), header_top(&view, 1));
        assert_eq!(view.current_file(), Some(FileIx(1)));
        // Open again: the header is still at the top.
        view.press_header(point(px(EMPTY), px(10.0)));
        assert!(!view.is_file_collapsed(FileIx(1)));
        assert_eq!(view.scroll_y(), header_top(&view, 1));
    }

    #[test]
    fn collapsed_files_stay_collapsed_through_style_changes_and_scrolling() {
        let mut view = view(Layout::Split, 3, Vec::new());
        assert!(view.apply_collapsed(FileIx(1), true));
        assert!(!view.apply_collapsed(FileIx(1), true));
        assert!(!view.apply_collapsed(FileIx(8), true));
        let mut style = test_style();
        style.line_height = px(24.0);
        assert!(view.apply_style(style));
        view.set_scroll_y(40.0);
        view.set_scroll_y(0.0);
        assert!(view.is_file_collapsed(FileIx(1)));
        assert!(view.list.is_collapsed(FileIx(1)) && !view.list.is_collapsed(FileIx(0)));
        // Switching layout re-projects the rows and keeps the fold.
        view.set_layout(Layout::Unified);
        assert!(view.list.is_collapsed(FileIx(1)));
    }

    #[test]
    fn threads_in_a_collapsed_file_are_hidden_with_it() {
        let anchor = |file| Anchor::Line {
            file: FileIx(file),
            side: Side::New,
            line: 11,
        };
        let blocks = vec![Block::new(anchor(0), 50.0), Block::new(anchor(1), 70.0)];
        let mut view = view(Layout::Unified, 2, blocks);
        let (first, second) = (diff::BlockId(0), diff::BlockId(1));
        assert!(view.list.shows_block(first) && view.list.shows_block(second));
        view.apply_collapsed(FileIx(1), true);
        assert!(view.list.shows_block(first));
        assert!(!view.list.shows_block(second));
        view.apply_collapsed(FileIx(1), false);
        assert!(view.list.shows_block(second));
    }

    #[test]
    fn collapse_all_and_expand_all_report_the_files_that_changed() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        let open = view.list.total_height();
        view.apply_collapsed(FileIx(1), true);
        let changed = view.apply_all_collapsed(true);
        assert_eq!(changed, [FileIx(0), FileIx(2)]);
        assert_eq!(view.list.total_height(), 3.0 * HEADER);
        assert!(view.apply_all_collapsed(true).is_empty());
        assert_eq!(view.apply_all_collapsed(false).len(), 3);
        assert_eq!(view.list.total_height(), open);
        assert!(view.apply_all_collapsed(false).is_empty());
    }

    #[test]
    fn header_slots_move_the_checkbox_and_cut_the_path() {
        let mut view = view(Layout::Unified, 2, Vec::new());
        let plain = view.header_place(FileIx(0), false).expect("place");
        // f0.rs: five columns of 8 after the chevron.
        let (left, right) = view.path_span(FileIx(0)).expect("path");
        assert_eq!(right - left, px(40.0));
        assert_eq!(plain.after_path, right);
        assert_eq!(plain.trailing, px(600.0), "no trailing room");
        let slots = HeaderSlots {
            after_path: px(60.0),
            stats: px(100.0),
            trailing: px(64.0),
        };
        view.set_header_renderer(
            slots,
            std::rc::Rc::new(|_, _, _| gpui::IntoElement::into_any_element(gpui::Empty)),
        );
        let place = view.header_place(FileIx(1), true).expect("place");
        assert!(place.pinned);
        assert_eq!(place.trailing, px(600.0 - 64.0));
        let zone = view.viewed_zone(view.columns.unwrap());
        assert_eq!(zone.right, place.trailing);
        assert_eq!(place.size, size(px(600.0), px(HEADER as f32)));
        // The checkbox moved left with its zone; the trailing room folds.
        let y = px(10.0);
        let toggle = Some(Some(DiffViewEvent::ToggleViewed(FileIx(0))));
        assert_eq!(view.press_header(point(on_checkbox(&view), y)), toggle);
        let trailing = point(px(600.0 - 20.0), y);
        let fold = Some(Some(DiffViewEvent::FileCollapsed(FileIx(0), true)));
        assert_eq!(view.press_header(trailing), fold);
        // A path longer than the room left is cut where the slots start.
        let long = "d/".repeat(60) + "x.rs";
        let mut view = DiffView::new(
            Diff {
                files: vec![file(&long)],
                blocks: Vec::new(),
            },
            Layout::Unified,
            test_style(),
        );
        view.columns = self::view(Layout::Unified, 1, Vec::new()).columns;
        view.set_header_renderer(
            slots,
            std::rc::Rc::new(|_, _, _| gpui::IntoElement::into_any_element(gpui::Empty)),
        );
        let (_, right) = view.path_span(FileIx(0)).expect("path");
        let clip = view.viewed_zone(view.columns.unwrap()).left - px(160.0);
        assert_eq!(right, clip);
        assert_eq!(
            view.header_place(FileIx(0), false).unwrap().after_path,
            clip
        );
    }

    #[test]
    fn the_current_file_is_what_the_toggle_key_flips() {
        let mut view = view(Layout::Unified, 3, Vec::new());
        view.set_scroll_y(header_top(&view, 1) + 55.0);
        let file = view.current_file().expect("file");
        assert_eq!(file, FileIx(1));
        assert!(view.apply_collapsed(file, !view.is_file_collapsed(file)));
        assert!(view.is_file_collapsed(FileIx(1)));
        assert_eq!(view.scroll_y(), header_top(&view, 1));
    }
}
