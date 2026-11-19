//! The expand arrows in the gutter of hunk headers and of the expander row
//! after a file's last hunk: where they are, what a click on one reveals,
//! and the hover that brightens them.

use std::collections::HashMap;

use diff::{Expand, FileIx, Layout, Reveals, Row};
use gpui::{Bounds, Pixels, Point, point, px, size};

use super::{Columns, DiffView, DiffViewEvent, RowTarget};

/// One expand arrow: the gap of `file` it reveals lines of (see
/// [`diff::FileDiff::gap`]) and which way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ExpandArrow {
    pub file: FileIx,
    pub gap: u32,
    pub expand: Expand,
}

/// The gutter cell of arrow `ix` of `count` on a row, relative to the row's
/// left: the arrows share the gutter of the left pane equally.
pub(super) fn arrow_cell(
    columns: Columns,
    layout: Layout,
    ix: usize,
    count: usize,
) -> (Pixels, Pixels) {
    let width = columns.gutter(layout) / count.max(1) as f32;
    (width * ix as f32, width)
}

impl DiffView {
    /// The arrows of a hunk header or expander row, left to right, with the
    /// gap they reveal; none for other rows.
    pub(super) fn row_arrows(&self, row: Row) -> Option<(FileIx, u32, &'static [Expand])> {
        let (file, gap) = row.gap(self.list.diff())?;
        let arrows = self.list.arrows(file, gap);
        (!arrows.is_empty()).then_some((file, gap, arrows))
    }

    /// The arrow under `point` (relative to the view's top left), not under
    /// the pinned header.
    pub(super) fn arrow_at(&self, point: Point<Pixels>) -> Option<ExpandArrow> {
        let columns = self.columns?;
        let row = self.row_at_point(point)?;
        let (file, gap, arrows) = self.row_arrows(row)?;
        let layout = self.list.layout();
        arrows.iter().enumerate().find_map(|(ix, &expand)| {
            let (left, width) = arrow_cell(columns, layout, ix, arrows.len());
            (left..left + width)
                .contains(&point.x)
                .then_some(ExpandArrow { file, gap, expand })
        })
    }

    /// A left press at `point`: on an arrow, reveals its lines, returning
    /// the event to emit when it did.
    pub(super) fn press_arrow(&mut self, point: Point<Pixels>) -> Option<Option<DiffViewEvent>> {
        let arrow = self.arrow_at(point)?;
        Some(
            self.expand(arrow)
                .then_some(DiffViewEvent::ContextRevealed(arrow.file)),
        )
    }

    /// Follows the pointer for the arrow under it (`None` when it is off the
    /// view); whether that changed.
    pub(super) fn hover_arrow(&mut self, point: Option<Point<Pixels>>) -> bool {
        let hovered = point.and_then(|point| self.arrow_at(point));
        if hovered == self.hovered_arrow {
            return false;
        }
        self.hovered_arrow = hovered;
        true
    }

    /// Whether `arrow` is the one under the pointer.
    pub(super) fn is_hovered(&self, arrow: ExpandArrow) -> bool {
        self.hovered_arrow == Some(arrow)
    }

    /// Reveals the lines `arrow` stands for, as a click on it does; whether
    /// anything changed. The hunk next to the arrow stays where it is on
    /// screen: revealing upward (or the whole gap) keeps the hunk's first
    /// row in place, revealing downward the row above the arrow.
    pub fn expand(&mut self, arrow: ExpandArrow) -> bool {
        let Some(row) = self.gap_row(arrow.file, arrow.gap) else {
            return false;
        };
        let anchor = match arrow.expand {
            Expand::Down => row.checked_sub(1),
            Expand::Up | Expand::All => Some(row + 1).filter(|&next| next < self.list.len()),
        }
        .map(|row| {
            let target = RowTarget::of(self.list.rows()[row]);
            (target, self.list.top_of(row) - self.scroll_y())
        });
        if !self.list.expand(arrow.file, arrow.gap, arrow.expand) {
            return false;
        }
        self.update_lanes();
        self.rebase_selection();
        self.hovered_arrow = None;
        if let Some((target, screen_y)) = anchor {
            let (row, _) = self.target_row(target);
            self.set_scroll_y(self.list.top_of(row) - screen_y);
        }
        true
    }

    /// The row of the header or expander that stands for gap `gap` of `file`.
    fn gap_row(&self, file: FileIx, gap: u32) -> Option<usize> {
        let start = self.list.file_row(file)?;
        let rows = self.list.rows();
        rows[start + 1..]
            .iter()
            .take_while(|row| !matches!(row, Row::FileHeader { .. }))
            .position(|&row| row.gap(self.list.diff()) == Some((file, gap)))
            .map(|ix| start + 1 + ix)
    }

    /// The arrows the last frame drew in the viewport and where, relative
    /// to the view's top left: for tests, which click them by position.
    #[must_use]
    pub fn expand_arrows(&self) -> Vec<(ExpandArrow, Bounds<Pixels>)> {
        let Some(columns) = self.columns else {
            return Vec::new();
        };
        let viewport = px(self.scroll.state.y.viewport as f32);
        let layout = self.list.layout();
        let mut out = Vec::new();
        for visible in &self.visible {
            let Some((file, gap, arrows)) = self.row_arrows(self.list.rows()[visible.row]) else {
                continue;
            };
            if visible.top < px(0.0) || visible.top + visible.height > viewport {
                continue;
            }
            for (ix, &expand) in arrows.iter().enumerate() {
                let (left, width) = arrow_cell(columns, layout, ix, arrows.len());
                let bounds = Bounds::new(point(left, visible.top), size(width, visible.height));
                if !self.pinned_covers(bounds.center()) {
                    out.push((ExpandArrow { file, gap, expand }, bounds));
                }
            }
        }
        out
    }

    /// The context revealed in each file that has some, by path: what a
    /// view of a reloaded diff at the same head takes back with
    /// [`DiffView::set_reveals`].
    #[must_use]
    pub fn reveals(&self) -> HashMap<String, Reveals> {
        let diff = self.list.diff();
        diff.files
            .iter()
            .enumerate()
            .map(|(ix, file)| (file, self.list.reveals(FileIx(ix as u32))))
            .filter(|(_, reveals)| !reveals.is_empty())
            .map(|(file, reveals)| (file.path.clone(), reveals.clone()))
            .collect()
    }

    /// Reveals the context `reveals` holds for each file, by path (see
    /// [`DiffView::reveals`]); files it does not name reveal none. The line
    /// at the top of the view stays there.
    pub fn set_reveals(&mut self, reveals: &HashMap<String, Reveals>) {
        let by_file = self
            .list
            .diff()
            .files
            .iter()
            .map(|file| reveals.get(&file.path).cloned().unwrap_or_default())
            .collect();
        self.reproject(|list| list.set_reveals(by_file));
    }
}

#[cfg(test)]
mod tests {
    use diff::{Diff, FileDiff, Hunk, Segment, Side, Text};
    use gpui::{CursorStyle, point, px, size};

    use super::*;
    use crate::view::tests::test_style;

    fn numbered(n: u32) -> Text {
        Text::new((0..n).map(|i| format!("l{i}\n")).collect::<String>())
    }

    /// A 120-line file: hunks at 40 (one line changed) and 70 (one line
    /// added), each with two context lines on either side. Gaps: 40 lines
    /// before the first hunk, 25 between, 46 after.
    fn diff() -> Diff {
        let hunk = |old_start, new_start, removed| Hunk {
            old_start,
            new_start,
            segments: vec![
                Segment::Context(2),
                Segment::Change { removed, added: 1 },
                Segment::Context(2),
            ],
        };
        let mut new = (0..121).map(|i| format!("l{i}\n")).collect::<Vec<_>>();
        new[42] = "changed\n".into();
        Diff {
            files: vec![FileDiff {
                path: "a.rs".into(),
                old: numbered(120),
                new: Text::new(new.concat()),
                hunks: vec![hunk(40, 40, 1), hunk(70, 70, 0)],
            }],
            blocks: Vec::new(),
        }
    }

    /// A view `viewport` high, at the top.
    fn view(layout: Layout, viewport: f64) -> DiffView {
        let mut view = DiffView::new(diff(), layout, test_style());
        view.columns = Some(Columns {
            ch: px(8.0),
            number: px(24.0),
            marker: px(16.0),
            width: px(600.0),
            split: px(300.0),
        });
        view.scroll.state.y.viewport = viewport;
        view.collect_visible();
        view
    }

    fn arrows(view: &DiffView) -> Vec<ExpandArrow> {
        view.expand_arrows()
            .into_iter()
            .map(|(arrow, _)| arrow)
            .collect()
    }

    fn arrow(gap: u32, expand: Expand) -> ExpandArrow {
        ExpandArrow {
            file: FileIx(0),
            gap,
            expand,
        }
    }

    /// Re-collects the rows the next frame would paint.
    fn frame(view: &mut DiffView) {
        view.visible.clear();
        view.collect_visible();
    }

    #[test]
    fn each_gap_gets_the_arrows_its_size_calls_for() {
        for layout in [Layout::Unified, Layout::Split] {
            let view = view(layout, 4000.0);
            use Expand::{Down, Up};
            assert_eq!(
                arrows(&view),
                [arrow(0, Up), arrow(1, Down), arrow(1, Up), arrow(2, Down)],
                "{layout:?}"
            );
            // Two arrows share the gutter: the two number columns unified,
            // halves of the one column split.
            let bounds: Vec<_> = view.expand_arrows().into_iter().map(|(_, b)| b).collect();
            let gutter = f32::from(view.gutter_width().expect("columns"));
            assert_eq!(bounds[0].size.width, px(gutter));
            assert_eq!(bounds[1].size.width, px(gutter / 2.0));
            assert_eq!(bounds[2].origin.x, px(gutter / 2.0));
        }
    }

    #[test]
    fn a_click_on_an_arrow_reveals_lines_and_keeps_the_hunk_in_place() {
        let mut view = view(Layout::Unified, 100.0);
        let (up, bounds) = view.expand_arrows()[0];
        assert_eq!(up, arrow(0, Expand::Up));
        let len = view.list.len();
        // The first row of the hunk, on screen, before and after.
        let hunk_line = |view: &DiffView| {
            let row = view
                .list
                .rows()
                .iter()
                .position(|row| matches!(row, Row::Line { new: Some(40), .. }))
                .expect("line 40");
            view.list.top_of(row) - view.scroll_y()
        };
        let before = hunk_line(&view);
        assert_eq!(
            view.press_arrow(bounds.center()),
            Some(Some(DiffViewEvent::ContextRevealed(FileIx(0))))
        );
        assert_eq!(view.list.len(), len + 20);
        assert_eq!(hunk_line(&view), before);
        // The header moved up over the 20 lines, which run 20..40.
        let header = view
            .list
            .rows()
            .iter()
            .position(|row| matches!(row, Row::HunkHeader { hunk: 0, .. }))
            .expect("header");
        assert_eq!(
            view.list.rows()[header + 1],
            Row::Line {
                file: FileIx(0),
                old: Some(20),
                new: Some(20)
            }
        );
        // Off the arrows nothing happens.
        let mut beside = bounds.center();
        beside.x = px(200.0);
        assert_eq!(view.press_arrow(beside), None);
    }

    #[test]
    fn revealing_downward_keeps_the_rows_above_the_arrow() {
        let mut view = view(Layout::Split, 60.0);
        view.set_scroll_y(100.0);
        frame(&mut view);
        let top = view.list.row_at(view.scroll_y());
        assert!(view.expand(arrow(1, Expand::Down)));
        assert_eq!(view.list.row_at(view.scroll_y()), top);
        // The second gap has 5 lines left: one arrow reveals them all.
        assert_eq!(view.list.arrows(FileIx(0), 1), [Expand::All]);
        assert!(view.expand(arrow(1, Expand::All)));
        assert!(view.list.arrows(FileIx(0), 1).is_empty());
        assert!(
            !view
                .list
                .rows()
                .iter()
                .any(|row| matches!(row, Row::HunkHeader { hunk: 1, .. }))
        );
        assert!(!view.expand(arrow(1, Expand::All)));
    }

    #[test]
    fn arrows_are_a_hand_and_brighten_under_the_pointer() {
        let mut view = view(Layout::Unified, 4000.0);
        let (first, bounds) = view.expand_arrows()[0];
        let at = bounds.center();
        let screen = size(px(620.0), px(4000.0));
        assert_eq!(view.cursor_at(at, screen), CursorStyle::PointingHand);
        // Past the gutter the header is plain.
        assert_eq!(
            view.cursor_at(point(px(300.0), at.y), screen),
            CursorStyle::Arrow
        );
        assert!(view.hover_arrow(Some(at)));
        assert!(view.is_hovered(first));
        assert!(!view.hover_arrow(Some(at)));
        assert!(view.hover_arrow(None));
        assert!(!view.is_hovered(first));
    }

    #[test]
    fn reveals_carry_over_by_path_and_full_context_shows_everything() {
        let mut view = view(Layout::Unified, 4000.0);
        view.expand(arrow(2, Expand::Down));
        view.expand(arrow(0, Expand::Up));
        let reveals = view.reveals();
        assert_eq!(reveals.len(), 1);
        let mut again = DiffView::new(diff(), Layout::Unified, test_style());
        again.set_reveals(&reveals);
        assert_eq!(again.list.rows(), view.list.rows());
        // Full context ignores them, and they are back after it.
        again.reproject(|list| list.set_context(None, diff::ContextMode::Full));
        assert!(again.list.arrows(FileIx(0), 0).is_empty());
        assert!(again.list.shows(FileIx(0), Side::Old, 5));
        again.reproject(|list| list.set_context(None, diff::ContextMode::Hunks));
        assert_eq!(again.list.rows(), view.list.rows());
        again.set_reveals(&HashMap::new());
        assert_eq!(
            again.list.rows(),
            DiffView::new(diff(), Layout::Unified, test_style())
                .list
                .rows()
        );
    }
}
