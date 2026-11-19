//! The mouse cursor over the diff view: the regions that set it, worked out
//! from the rows without a window so that the paint path only has to turn
//! them into hitboxes.

use diff::{Layout, Row, Side};
use gpui::{Bounds, CursorStyle, Pixels, Size, point, px, size};

use super::expand::arrow_cell;
use super::files::path_left;
use super::{Columns, DiffView};
use crate::select::code_line;

/// Rows closer than this to the one before them continue its region.
const SEAM: f32 = 0.5;

/// A part of the view and the cursor over it, relative to the view's top
/// left.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct CursorRegion {
    pub bounds: Bounds<Pixels>,
    pub style: CursorStyle,
}

/// The cursor regions of one frame, in priority order: where regions overlap,
/// the later one wins.
#[derive(Debug, Default, PartialEq)]
pub(super) struct CursorMap {
    pub regions: Vec<CursorRegion>,
    /// Regions from this index on belong to the pinned header, over the
    /// rest, which it hides.
    pub over_pinned: usize,
    /// The visible part of the pinned header.
    pub pinned: Option<Bounds<Pixels>>,
}

impl CursorMap {
    /// The cursor at `point`, the default arrow where no region is. The
    /// window does this with the hitboxes; this is for tests.
    #[cfg(test)]
    pub(super) fn at(&self, point: gpui::Point<Pixels>) -> CursorStyle {
        let hidden = self.pinned.is_some_and(|pinned| pinned.contains(&point));
        let live = if hidden {
            &self.regions[self.over_pinned..]
        } else {
            &self.regions[..self.over_pinned]
        };
        live.iter()
            .rev()
            .find(|region| region.bounds.contains(&point))
            .map_or(CursorStyle::Arrow, |region| region.style)
    }
}

/// Adds `bounds` to `runs`, as a part of the last region when it is the next
/// rows of the same columns.
fn extend(runs: &mut Vec<CursorRegion>, bounds: Bounds<Pixels>, style: CursorStyle) {
    if let Some(last) = runs.last_mut()
        && last.style == style
        && last.bounds.origin.x == bounds.origin.x
        && last.bounds.size.width == bounds.size.width
        && (bounds.origin.y - last.bounds.bottom()).abs() < px(SEAM)
    {
        last.bounds.size.height = bounds.bottom() - last.bounds.origin.y;
        return;
    }
    runs.push(CursorRegion { bounds, style });
}

/// The x ranges of the code of a row that a press starts a selection in:
/// after the gutter and up to the end of the pane.
fn text_spans(row: Row, layout: Layout, columns: Columns) -> [Option<(Pixels, Pixels)>; 2] {
    let span = |side| {
        let left = columns.pane_left(layout, side) + columns.gutter(layout);
        let right = match (layout, side) {
            (Layout::Split, Side::Old) => columns.split,
            _ => columns.width,
        };
        (right > left).then_some((left, right))
    };
    match row {
        Row::Pair { .. } if layout == Layout::Split => {
            let side = |side| code_line(row, Some(side)).and_then(|_| span(side));
            [side(Side::Old), side(Side::New)]
        }
        _ => match code_line(row, None) {
            Some(line) => [span(line.side), None],
            None => [None, None],
        },
    }
}

impl DiffView {
    /// The cursor to hold wherever the pointer is while a drag goes on.
    pub(super) fn drag_cursor(&self) -> Option<CursorStyle> {
        if self.is_resizing() {
            Some(CursorStyle::ResizeLeftRight)
        } else if self.is_selecting() {
            Some(CursorStyle::IBeam)
        } else {
            None
        }
    }

    /// The cursor regions of a view `view` big, as of the rows now at the
    /// top: the I-beam over code a drag selects, the split divider, the
    /// click targets of the file headers and, over all of those, the pinned
    /// header.
    pub(super) fn cursor_map(&self, view: Size<Pixels>) -> CursorMap {
        let mut map = CursorMap::default();
        let Some(columns) = self.columns else {
            return map;
        };
        let (list, layout) = (&self.list, self.list.layout());
        let scroll = self.scroll_y();
        let clip = Bounds::new(point(px(0.0), px(0.0)), size(columns.width, view.height));
        // Each pane's code and the headers' click targets grow by rows.
        let mut text: [Vec<CursorRegion>; 2] = Default::default();
        let mut headers = Vec::new();
        if !list.is_empty() {
            for row in list.row_at(scroll).row..list.len() {
                let top = px((list.top_of(row) - scroll) as f32);
                if top >= view.height {
                    break;
                }
                let height = px(list.row_height(row) as f32);
                let at = |x: Pixels, width: Pixels| {
                    Bounds::new(point(x, top), size(width, height)).intersect(&clip)
                };
                match list.rows()[row] {
                    Row::FileHeader { file } => {
                        let height = self.style.file_header_height;
                        let zone = self.viewed_zone(columns);
                        // The chevron, the space after the path and the
                        // checkbox act; the path of an inline header does not.
                        let path = self.path_span(file).map_or(zone.left, |(_, right)| right);
                        for (left, right) in [
                            (px(0.0), path_left(columns, height)),
                            (path, zone.left),
                            (zone.left, columns.width),
                        ] {
                            let bounds = at(left, (right - left).max(px(0.0)));
                            extend(&mut headers, bounds, CursorStyle::PointingHand);
                        }
                    }
                    row @ (Row::HunkHeader { .. } | Row::Expander { .. }) => {
                        let count = self.row_arrows(row).map_or(0, |(.., arrows)| arrows.len());
                        for ix in 0..count {
                            let (left, width) = arrow_cell(columns, layout, ix, count);
                            extend(&mut headers, at(left, width), CursorStyle::PointingHand);
                        }
                    }
                    row => {
                        for (runs, span) in text.iter_mut().zip(text_spans(row, layout, columns)) {
                            if let Some((left, right)) = span {
                                extend(runs, at(left, right - left), CursorStyle::IBeam);
                            }
                        }
                    }
                }
            }
        }
        map.regions.extend(text.into_iter().flatten());
        if let Some(divider) = self.divider_bounds(Bounds::new(point(px(0.0), px(0.0)), view)) {
            map.regions.push(CursorRegion {
                bounds: divider,
                style: CursorStyle::ResizeLeftRight,
            });
        }
        map.regions.extend(headers);
        map.over_pinned = map.regions.len();
        map.pinned = self.pinned_bounds(Bounds::new(point(px(0.0), px(0.0)), view));
        if let Some(pinned) = map.pinned {
            map.regions.push(CursorRegion {
                bounds: pinned,
                style: CursorStyle::PointingHand,
            });
        }
        map
    }

    /// The cursor at `point` (relative to the view's top left) for a view
    /// `view` big, with no drag going on.
    #[cfg(test)]
    pub(super) fn cursor_at(&self, point: gpui::Point<Pixels>, view: Size<Pixels>) -> CursorStyle {
        self.drag_cursor()
            .unwrap_or_else(|| self.cursor_map(view).at(point))
    }
}

#[cfg(test)]
mod tests {
    use diff::{Layout, Row};
    use gpui::{CursorStyle, Pixels, Size, point, px, size};

    use super::*;
    use crate::view::files::tests::view;

    const VIEW: Size<Pixels> = size(px(620.0), px(300.0));

    fn at(view: &DiffView, x: f32, y: f32) -> CursorStyle {
        view.cursor_at(point(px(x), px(y)), VIEW)
    }

    /// The y of the first row that is `wanted`, in a view at the top.
    fn row_y(view: &DiffView, wanted: impl Fn(Row) -> bool) -> f32 {
        let row = view
            .list
            .rows()
            .iter()
            .position(|&row| wanted(row))
            .expect("row");
        view.list.top_of(row) as f32 + 5.0
    }

    #[test]
    fn unified_code_is_a_beam_and_the_gutter_an_arrow() {
        let view = view(Layout::Unified, 1, Vec::new());
        let code = row_y(&view, |row| matches!(row, Row::Line { .. }));
        // Gutter: two number columns of 24.
        assert_eq!(at(&view, 10.0, code), CursorStyle::Arrow);
        assert_eq!(at(&view, 47.0, code), CursorStyle::Arrow);
        assert_eq!(at(&view, 48.0, code), CursorStyle::IBeam);
        assert_eq!(at(&view, 400.0, code), CursorStyle::IBeam);
        // Past the code, where the scrollbar is.
        assert_eq!(at(&view, 610.0, code), CursorStyle::Arrow);
        let hunk = row_y(&view, |row| matches!(row, Row::HunkHeader { .. }));
        assert_eq!(at(&view, 100.0, hunk), CursorStyle::Arrow);
    }

    #[test]
    fn the_cursor_agrees_with_where_a_press_starts_a_selection() {
        for layout in [Layout::Unified, Layout::Split] {
            let mut view = view(layout, 2, Vec::new());
            let height = view.list.total_height() as f32;
            let mut y = 1.0;
            while y < height {
                // Off the divider, which no press selects on.
                for x in [5.0, 30.0, 60.0, 150.0, 290.0, 330.0, 360.0, 450.0, 590.0] {
                    let on = point(px(x), px(y));
                    let selects = view.press_text(on, 1, false);
                    view.release_text();
                    view.clear_selection();
                    assert_eq!(
                        view.cursor_at(on, size(px(620.0), px(2000.0))) == CursorStyle::IBeam,
                        selects,
                        "{layout:?} at {x},{y}"
                    );
                }
                y += 7.0;
            }
        }
    }

    #[test]
    fn split_halves_stop_at_the_divider_and_keep_their_gutters() {
        let view = view(Layout::Split, 1, Vec::new());
        let code = row_y(&view, |row| matches!(row, Row::Pair { .. }));
        assert_eq!(at(&view, 10.0, code), CursorStyle::Arrow);
        assert_eq!(at(&view, 24.0, code), CursorStyle::IBeam);
        assert_eq!(at(&view, 250.0, code), CursorStyle::IBeam);
        // The divider's grab area takes over from the old pane's code...
        assert_eq!(at(&view, 298.0, code), CursorStyle::ResizeLeftRight);
        assert_eq!(at(&view, 302.0, code), CursorStyle::ResizeLeftRight);
        // ...and the new pane has its own gutter.
        assert_eq!(at(&view, 310.0, code), CursorStyle::Arrow);
        assert_eq!(at(&view, 330.0, code), CursorStyle::IBeam);
    }

    #[test]
    fn the_divider_is_a_resize_in_split_only() {
        let split = view(Layout::Split, 1, Vec::new());
        let unified = view(Layout::Unified, 1, Vec::new());
        // Over the hunk header, which has no code to beam over.
        let hunk = row_y(&split, |row| matches!(row, Row::HunkHeader { .. }));
        assert_eq!(at(&split, 300.0, hunk), CursorStyle::ResizeLeftRight);
        assert_eq!(at(&split, 296.0, hunk), CursorStyle::Arrow);
        assert_eq!(at(&unified, 300.0, hunk), CursorStyle::Arrow);
    }

    #[test]
    fn a_drag_holds_its_cursor_anywhere() {
        let mut view = view(Layout::Split, 1, Vec::new());
        let code = row_y(&view, |row| matches!(row, Row::Pair { .. }));
        assert!(view.press_divider(point(px(300.0), px(code)), 1).is_some());
        assert_eq!(at(&view, 10.0, code), CursorStyle::ResizeLeftRight);
        assert_eq!(at(&view, 500.0, 5.0), CursorStyle::ResizeLeftRight);
        view.release_divider();
        assert_eq!(at(&view, 10.0, code), CursorStyle::Arrow);
        assert!(view.press_text(point(px(100.0), px(code)), 1, false));
        assert_eq!(at(&view, 10.0, code), CursorStyle::IBeam);
        assert_eq!(at(&view, 500.0, 5.0), CursorStyle::IBeam);
        view.release_text();
        assert_eq!(at(&view, 10.0, code), CursorStyle::Arrow);
    }

    #[test]
    fn the_inline_header_is_a_hand_except_over_its_path() {
        let view = view(Layout::Unified, 2, Vec::new());
        let header = row_y(&view, |row| matches!(row, Row::FileHeader { .. }));
        let zone = view.viewed_zone(view.columns.unwrap());
        let left = f32::from(zone.left);
        assert_eq!(at(&view, left + 12.0, header), CursorStyle::PointingHand);
        assert_eq!(at(&view, 590.0, header), CursorStyle::PointingHand);
        // The chevron and the empty space fold the file; the path (f0.rs,
        // from x 25 to 65) does nothing inline.
        assert_eq!(at(&view, 8.0, header), CursorStyle::PointingHand);
        assert_eq!(at(&view, 40.0, header), CursorStyle::Arrow);
        assert_eq!(at(&view, 70.0, header), CursorStyle::PointingHand);
        assert_eq!(at(&view, left - 2.0, header), CursorStyle::PointingHand);
        // The same for the second file's header, further down.
        let second = view.list.top_of(view.list.file_rows()[1] as usize) as f32;
        assert_eq!(
            at(&view, left + 12.0, second + 5.0),
            CursorStyle::PointingHand
        );
        assert_eq!(at(&view, 40.0, second + 5.0), CursorStyle::Arrow);
        assert_eq!(at(&view, 8.0, second + 5.0), CursorStyle::PointingHand);
    }

    #[test]
    fn the_pinned_header_is_a_hand_and_hides_what_is_under_it() {
        let mut view = view(Layout::Split, 2, Vec::new());
        let second = view.list.top_of(view.list.file_rows()[1] as usize);
        view.set_scroll_y(second + 60.0);
        assert!(view.pinned().is_some());
        // The pinned header's path and checkbox, over code, the divider and
        // the gutter of the rows under it.
        for x in [5.0, 100.0, 300.0, 500.0] {
            assert_eq!(at(&view, x, 10.0), CursorStyle::PointingHand, "x {x}");
        }
        // Past the header, the rows have their own again.
        let below = f32::from(view.style.file_header_height) + 5.0;
        assert_eq!(at(&view, 300.0, below), CursorStyle::ResizeLeftRight);
        assert_eq!(at(&view, 100.0, below), CursorStyle::IBeam);
        // Pushed up by the next header, it covers only what is left.
        view.set_scroll_y(second - 10.0);
        let map = view.cursor_map(VIEW);
        let pinned = map.pinned.expect("pinned");
        assert_eq!(pinned.size.height, px(10.0));
        assert_eq!(at(&view, 100.0, 9.0), CursorStyle::PointingHand);
        // Over the next file's path, which does nothing inline.
        assert_ne!(at(&view, 40.0, 11.0), CursorStyle::PointingHand);
    }

    #[test]
    fn runs_of_rows_make_one_region_and_nothing_leaves_the_view() {
        let view = view(Layout::Unified, 3, Vec::new());
        let small = size(px(620.0), px(100.0));
        let map = view.cursor_map(small);
        assert!(map.regions.len() < 6, "{map:?}");
        for region in &map.regions {
            assert!(region.bounds.bottom() <= small.height);
            assert!(region.bounds.right() <= px(600.0));
        }
        assert_eq!(
            CursorMap::default().at(point(px(5.0), px(5.0))),
            CursorStyle::Arrow
        );
    }
}
