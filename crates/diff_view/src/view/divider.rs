//! The split divider: a draggable partition between the old and new panes.

use gpui::{Bounds, Context, Pixels, Point, point, px, size};

use diff::Layout;

use super::{Columns, DiffView, DiffViewEvent};

/// Width of the divider's grab area, centered on the dividing line.
const GRAB_WIDTH: f32 = 6.0;
/// The smallest share of the view either pane keeps.
const MIN_RATIO: f32 = 0.2;
/// Columns of code a pane keeps at the least, beside its gutter.
const MIN_CODE_COLUMNS: f32 = 8.0;

/// A drag in progress.
pub(super) struct DividerDrag {
    /// The ratio when the press began, to tell whether the drag changed it.
    start: f32,
}

/// `ratio` held to what the panes can give up; NaN is the middle.
pub(super) fn clamp_ratio(ratio: f32) -> f32 {
    if ratio.is_nan() {
        0.5
    } else {
        ratio.clamp(MIN_RATIO, 1.0 - MIN_RATIO)
    }
}

/// The x of the dividing line in a view `width` wide: `ratio` of it, with
/// each pane keeping `min_pane`; the middle when they cannot both.
pub(super) fn split_x(width: Pixels, ratio: f32, min_pane: Pixels) -> Pixels {
    if min_pane * 2.0 >= width {
        return (width / 2.0).floor();
    }
    (width * clamp_ratio(ratio))
        .floor()
        .max(min_pane)
        .min(width - min_pane)
}

/// The least a pane may be: its share of the view, or its gutter and some
/// code if that is more.
pub(super) fn min_pane(width: Pixels, number: Pixels, marker: Pixels, ch: Pixels) -> Pixels {
    (width * MIN_RATIO).max(number + marker + ch * MIN_CODE_COLUMNS)
}

impl DiffView {
    /// The left pane's share of the width in split layout, from 0.2 to 0.8.
    #[must_use]
    pub fn split_ratio(&self) -> f32 {
        self.split_ratio
    }

    /// Sets the split ratio (held to 0.2..=0.8, and wider if a pane would
    /// lose its gutter). Emits no event: the app set it.
    pub fn set_split_ratio(&mut self, ratio: f32, cx: &mut Context<Self>) {
        if self.apply_ratio(ratio) {
            cx.notify();
        }
    }

    fn apply_ratio(&mut self, ratio: f32) -> bool {
        let ratio = clamp_ratio(ratio);
        let changed = ratio != self.split_ratio;
        self.split_ratio = ratio;
        changed
    }

    /// True while the divider is being dragged.
    pub fn is_resizing(&self) -> bool {
        self.divider_drag.is_some()
    }

    /// Whether `point` (relative to the view's top left) is on the divider's
    /// grab area; never in unified layout or under the pinned header.
    pub(super) fn on_divider(&self, point: Point<Pixels>) -> bool {
        let Some(columns) = self.columns else {
            return false;
        };
        if self.list.layout() != Layout::Split || self.pinned_covers(point) {
            return false;
        }
        let grab = px(GRAB_WIDTH);
        (columns.split - grab / 2.0..columns.split + grab / 2.0).contains(&point.x)
            && point.y >= px(0.0)
    }

    /// The divider's grab area, for a view at `bounds`.
    pub(super) fn divider_bounds(&self, bounds: Bounds<Pixels>) -> Option<Bounds<Pixels>> {
        let columns = self.columns?;
        (self.list.layout() == Layout::Split).then(|| {
            let grab = px(GRAB_WIDTH);
            Bounds::new(
                bounds.origin + point(columns.split - grab / 2.0, px(0.0)),
                size(grab, bounds.size.height),
            )
        })
    }

    /// A left press at `point` with `clicks` clicks so far. `None` when it is
    /// not on the divider; otherwise the press is taken: one click begins a
    /// drag, two reset the ratio to the middle, returning the event for that.
    pub(super) fn press_divider(
        &mut self,
        point: Point<Pixels>,
        clicks: usize,
    ) -> Option<Option<DiffViewEvent>> {
        if !self.on_divider(point) {
            return None;
        }
        if clicks >= 2 {
            self.divider_drag = None;
            let changed = self.apply_ratio(0.5);
            return Some(changed.then_some(DiffViewEvent::SplitRatioChanged(0.5)));
        }
        self.divider_drag = Some(DividerDrag {
            start: self.split_ratio,
        });
        Some(None)
    }

    /// The pointer moved to `point` during a drag; whether the view must
    /// redraw.
    pub(super) fn drag_divider(&mut self, point: Point<Pixels>) -> bool {
        let (Some(columns), Some(_)) = (self.columns, &self.divider_drag) else {
            return false;
        };
        if columns.width <= px(0.0) {
            return false;
        }
        self.apply_ratio(point.x / columns.width)
    }

    /// The button went up: ends the drag, returning the event when the drag
    /// changed the ratio.
    pub(super) fn release_divider(&mut self) -> Option<DiffViewEvent> {
        let drag = self.divider_drag.take()?;
        (drag.start != self.split_ratio)
            .then_some(DiffViewEvent::SplitRatioChanged(self.split_ratio))
    }

    /// Whether `point` is under the pinned header.
    pub(super) fn pinned_covers(&self, point: Point<Pixels>) -> bool {
        let height = f64::from(self.style.file_header_height);
        self.pinned()
            .is_some_and(|pinned| f64::from(point.y) < pinned.top + height)
    }
}

impl Columns {
    /// The widest code pane of `layout`, for shaping.
    pub(super) fn widest_pane(&self, layout: Layout) -> Pixels {
        match layout {
            Layout::Unified => self.width,
            Layout::Split => self.split.max(self.width - self.split),
        }
    }
}

#[cfg(test)]
mod tests {
    use diff::FileIx;

    use super::*;
    use crate::view::files::tests::view;

    #[test]
    fn the_ratio_is_held_to_what_the_panes_can_give_up() {
        assert_eq!(clamp_ratio(0.5), 0.5);
        assert_eq!(clamp_ratio(0.0), MIN_RATIO);
        assert_eq!(clamp_ratio(-3.0), MIN_RATIO);
        assert_eq!(clamp_ratio(0.95), 1.0 - MIN_RATIO);
        assert_eq!(clamp_ratio(f32::NAN), 0.5);
        assert_eq!(clamp_ratio(f32::INFINITY), 1.0 - MIN_RATIO);
    }

    #[test]
    fn the_divider_leaves_each_pane_its_gutter_and_some_code() {
        let (width, number, marker, ch) = (px(600.0), px(24.0), px(16.0), px(8.0));
        let min = min_pane(width, number, marker, ch);
        // A fifth of the view is more than a gutter and 8 columns here.
        assert_eq!(min, px(120.0));
        assert_eq!(split_x(width, 0.5, min), px(300.0));
        assert_eq!(split_x(width, 0.3, min), px(180.0));
        assert_eq!(split_x(width, 0.7, min), px(420.0));
        assert_eq!(split_x(width, 0.2, min), px(120.0));
        // Narrow: the gutter and code decide, not a fifth of the width.
        let narrow = px(300.0);
        let min = min_pane(narrow, number, marker, ch);
        assert_eq!(min, px(104.0));
        assert_eq!(split_x(narrow, 0.2, min), px(104.0));
        assert_eq!(split_x(narrow, 0.8, min), px(196.0));
        // Too narrow for two panes: the middle.
        assert_eq!(split_x(px(200.0), 0.2, px(104.0)), px(100.0));
    }

    fn split_view() -> DiffView {
        view(Layout::Split, 2, Vec::new())
    }

    #[test]
    fn a_drag_moves_the_ratio_and_reports_where_it_ended() {
        let mut view = split_view();
        let on = point(px(300.0), px(60.0));
        assert!(view.on_divider(on));
        assert!(view.on_divider(point(px(302.5), px(60.0))));
        assert!(!view.on_divider(point(px(296.0), px(60.0))));
        assert_eq!(view.press_divider(on, 1), Some(None));
        assert!(view.is_resizing());
        // Nothing is reported while the pointer moves.
        assert!(view.drag_divider(point(px(180.0), px(90.0))));
        assert_eq!(view.split_ratio(), 0.3);
        assert!(!view.drag_divider(point(px(180.0), px(95.0))));
        // Dragged far out, the ratio stops at the limit.
        assert!(view.drag_divider(point(px(-50.0), px(90.0))));
        assert_eq!(view.split_ratio(), MIN_RATIO);
        assert!(view.drag_divider(point(px(240.0), px(90.0))));
        assert_eq!(
            view.release_divider(),
            Some(DiffViewEvent::SplitRatioChanged(0.4))
        );
        assert!(!view.is_resizing());
        assert_eq!(view.release_divider(), None);
        // A press and release that moved nothing reports nothing.
        view.press_divider(point(px(300.0), px(10.0)), 1);
        assert_eq!(view.release_divider(), None);
        assert_eq!(view.split_ratio(), 0.4);
    }

    #[test]
    fn a_double_click_resets_the_ratio_to_the_middle() {
        let mut view = split_view();
        view.apply_ratio(0.3);
        let on = point(px(180.0), px(60.0));
        view.columns.as_mut().expect("columns").split = px(180.0);
        assert!(view.on_divider(on));
        assert_eq!(view.press_divider(on, 1), Some(None));
        assert_eq!(view.release_divider(), None);
        assert_eq!(
            view.press_divider(on, 2),
            Some(Some(DiffViewEvent::SplitRatioChanged(0.5)))
        );
        assert_eq!(view.split_ratio(), 0.5);
        assert!(!view.is_resizing());
        // Already in the middle: nothing changed, nothing reported.
        assert_eq!(
            view.press_divider(point(px(180.0), px(60.0)), 2),
            Some(None)
        );
    }

    #[test]
    fn the_set_ratio_is_clamped_and_tells_no_one() {
        let mut view = split_view();
        assert_eq!(view.split_ratio(), 0.5);
        assert!(view.apply_ratio(0.1));
        assert_eq!(view.split_ratio(), MIN_RATIO);
        assert!(!view.apply_ratio(0.0));
        assert!(view.apply_ratio(0.9));
        assert_eq!(view.split_ratio(), 1.0 - MIN_RATIO);
    }

    #[test]
    fn a_press_on_the_divider_selects_nothing_and_hits_no_line() {
        let mut view = split_view();
        // File 0's rows: header, hunk header, then pairs; the second pair
        // row (a code row) is at 30 + 20 + 20.
        let on = point(px(300.0), px(30.0 + 20.0 + 30.0));
        assert_eq!(
            view.hit_test(on),
            None,
            "an app's gutter press finds no line"
        );
        assert!(!view.press_text(on, 1, false));
        assert!(!view.is_selecting() && !view.has_selection());
        // The same row either side of it does hit.
        let side = point(px(260.0), px(100.0));
        assert!(view.hit_test(side).is_some());
        let right = point(px(340.0), px(100.0));
        assert!(view.hit_test(right).is_some());
        assert!(view.press_text(right, 1, false));
        view.release_text();
        // Unified layout has no divider.
        let mut unified = view_unified();
        assert!(!unified.on_divider(on));
        assert_eq!(unified.press_divider(on, 1), None);
        assert!(unified.press_text(on, 1, false));
    }

    fn view_unified() -> DiffView {
        view(Layout::Unified, 1, Vec::new())
    }

    #[test]
    fn the_pinned_header_is_not_the_divider() {
        let mut view = split_view();
        let second = view.list.top_of(view.list.file_rows()[1] as usize);
        view.set_scroll_y(second + 60.0);
        let on = point(px(300.0), px(10.0));
        assert_eq!(view.pinned().map(|p| p.file), Some(FileIx(1)));
        assert!(!view.on_divider(on));
        assert!(view.on_divider(point(px(300.0), px(40.0))));
    }

    #[test]
    fn the_new_pane_starts_at_the_divider() {
        let mut view = split_view();
        view.columns.as_mut().expect("columns").split = px(180.0);
        assert_eq!(view.pane_left(diff::Side::Old), Some(px(0.0)));
        assert_eq!(view.pane_left(diff::Side::New), Some(px(180.0)));
        // Rows pair by the new split: a point at x 200 is the new side.
        let hit = view.hit_test(point(px(200.0), px(100.0)));
        assert_eq!(hit.map(|(_, side, _)| side), Some(diff::Side::New));
        let hit = view.hit_test(point(px(170.0), px(100.0)));
        assert_eq!(hit.map(|(_, side, _)| side), Some(diff::Side::Old));
        // The widest pane is what shaping covers.
        let columns = view.columns.expect("columns");
        assert_eq!(columns.widest_pane(Layout::Split), px(420.0));
        assert_eq!(columns.widest_pane(Layout::Unified), px(600.0));
    }
}
