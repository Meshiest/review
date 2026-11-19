//! Selecting and copying text in the diff view: the press, the drag with its
//! scrolling at the edges, and the keys.

use diff::{Layout, Side};
use gpui::{App, ClipboardItem, Context, Pixels, Point, px};
use scroll::{AutoscrollConfig, Time, now};

use super::{Columns, DiffView, hit_at};
use crate::select::{
    Caret, CodeLine, Selection, Stretch, Unit, blob, char_count, code_line, locate, word_range,
};

/// A press-and-drag in progress.
pub(super) struct TextDrag {
    /// The pointer, relative to the view's top left; the drag keeps
    /// following it while the view scrolls under it.
    pointer: Point<Pixels>,
    /// When the view last scrolled for it.
    last: Time,
}

/// Pixels per second to scroll at when the pointer is `distance` outside the
/// view: from the first pixel on, faster the further out.
fn edge_speed(distance: f64) -> f64 {
    AutoscrollConfig {
        dead_zone: 0.0,
        ..AutoscrollConfig::default()
    }
    .speed(distance)
}

impl DiffView {
    /// True when some text is selected.
    pub fn has_selection(&self) -> bool {
        self.selection.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// True between a press that began a selection and its release.
    pub fn is_selecting(&self) -> bool {
        self.text_drag.is_some()
    }

    /// The selected source text, or `None` when nothing is selected.
    ///
    /// Each line is the line of its file as it is, tabs included, without
    /// line numbers, `+` and `-` markers, hunk headers or thread cards.
    /// Lines are joined with `\n`, or with `\r\n` when the file they come
    /// from uses it. A split view selection takes lines from one side only.
    #[must_use]
    pub fn copy_selection(&self) -> Option<String> {
        self.selection.as_ref()?.text(&self.list)
    }

    /// Puts [`DiffView::copy_selection`] on the clipboard; false when there
    /// is nothing selected.
    pub fn copy_to_clipboard(&self, cx: &mut App) -> bool {
        let Some(text) = self.copy_selection() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    /// Drops the selection; whether there was one.
    pub fn clear_selection(&mut self) -> bool {
        self.text_drag = None;
        self.selection.take().is_some()
    }

    /// The x of the left of the code of `side`, relative to the view.
    #[must_use]
    pub fn text_left(&self, side: Side) -> Option<Pixels> {
        Some(self.columns?.text_left(self.list.layout(), side))
    }

    /// The width of one column of code.
    #[must_use]
    pub fn char_width(&self) -> Option<Pixels> {
        Some(self.columns?.ch)
    }

    /// A left press at `point` (relative to the view's top left): one click
    /// starts a selection, two select the word, three or more the line, and
    /// `extend` (Shift) moves the end of the selection there is. A press on
    /// the line numbers, a header, a thread card or past the content starts
    /// nothing and drops the selection. Returns whether the view must redraw.
    pub fn press_text(&mut self, point: Point<Pixels>, clicks: usize, extend: bool) -> bool {
        let Some(columns) = self.columns else {
            return false;
        };
        if self.on_header(point) || self.on_divider(point) {
            return false;
        }
        self.text_drag = None;
        let layout = self.list.layout();
        let hit = hit_at(&self.list, self.scroll_y(), columns, point);
        let Some((row, side, _)) = hit.filter(|&(_, side, _)| {
            point.x - columns.pane_left(layout, side) >= columns.gutter(layout)
        }) else {
            return self.selection.take().is_some();
        };
        let drag = TextDrag {
            pointer: point,
            last: now(),
        };
        if extend && clicks <= 1 && self.selection.is_some() {
            self.text_drag = Some(drag);
            return self.drag_text(point);
        }
        let lock = (layout == Layout::Split).then_some(side);
        let Some(at) = code_line(self.list.rows()[row], lock) else {
            return self.selection.take().is_some();
        };
        let unit = Unit::of_clicks(clicks);
        let anchor = self.stretch(row, at, point.x, columns, unit);
        self.selection = Some(Selection::new(anchor, unit, lock));
        self.text_drag = Some(drag);
        true
    }

    /// The pointer moved to `point` with the button down: moves the end of
    /// the selection to it. A point above or below the view counts as its
    /// first or last row; [`DiffView`] scrolls on its own meanwhile.
    pub fn drag_text(&mut self, point: Point<Pixels>) -> bool {
        let Some(drag) = self.text_drag.as_mut() else {
            return false;
        };
        drag.pointer = point;
        let Some(head) = self.head_at(point) else {
            return false;
        };
        let Some(selection) = self.selection.as_mut() else {
            return false;
        };
        selection.drag(head);
        true
    }

    /// The button went up: ends the drag, and drops a selection of nothing,
    /// which is what a plain click leaves. Returns whether a drag ended.
    pub fn release_text(&mut self) -> bool {
        let was = self.text_drag.take().is_some();
        if self.selection.as_ref().is_some_and(Selection::is_empty) {
            self.selection = None;
        }
        was
    }

    /// Scrolls for a drag whose pointer is outside the view, by how far out
    /// it is and the time since the last step, then moves the selection to
    /// the pointer again. True when the view scrolled and wants another
    /// frame.
    pub(super) fn edge_scroll(&mut self, now: Time) -> bool {
        let Some(drag) = self.text_drag.as_mut() else {
            return false;
        };
        let step = now
            .saturating_sub(drag.last)
            .min(AutoscrollConfig::default().max_step);
        drag.last = now;
        let pointer = drag.pointer;
        let (y, viewport) = (f64::from(pointer.y), self.scroll.state.y.viewport);
        let past = if y < 0.0 {
            y
        } else if y > viewport {
            y - viewport
        } else {
            return false;
        };
        let dy = edge_speed(past.abs()) * step.as_secs_f64() * past.signum();
        let scrolled = self.scroll_by(dy);
        if scrolled {
            self.drag_text(pointer);
        }
        scrolled
    }

    /// Finds the selection's rows again after the rows were projected anew.
    pub(super) fn rebase_selection(&mut self) {
        if let Some(selection) = &mut self.selection
            && !selection.rebase(&self.list)
        {
            self.clear_selection();
        }
    }

    /// Starts watching the keys that belong to a selection: Escape clears it,
    /// and Ctrl+C (Cmd+C) copies it unless a binding took the key, as a text
    /// box's copy does.
    pub(super) fn watch_keys(&mut self, cx: &mut Context<Self>) {
        if self.keys.is_some() {
            return;
        }
        self.keys = Some(cx.observe_keystrokes(|view, event, _, cx| {
            if view.selection.is_none() {
                return;
            }
            let stroke = &event.keystroke;
            if stroke.key == "escape" {
                view.clear_selection();
                cx.notify();
            } else if stroke.key == "c" && stroke.modifiers.secondary(cx) && event.action.is_none()
            {
                view.copy_to_clipboard(cx);
            }
        }));
    }

    /// The stretch of `unit` at the pointer's x on code line `at`, shown on
    /// `row`.
    fn stretch(
        &self,
        row: usize,
        at: CodeLine,
        x: Pixels,
        columns: Columns,
        unit: Unit,
    ) -> Stretch {
        let line = blob(self.list.diff(), at).line(at.line);
        let left = columns.text_left(self.list.layout(), at.side);
        let column = if columns.ch > px(0.0) {
            (x - left) / columns.ch
        } else {
            0.0
        };
        let found = locate(&line, self.style.tab_width, column);
        let caret = |col| Caret { at, col, row };
        match unit {
            Unit::Char => (caret(found.boundary), caret(found.boundary)),
            Unit::Word => {
                let (start, end) = word_range(&line, found.cell);
                (caret(start), caret(end))
            }
            Unit::Line => (caret(0), caret(char_count(&line))),
        }
    }

    /// Where the selection's moving end goes for a pointer at `point`.
    ///
    /// Over a header, a thread card or past the last row there is no line to
    /// point at: the end goes to the end of the nearest line above when the
    /// pointer is below the press, and to the start of the nearest below
    /// when it is above, so those rows are crossed without selecting them.
    fn head_at(&self, point: Point<Pixels>) -> Option<Stretch> {
        let columns = self.columns?;
        let selection = self.selection.as_ref()?;
        let (lock, unit) = (selection.lock(), selection.unit());
        let list = &self.list;
        let viewport = self.scroll.state.y.viewport;
        let y = f64::from(point.y).clamp(0.0, (viewport - 1.0).max(0.0));
        let at = self.scroll_y() + y;
        let past_end = at >= list.total_height();
        let row = if past_end {
            list.len().checked_sub(1)?
        } else {
            list.row_at(at).row
        };
        if !past_end && let Some(line) = code_line(list.rows()[row], lock) {
            return Some(self.stretch(row, line, point.x, columns, unit));
        }
        let rows = list.rows();
        if past_end || row > selection.anchor_row() {
            let (row, at) = (0..=row)
                .rev()
                .find_map(|r| Some((r, code_line(rows[r], lock)?)))?;
            let line = blob(list.diff(), at).line(at.line);
            let caret = Caret {
                at,
                col: char_count(&line),
                row,
            };
            Some((caret, caret))
        } else {
            let (row, at) = (row..rows.len()).find_map(|r| Some((r, code_line(rows[r], lock)?)))?;
            let caret = Caret { at, col: 0, row };
            Some((caret, caret))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use diff::{Anchor, Block, BlockId, Diff, FileDiff, FileIx, Hunk, Segment, Text};
    use gpui::{Bounds, point, px};

    use super::*;
    use crate::view::RowPainter;
    use crate::view::tests::test_style;

    const OLD: &str = "one two\n\tthree\nfour five\ngap\ngap2\nsix\nseven\n";
    const NEW: &str = "one two\n\tTHREE\nfour five\ngap\ngap2\nsix\neight\n";

    fn file(old: &str, new: &str, segments: Vec<Segment>) -> FileDiff {
        FileDiff {
            path: "a.rs".into(),
            old: Text::new(old),
            new: Text::new(new),
            hunks: vec![Hunk {
                old_start: 0,
                new_start: 0,
                segments,
            }],
        }
    }

    /// Unified rows: file header, hunk header, `one two`, `-\tthree`,
    /// `+\tTHREE`, `four five`, hunk header, `six`, `-seven`, `+eight`.
    /// Split rows: header, hunk header, then one pair per change.
    fn two_hunks(layout: Layout, blocks: Vec<Block>) -> DiffView {
        let change = Segment::Change {
            removed: 1,
            added: 1,
        };
        let mut file = file(
            OLD,
            NEW,
            vec![Segment::Context(1), change, Segment::Context(1)],
        );
        file.hunks.push(Hunk {
            old_start: 5,
            new_start: 5,
            segments: vec![Segment::Context(1), change],
        });
        shown(file, layout, blocks)
    }

    fn shown(file: FileDiff, layout: Layout, blocks: Vec<Block>) -> DiffView {
        let diff = Diff {
            files: vec![file],
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
        view.scroll.state.y.viewport = 400.0;
        view
    }

    fn after_line_1() -> Anchor {
        Anchor::Line {
            file: FileIx(0),
            side: Side::New,
            line: 1,
        }
    }

    /// The point `column` display columns into the code of `side` on `row`,
    /// at the middle of the row.
    fn at(view: &DiffView, row: usize, side: Side, column: f32) -> Point<Pixels> {
        let y = view.list.top_of(row) - view.scroll_y() + view.list.row_height(row) / 2.0;
        let x = f32::from(view.text_left(side).expect("columns")) + column * 8.0;
        point(px(x), px(y as f32))
    }

    fn drag(view: &mut DiffView, from: Point<Pixels>, to: Point<Pixels>) {
        assert!(view.press_text(from, 1, false));
        view.drag_text(to);
        view.release_text();
    }

    #[test]
    fn a_drag_across_unified_rows_copies_each_kind_of_line() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        // From `two` in `one two` to after `four` in `four five`.
        let (from, to) = (at(&view, 2, Side::New, 4.0), at(&view, 5, Side::New, 4.0));
        drag(&mut view, from, to);
        assert!(view.has_selection());
        let text = "two\n\tthree\n\tTHREE\nfour";
        assert_eq!(view.copy_selection().as_deref(), Some(text));
        // The other way round selects the same.
        let mut up = two_hunks(Layout::Unified, Vec::new());
        let (from, to) = (at(&up, 5, Side::New, 4.0), at(&up, 2, Side::New, 4.0));
        drag(&mut up, from, to);
        assert_eq!(up.copy_selection().as_deref(), Some(text));
    }

    #[test]
    fn a_split_drag_takes_one_side_whatever_the_pointer_crosses() {
        let mut view = two_hunks(Layout::Split, Vec::new());
        // Start after the tab on the old side; end in the new half.
        let to = at(&view, 7, Side::New, 2.0);
        let (from, to) = (at(&view, 3, Side::Old, 4.0), to);
        drag(&mut view, from, to);
        assert_eq!(
            view.copy_selection().as_deref(),
            Some("three\nfour five\nsix\nseven")
        );
        // From the new side to the left half: the new lines, not the old.
        let to = at(&view, 7, Side::Old, 1.0);
        let (from, to) = (at(&view, 3, Side::New, 4.0), to);
        drag(&mut view, from, to);
        assert_eq!(
            view.copy_selection().as_deref(),
            Some("THREE\nfour five\nsix\n")
        );
    }

    #[test]
    fn a_double_click_selects_a_word_and_a_triple_click_the_line() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let two = at(&view, 2, Side::New, 5.3);
        assert!(view.press_text(two, 2, false));
        view.release_text();
        assert_eq!(view.copy_selection().as_deref(), Some("two"));
        assert!(view.press_text(two, 3, false));
        view.release_text();
        assert_eq!(view.copy_selection().as_deref(), Some("one two"));
        // A tab is a run of blanks of its own.
        let tab = at(&view, 3, Side::Old, 1.0);
        view.press_text(tab, 2, false);
        view.release_text();
        assert_eq!(view.copy_selection().as_deref(), Some("\t"));
        // Dragging on from a double click grows by words.
        view.press_text(two, 2, false);
        view.drag_text(at(&view, 5, Side::New, 2.0));
        view.release_text();
        assert_eq!(
            view.copy_selection().as_deref(),
            Some("two\n\tthree\n\tTHREE\nfour")
        );
        // A double click on nothing selects nothing, and clears what was.
        let past = at(&view, 2, Side::New, 30.0);
        assert!(view.press_text(past, 2, false));
        view.release_text();
        assert!(!view.has_selection());
        assert_eq!(view.copy_selection(), None);
    }

    #[test]
    fn shift_click_moves_the_end_of_the_selection() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let (from, to) = (at(&view, 2, Side::New, 1.0), at(&view, 2, Side::New, 3.0));
        drag(&mut view, from, to);
        assert_eq!(view.copy_selection().as_deref(), Some("ne"));
        assert!(view.press_text(at(&view, 5, Side::New, 2.0), 1, true));
        view.release_text();
        assert_eq!(
            view.copy_selection().as_deref(),
            Some("ne two\n\tthree\n\tTHREE\nfo")
        );
    }

    #[test]
    fn a_copy_has_no_numbers_markers_or_headers() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let to = at(&view, 9, Side::New, 99.0);
        let (from, to) = (at(&view, 2, Side::New, 0.0), to);
        drag(&mut view, from, to);
        let text = view.copy_selection().expect("selected");
        assert_eq!(
            text,
            "one two\n\tthree\n\tTHREE\nfour five\nsix\nseven\neight"
        );
        assert!(!text.contains("@@") && !text.contains("a.rs"));
        // Pulling the end onto the header between the hunks stops at the
        // line before it.
        let mut over = two_hunks(Layout::Unified, Vec::new());
        let to = at(&over, 6, Side::New, 3.0);
        let (from, to) = (at(&over, 2, Side::New, 0.0), to);
        drag(&mut over, from, to);
        assert_eq!(
            over.copy_selection().as_deref(),
            Some("one two\n\tthree\n\tTHREE\nfour five")
        );
        // So does pulling it below the last row.
        let mut past = two_hunks(Layout::Unified, Vec::new());
        let from = at(&past, 8, Side::Old, 2.0);
        drag(&mut past, from, point(px(100.0), px(10_000.0)));
        assert_eq!(past.copy_selection().as_deref(), Some("ven\neight"));
        // Pulling it up over the header selects from the line after it.
        let mut up = two_hunks(Layout::Unified, Vec::new());
        let to = at(&up, 6, Side::New, 3.0);
        let (from, to) = (at(&up, 8, Side::Old, 2.0), to);
        drag(&mut up, from, to);
        assert_eq!(up.copy_selection().as_deref(), Some("six\nse"));
    }

    #[test]
    fn lines_keep_the_line_ending_of_their_file() {
        let text = "a\r\nb\r\nc\r\n";
        let lines = file(text, text, vec![Segment::Context(3)]);
        let mut view = shown(lines, Layout::Unified, Vec::new());
        let to = at(&view, 4, Side::New, 1.0);
        let (from, to) = (at(&view, 2, Side::New, 0.0), to);
        drag(&mut view, from, to);
        assert_eq!(view.copy_selection().as_deref(), Some("a\r\nb\r\nc"));
    }

    #[test]
    fn the_selection_survives_a_new_style() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let to = at(&view, 4, Side::New, 7.0);
        let (from, to) = (at(&view, 3, Side::Old, 4.0), to);
        drag(&mut view, from, to);
        let copied = view.copy_selection();
        assert_eq!(copied.as_deref(), Some("three\n\tTHR"));
        let mut style = test_style();
        style.tab_width = 8;
        style.line_height = px(24.0);
        style.selection.a = 0.5;
        assert!(view.apply_style(style));
        assert!(view.has_selection());
        assert_eq!(view.copy_selection(), copied);
    }

    #[test]
    fn a_press_in_the_gutter_starts_no_selection() {
        for layout in [Layout::Unified, Layout::Split] {
            let mut view = two_hunks(layout, Vec::new());
            for side in [Side::Old, Side::New] {
                let left = f32::from(view.pane_left(side).expect("columns"));
                let gutter = point(px(left + 5.0), at(&view, 3, side, 0.0).y);
                assert!(!view.press_text(gutter, 1, false), "{layout:?} {side:?}");
                assert!(!view.is_selecting());
                assert!(!view.drag_text(at(&view, 6, side, 3.0)));
                assert_eq!(view.copy_selection(), None);
            }
            // It clears a selection, as any click elsewhere does.
            let (from, to) = (at(&view, 2, Side::Old, 0.0), at(&view, 2, Side::Old, 3.0));
            drag(&mut view, from, to);
            assert!(view.has_selection());
            let gutter = point(px(5.0), at(&view, 4, Side::Old, 0.0).y);
            assert!(view.press_text(gutter, 1, false));
            assert!(!view.has_selection());
        }
    }

    #[test]
    fn a_press_on_a_card_is_not_the_diffs() {
        let blocks = vec![Block::new(after_line_1(), 60.0)];
        let mut view = two_hunks(Layout::Unified, blocks);
        let card = view.list.block_row(BlockId(0));
        let y = view.list.top_of(card) as f32 + 20.0;
        let in_card = point(px(300.0), px(y));
        assert!(!view.press_text(in_card, 1, false));
        assert!(!view.is_selecting());
        // Dragging over a card selects what is on both sides of it.
        let from = at(&view, 2, Side::New, 0.0);
        assert!(view.press_text(from, 1, false));
        view.drag_text(in_card);
        view.drag_text(at(&view, card + 1, Side::New, 4.0));
        view.release_text();
        assert_eq!(
            view.copy_selection().as_deref(),
            Some("one two\n\tthree\n\tTHREE\nfour")
        );
        // A click on a card drops the selection.
        assert!(view.press_text(in_card, 1, false));
        assert!(!view.has_selection());
    }

    #[test]
    fn rows_moving_under_a_selection_do_not_move_it() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let to = at(&view, 5, Side::New, 4.0);
        let (from, to) = (at(&view, 4, Side::New, 4.0), to);
        drag(&mut view, from, to);
        let copied = view.copy_selection();
        assert_eq!(copied.as_deref(), Some("THREE\nfour"));
        view.set_blocks(vec![Block::new(after_line_1(), 60.0)], Vec::new());
        assert_eq!(view.copy_selection(), copied);
        view.set_blocks(Vec::new(), Vec::new());
        assert_eq!(view.copy_selection(), copied);
        // A layout switch re-projects the rows into pairs: nothing is kept.
        view.set_layout(Layout::Split);
        assert!(!view.has_selection());
    }

    #[test]
    fn clearing_and_the_copy_of_nothing() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        assert_eq!(view.copy_selection(), None);
        assert!(!view.clear_selection());
        let (from, to) = (at(&view, 2, Side::New, 0.0), at(&view, 2, Side::New, 3.0));
        drag(&mut view, from, to);
        assert_eq!(view.copy_selection().as_deref(), Some("one"));
        assert!(view.clear_selection());
        assert_eq!(view.copy_selection(), None);
    }

    /// 200 context lines `l0` to `l199`, five rows tall on screen.
    fn tall() -> DiffView {
        let text: String = (0..200).map(|i| format!("l{i}\n")).collect();
        let lines = file(&text, &text, vec![Segment::Context(200)]);
        let mut view = shown(lines, Layout::Unified, Vec::new());
        view.scroll.state.y.viewport = 100.0;
        view.set_scroll_y(1000.0);
        view
    }

    #[test]
    fn a_drag_past_the_edge_scrolls_and_keeps_selecting() {
        let mut view = tall();
        let t = now();
        assert!(view.press_text(point(px(70.0), px(50.0)), 1, false));
        let pressed = view.list.row_at(view.scroll_y() + 50.0).row;
        view.drag_text(point(px(70.0), px(-30.0)));
        let before = view.scroll_y();
        assert!(view.edge_scroll(t + Duration::from_secs(1)));
        assert!(view.scroll_y() < before, "scrolled up");
        let lines = view.copy_selection().expect("selected").lines().count();
        assert!(lines >= 3, "{lines} lines");
        let selection = view.selection.as_ref().expect("selection");
        assert_eq!(selection.anchor_row(), pressed, "still from the press");
        // Back inside, it stops.
        view.drag_text(point(px(70.0), px(60.0)));
        let held = view.scroll_y();
        assert!(!view.edge_scroll(t + Duration::from_secs(2)));
        assert_eq!(view.scroll_y(), held);
        // Below the view it scrolls down, and a release ends it.
        view.drag_text(point(px(70.0), px(160.0)));
        assert!(view.edge_scroll(t + Duration::from_secs(3)));
        assert!(view.scroll_y() > held);
        view.release_text();
        assert!(!view.edge_scroll(t + Duration::from_secs(4)));
    }

    #[test]
    fn the_selection_paints_the_selected_columns_of_each_line() {
        let mut view = two_hunks(Layout::Unified, Vec::new());
        let to = at(&view, 4, Side::New, 6.0);
        let (from, to) = (at(&view, 3, Side::Old, 5.0), to);
        drag(&mut view, from, to);
        let painter = RowPainter {
            view: &view,
            bounds: Bounds::default(),
            columns: view.columns.expect("columns"),
        };
        let code = |row: usize, side, line, right: f32| {
            let at = CodeLine {
                file: FileIx(0),
                side,
                line,
            };
            let origin = point(px(64.0), px(0.0));
            painter.selected(row, at, origin, px(20.0), px(right))
        };
        // `\tthree` from `t` (column 4... the press at column 5 is nearest
        // the gap after `t`) to its end, and the half column marking the
        // line break.
        let removed = code(3, Side::Old, 1, 600.0).expect("painted");
        assert_eq!(removed.origin.x, px(64.0 + 5.0 * 8.0));
        assert_eq!(removed.size.width, px(4.0 * 8.0 + 4.0));
        // `\tTHREE` from its start to column 6.
        let added = code(4, Side::New, 1, 600.0).expect("painted");
        assert_eq!(added.origin.x, px(64.0));
        assert_eq!(added.size.width, px(8.0 * 6.0));
        assert_eq!(code(2, Side::New, 0, 600.0), None);
        assert_eq!(code(5, Side::New, 2, 600.0), None);
        // Cut at the right edge of the pane.
        let narrow = code(3, Side::Old, 1, 120.0).expect("painted");
        assert_eq!(narrow.size.width, px(120.0 - 64.0 - 5.0 * 8.0));
    }

    #[test]
    fn a_split_selection_paints_only_its_side() {
        let mut view = two_hunks(Layout::Split, Vec::new());
        let to = at(&view, 4, Side::Old, 3.0);
        let (from, to) = (at(&view, 3, Side::Old, 4.0), to);
        drag(&mut view, from, to);
        let painter = RowPainter {
            view: &view,
            bounds: Bounds::default(),
            columns: view.columns.expect("columns"),
        };
        let paint = |side| {
            let at = CodeLine {
                file: FileIx(0),
                side,
                line: 1,
            };
            painter.selected(3, at, point(px(40.0), px(0.0)), px(20.0), px(300.0))
        };
        assert!(paint(Side::Old).is_some());
        assert_eq!(paint(Side::New), None);
    }
}
