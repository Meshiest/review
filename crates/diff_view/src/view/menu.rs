//! What a right-click means: the target the app builds its context menu for,
//! and the line helpers the menu's actions need.

use std::ops::RangeInclusive;

use diff::{FileIx, Layout, Row, Side};
use gpui::{Pixels, Point};

use super::DiffView;
use crate::select::code_line;

/// What a right-click landed on. Lines are 0-based, as in the diff model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextTarget {
    /// A code row; in split view, the half under the pointer. In unified a
    /// context or added row names its new line and a removed row its old one.
    Line {
        file: FileIx,
        side: Side,
        line: u32,
        path: String,
    },
    /// The click was inside the text selection: its first and last line
    /// (inclusive) on `side`. A selection across files takes the first
    /// file's lines; across removed and added lines in unified, the added
    /// ones (the removed ones when there are none).
    Range {
        file: FileIx,
        side: Side,
        start: u32,
        end: u32,
        path: String,
    },
    /// A file header, inline or pinned.
    FileHeader { file: FileIx, path: String },
    /// A hunk header, by where the hunk starts on each side.
    Hunk {
        file: FileIx,
        old_start: u32,
        new_start: u32,
    },
}

/// The lines a selection covers, in one file.
struct SelectedLines {
    file: FileIx,
    side: Side,
    start: u32,
    end: u32,
    /// The rows of the first and the last of those lines.
    rows: RangeInclusive<usize>,
    /// Split view: the only side whose rows are selected.
    lock: Option<Side>,
}

impl SelectedLines {
    fn holds(&self, row: usize, side: Side) -> bool {
        self.rows.contains(&row) && self.lock.is_none_or(|lock| lock == side)
    }
}

fn side_ix(side: Side) -> usize {
    match side {
        Side::Old => 0,
        Side::New => 1,
    }
}

impl DiffView {
    /// The lines the text selection covers: its file, side and first and
    /// last line (0-based, inclusive). `None` without a selection. See
    /// [`ContextTarget::Range`] for which side and file are chosen.
    #[must_use]
    pub fn selected_line_range(&self) -> Option<(FileIx, Side, u32, u32)> {
        let lines = self.selected_lines()?;
        Some((lines.file, lines.side, lines.start, lines.end))
    }

    /// The source text of a line, without its line ending or any marker;
    /// `None` when there is no such line.
    #[must_use]
    pub fn line_text(&self, file: FileIx, side: Side, line: u32) -> Option<String> {
        self.list.diff().line_text(file, side, line)
    }

    /// What a right-click at `point` (relative to the view's top left) is
    /// on, as of the last frame. `None` over thread cards, the divider, the
    /// space past the content and before the first frame.
    #[must_use]
    pub fn context_target(&self, point: Point<Pixels>) -> Option<ContextTarget> {
        let diff = self.list.diff();
        if let Some((file, ..)) = self.header_at(point) {
            return Some(ContextTarget::FileHeader {
                file,
                path: diff.file(file).path.clone(),
            });
        }
        if let Some((row, side, line)) = self.hit_test(point) {
            let (Row::Line { file, .. } | Row::Pair { file, .. }) = self.list.rows()[row] else {
                return None;
            };
            let path = diff.file(file).path.clone();
            if let Some(lines) = self.selected_lines()
                && lines.file == file
                && lines.holds(row, side)
            {
                return Some(ContextTarget::Range {
                    file,
                    side: lines.side,
                    start: lines.start,
                    end: lines.end,
                    path,
                });
            }
            return Some(ContextTarget::Line {
                file,
                side,
                line,
                path,
            });
        }
        let Some(Row::HunkHeader { file, hunk }) = self.row_at_point(point) else {
            return None;
        };
        let hunk = &diff.file(file).hunks[hunk as usize];
        Some(ContextTarget::Hunk {
            file,
            old_start: hunk.old_start,
            new_start: hunk.new_start,
        })
    }

    /// The row under `point`, not under the pinned header.
    pub(super) fn row_at_point(&self, point: Point<Pixels>) -> Option<Row> {
        let columns = self.columns?;
        let (x, y) = (f32::from(point.x), f64::from(point.y));
        if self.list.is_empty()
            || x < 0.0
            || y < 0.0
            || x >= f32::from(columns.width)
            || self.pinned_covers(point)
        {
            return None;
        }
        let at = self.scroll_y() + y;
        (at < self.list.total_height()).then(|| self.list.rows()[self.list.row_at(at).row])
    }

    fn selected_lines(&self) -> Option<SelectedLines> {
        let selection = self.selection.as_ref().filter(|s| !s.is_empty())?;
        let (from, to) = selection.range();
        // A selection that ends at the very start of a line has not taken it.
        let last = if to.col == 0 && to.row > from.row {
            to.row - 1
        } else {
            to.row
        };
        let rows = self.list.rows();
        let mut file = None;
        let mut sides: [Option<(u32, u32)>; 2] = [None; 2];
        let mut span: Option<(usize, usize)> = None;
        for (row, shown) in rows.iter().enumerate().take(last + 1).skip(from.row) {
            let Some(at) = code_line(*shown, selection.lock()) else {
                continue;
            };
            if *file.get_or_insert(at.file) != at.file {
                break;
            }
            let slot = &mut sides[side_ix(at.side)];
            let (lo, hi) = slot.map_or((at.line, at.line), |(lo, hi)| {
                (lo.min(at.line), hi.max(at.line))
            });
            *slot = Some((lo, hi));
            span = Some(span.map_or((row, row), |(first, _)| (first, row)));
        }
        let (side, (start, end)) = match sides {
            [_, Some(new)] => (Side::New, new),
            [Some(old), None] => (Side::Old, old),
            [None, None] => return None,
        };
        let (first, last) = span?;
        Some(SelectedLines {
            file: file?,
            side,
            start,
            end,
            rows: first..=last,
            lock: (self.list.layout() == Layout::Split)
                .then(|| selection.lock())
                .flatten(),
        })
    }
}

#[cfg(test)]
mod tests {
    use diff::{Anchor, Block, Diff, FileDiff, Hunk, Segment, Text};
    use gpui::{point, px};

    use super::*;
    use crate::view::Columns;
    use crate::view::tests::test_style;

    const OLD: &str = "one two\n\tthree\nfour five\ngap\ngap2\nsix\nseven\n";
    const NEW: &str = "one two\n\tTHREE\nfour five\ngap\ngap2\nsix\neight\n";

    /// Unified rows: file header, hunk header, `one two`, `-\tthree`,
    /// `+\tTHREE`, `four five`, hunk header, `six`, `-seven`, `+eight`.
    /// Split rows: header, hunk header, then one pair per change.
    fn view(layout: Layout, blocks: Vec<Block>) -> DiffView {
        let change = Segment::Change {
            removed: 1,
            added: 1,
        };
        let diff = Diff {
            files: vec![FileDiff {
                path: "src/a b.rs".into(),
                old: Text::new(OLD),
                new: Text::new(NEW),
                hunks: vec![
                    Hunk {
                        old_start: 0,
                        new_start: 0,
                        segments: vec![Segment::Context(1), change, Segment::Context(1)],
                    },
                    Hunk {
                        old_start: 5,
                        new_start: 5,
                        segments: vec![Segment::Context(1), change],
                    },
                ],
            }],
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

    fn at(view: &DiffView, row: usize, side: Side, column: f32) -> Point<Pixels> {
        let y = view.list.top_of(row) - view.scroll_y() + view.list.row_height(row) / 2.0;
        let x = f32::from(view.text_left(side).expect("columns")) + column * 8.0;
        point(px(x), px(y as f32))
    }

    fn select(view: &mut DiffView, from: Point<Pixels>, to: Point<Pixels>) {
        assert!(view.press_text(from, 1, false));
        view.drag_text(to);
        view.release_text();
    }

    fn line(side: Side, line: u32) -> ContextTarget {
        ContextTarget::Line {
            file: FileIx(0),
            side,
            line,
            path: "src/a b.rs".into(),
        }
    }

    #[test]
    fn unified_rows_name_their_new_line_or_the_removed_old_one() {
        let view = view(Layout::Unified, Vec::new());
        let target = |row, side| view.context_target(at(&view, row, side, 2.0));
        assert_eq!(target(2, Side::New), Some(line(Side::New, 0)));
        assert_eq!(target(3, Side::Old), Some(line(Side::Old, 1)));
        assert_eq!(target(4, Side::New), Some(line(Side::New, 1)));
        // The line numbers are part of the row.
        let gutter = point(px(5.0), at(&view, 4, Side::New, 0.0).y);
        assert_eq!(view.context_target(gutter), Some(line(Side::New, 1)));
        // Past the content there is nothing.
        assert_eq!(view.context_target(point(px(5.0), px(5000.0))), None);
    }

    #[test]
    fn split_rows_name_the_side_under_the_pointer() {
        let view = view(Layout::Split, Vec::new());
        let target = |row, side| view.context_target(at(&view, row, side, 2.0));
        assert_eq!(target(3, Side::Old), Some(line(Side::Old, 1)));
        assert_eq!(target(3, Side::New), Some(line(Side::New, 1)));
        assert_eq!(target(7, Side::Old), Some(line(Side::Old, 6)));
    }

    #[test]
    fn headers_and_hunks_are_their_own_targets() {
        for layout in [Layout::Unified, Layout::Split] {
            let view = view(layout, Vec::new());
            let header = view.context_target(point(px(40.0), px(5.0)));
            let file = ContextTarget::FileHeader {
                file: FileIx(0),
                path: "src/a b.rs".into(),
            };
            assert_eq!(header, Some(file));
            let hunk_row = view.list.hunk_rows()[1] as usize;
            let y = px((view.list.top_of(hunk_row) + 4.0) as f32);
            let hunk = ContextTarget::Hunk {
                file: FileIx(0),
                old_start: 5,
                new_start: 5,
            };
            assert_eq!(view.context_target(point(px(40.0), y)), Some(hunk));
        }
    }

    #[test]
    fn the_pinned_header_is_a_file_header_over_the_rows_under_it() {
        let mut view = view(Layout::Unified, Vec::new());
        view.set_scroll_y(view.list.top_of(3) + 2.0);
        let target = view.context_target(point(px(100.0), px(5.0)));
        assert!(matches!(target, Some(ContextTarget::FileHeader { .. })));
    }

    #[test]
    fn a_thread_card_has_no_target_of_its_own() {
        let anchor = Anchor::Line {
            file: FileIx(0),
            side: Side::New,
            line: 1,
        };
        let view = view(Layout::Unified, vec![Block::new(anchor, 60.0)]);
        let card = view.list.block_row(diff::BlockId(0));
        let y = px(view.list.top_of(card) as f32 + 20.0);
        assert_eq!(view.context_target(point(px(300.0), y)), None);
    }

    #[test]
    fn a_click_inside_the_selection_is_the_selected_range() {
        let mut view = view(Layout::Unified, Vec::new());
        let (from, to) = (at(&view, 2, Side::New, 1.0), at(&view, 5, Side::New, 3.0));
        select(&mut view, from, to);
        // Rows: one two (new 0), -three (old 1), +THREE (new 1), four (new 2).
        // The added lines win in unified, so the range is new 0..=2.
        let range = |side, start, end| ContextTarget::Range {
            file: FileIx(0),
            side,
            start,
            end,
            path: "src/a b.rs".into(),
        };
        for row in [2, 3, 4, 5] {
            let side = if row == 3 { Side::Old } else { Side::New };
            let target = view.context_target(at(&view, row, side, 1.0));
            assert_eq!(target, Some(range(Side::New, 0, 2)), "row {row}");
        }
        assert_eq!(
            view.selected_line_range(),
            Some((FileIx(0), Side::New, 0, 2))
        );
        // Outside it, it is a line again, and the selection is untouched.
        let outside = view.context_target(at(&view, 8, Side::Old, 1.0));
        assert_eq!(outside, Some(line(Side::Old, 6)));
        assert!(view.has_selection());
    }

    #[test]
    fn a_split_range_is_on_the_locked_side_only() {
        let mut view = view(Layout::Split, Vec::new());
        let (from, to) = (at(&view, 3, Side::Old, 2.0), at(&view, 7, Side::Old, 2.0));
        select(&mut view, from, to);
        assert_eq!(
            view.selected_line_range(),
            Some((FileIx(0), Side::Old, 1, 6))
        );
        let inside = view.context_target(at(&view, 4, Side::Old, 1.0));
        assert!(matches!(
            inside,
            Some(ContextTarget::Range {
                side: Side::Old,
                start: 1,
                end: 6,
                ..
            })
        ));
        // The other half of a selected row is not selected.
        let other = view.context_target(at(&view, 4, Side::New, 1.0));
        assert_eq!(other, Some(line(Side::New, 2)));
    }

    #[test]
    fn a_selection_ending_at_the_start_of_a_line_has_not_taken_it() {
        let mut view = view(Layout::Unified, Vec::new());
        let (from, to) = (at(&view, 2, Side::New, 1.0), at(&view, 5, Side::New, 0.0));
        select(&mut view, from, to);
        // Rows 2..=4 and the start of row 5: new 0, 1 only.
        assert_eq!(
            view.selected_line_range(),
            Some((FileIx(0), Side::New, 0, 1))
        );
        // All removed lines: the old side.
        let mut removed = self::view(Layout::Unified, Vec::new());
        let from = at(&removed, 3, Side::Old, 1.0);
        let to = at(&removed, 3, Side::Old, 4.0);
        select(&mut removed, from, to);
        assert_eq!(
            removed.selected_line_range(),
            Some((FileIx(0), Side::Old, 1, 1))
        );
        // Nothing selected, nothing to range.
        let none = self::view(Layout::Unified, Vec::new());
        assert_eq!(none.selected_line_range(), None);
    }

    #[test]
    fn a_context_lookup_does_not_touch_the_selection() {
        let mut view = view(Layout::Unified, Vec::new());
        let (from, to) = (at(&view, 2, Side::New, 1.0), at(&view, 2, Side::New, 4.0));
        select(&mut view, from, to);
        let copied = view.copy_selection();
        assert!(view.context_target(at(&view, 2, Side::New, 2.0)).is_some());
        assert!(view.context_target(at(&view, 8, Side::New, 2.0)).is_some());
        assert_eq!(view.copy_selection(), copied);
        assert!(!view.is_selecting());
    }

    #[test]
    fn line_text_is_the_source_line() {
        let view = view(Layout::Unified, Vec::new());
        assert_eq!(
            view.line_text(FileIx(0), Side::Old, 1).as_deref(),
            Some("\tthree")
        );
        assert_eq!(
            view.line_text(FileIx(0), Side::New, 6).as_deref(),
            Some("eight")
        );
        assert_eq!(view.line_text(FileIx(0), Side::New, 70), None);
    }
}
