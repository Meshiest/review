//! Where a pointer is in the diff: which code line, and whether it is over
//! the gutter, where the "add comment" button appears and a drag picks a
//! range. The diff view reports the line and its column sizes
//! ([`DiffView::hit_test`], [`DiffView::gutter_width`]); this adds the file
//! and the range a drag covers.

use diff::{DiffList, FileIx, Row, Side};
use diff_view::DiffView;
use forge::model::LineRef;
use gpui::{point, px};
use store::DraftAnchor;

/// A code line under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub row: usize,
    pub file: FileIx,
    pub side: Side,
    /// 0-based, as in the diff.
    pub line: u32,
    /// The left edge of the pane the pointer is in: 0, or the right pane's in split.
    pub pane: u32,
    /// Whether the pointer is over the pane's line numbers.
    pub in_gutter: bool,
}

impl Hit {
    /// The same place with its line as the forge numbers it.
    #[must_use]
    pub fn line_ref(&self) -> LineRef {
        LineRef {
            side: match self.side {
                Side::Old => forge::model::Side::Old,
                Side::New => forge::model::Side::New,
            },
            line: self.line + 1,
        }
    }
}

/// The code line at `(x, y)`, relative to the view's top left, as the view
/// last painted it. `None` over headers, blocks and past the content, and
/// before the view's first frame.
#[must_use]
pub fn hit(view: &DiffView, x: f32, y: f32) -> Option<Hit> {
    let (row, side, line) = view.hit_test(point(px(x), px(y)))?;
    let file = file_of(view.list(), row)?;
    let pane = f32::from(view.pane_left(side)?);
    let gutter = f32::from(view.gutter_width()?);
    Some(Hit {
        row,
        file,
        side,
        line,
        pane: pane as u32,
        in_gutter: x - pane < gutter,
    })
}

/// The line `row` shows on `side`.
#[must_use]
pub fn line_on(list: &DiffList, row: usize, side: Side) -> Option<u32> {
    match (list.rows().get(row)?, side) {
        (Row::Line { old, .. }, Side::Old) => *old,
        (Row::Line { new, .. }, Side::New) => *new,
        (Row::Pair { left, .. }, Side::Old) => left.map(|l| l.line),
        (Row::Pair { right, .. }, Side::New) => right.map(|r| r.line),
        _ => None,
    }
}

/// The file a code row belongs to.
#[must_use]
pub fn file_of(list: &DiffList, row: usize) -> Option<FileIx> {
    match list.rows().get(row)? {
        Row::Line { file, .. } | Row::Pair { file, .. } => Some(*file),
        _ => None,
    }
}

/// Lines of one file and side the user is pointing at: a click is a range of
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Range {
    pub file: FileIx,
    pub side: Side,
    /// 0-based, `first <= last`.
    pub first: u32,
    pub last: u32,
    /// The rows from the first line's to the last line's.
    pub rows: (usize, usize),
}

impl Range {
    #[must_use]
    pub fn of(hit: &Hit) -> Self {
        Self {
            file: hit.file,
            side: hit.side,
            first: hit.line,
            last: hit.line,
            rows: (hit.row, hit.row),
        }
    }

    /// The range from `start` to wherever the pointer is now. A pointer on
    /// another file, or on a row with no line on the range's side, leaves
    /// the end where it was.
    #[must_use]
    pub fn drag(start: &Hit, now: Option<&Hit>, list: &DiffList) -> Self {
        let mut range = Self::of(start);
        let Some(now) = now else { return range };
        if file_of(list, now.row) != Some(start.file) {
            return range;
        }
        let Some(line) = line_on(list, now.row, start.side) else {
            return range;
        };
        range.first = start.line.min(line);
        range.last = start.line.max(line);
        range.rows = (start.row.min(now.row), start.row.max(now.row));
        range
    }

    /// The draft anchor of a comment on this range of `path`.
    #[must_use]
    pub fn anchor(&self, path: &str, multi_line: bool) -> DraftAnchor {
        let forge_side = match self.side {
            Side::Old => forge::model::Side::Old,
            Side::New => forge::model::Side::New,
        };
        let last = LineRef {
            side: forge_side,
            line: self.last + 1,
        };
        let start = (multi_line && self.first < self.last).then_some(LineRef {
            side: forge_side,
            line: self.first + 1,
        });
        // Without ranges, a drag comments on the line it ended on.
        DraftAnchor::Line {
            path: path.to_owned(),
            line: last,
            start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diff::{Diff, FileDiff, Hunk, Layout, RowHeights, Segment, Text};

    fn list(layout: Layout) -> DiffList {
        let text =
            |lines: &[&str]| Text::new(lines.iter().map(|l| format!("{l}\n")).collect::<String>());
        let file = FileDiff {
            path: "a.rs".into(),
            old: text(&["a", "b", "c", "d"]),
            new: text(&["a", "B", "B2", "c", "d"]),
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
        DiffList::new(
            Diff {
                files: vec![file],
                blocks: Vec::new(),
            },
            layout,
            RowHeights {
                code: 20.0,
                file_header: 32.0,
            },
        )
    }

    /// The hit on `row`'s `side`, as the view would report it.
    fn at(list: &DiffList, row: usize, side: Side) -> Hit {
        Hit {
            row,
            file: file_of(list, row).unwrap(),
            side,
            line: line_on(list, row, side).unwrap(),
            pane: 0,
            in_gutter: true,
        }
    }

    #[test]
    fn a_drag_covers_the_lines_between_and_stays_in_its_file_and_side() {
        let list = list(Layout::Unified);
        // Rows: file header, hunk header, a, -b, +B, +B2, c, d.
        let start = at(&list, 4, Side::New);
        let end = at(&list, 7, Side::New);
        let range = Range::drag(&start, Some(&end), &list);
        assert_eq!((range.first, range.last, range.rows), (1, 4, (4, 7)));
        // Dragging up works the same way.
        let up = Range::drag(&end, Some(&start), &list);
        assert_eq!((up.first, up.last), (1, 4));
        // A removed line has no line on the new side: the end stays.
        let removed = at(&list, 3, Side::Old);
        assert_eq!(
            Range::drag(&start, Some(&removed), &list),
            Range::of(&start)
        );
        assert_eq!(Range::drag(&start, None, &list), Range::of(&start));
    }

    #[test]
    fn the_anchor_ends_on_the_last_line_and_starts_where_ranges_are_allowed() {
        let list = list(Layout::Unified);
        let start = at(&list, 4, Side::New);
        let end = at(&list, 7, Side::New);
        let range = Range::drag(&start, Some(&end), &list);
        let line = |n| LineRef {
            side: forge::model::Side::New,
            line: n,
        };
        assert_eq!(
            range.anchor("a.rs", true),
            DraftAnchor::Line {
                path: "a.rs".into(),
                line: line(5),
                start: Some(line(2)),
            }
        );
        assert_eq!(
            range.anchor("a.rs", false),
            DraftAnchor::Line {
                path: "a.rs".into(),
                line: line(5),
                start: None,
            }
        );
    }

    #[test]
    fn split_rows_name_a_line_per_side() {
        let list = list(Layout::Split);
        // Rows: file header, hunk header, a|a, b|B, -|B2, c|c, d|d.
        assert_eq!(line_on(&list, 3, Side::Old), Some(1));
        assert_eq!(line_on(&list, 3, Side::New), Some(1));
        assert_eq!(line_on(&list, 4, Side::Old), None);
        assert_eq!(file_of(&list, 0), None);
    }
}
