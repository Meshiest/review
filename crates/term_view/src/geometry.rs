//! Where the grid is on screen, and which cell a pixel falls in.

use gpui::{Bounds, Pixels, Point, Size, point, size};
use term::alacritty_terminal::index::Side;

/// The most lines a drag outside the pane scrolls at once.
const MAX_EDGE_LINES: i32 = 6;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    /// The grid: the pane less its padding. Cells are counted from here.
    pub bounds: Bounds<Pixels>,
    /// The whole pane, padding included: a drag scrolls only past its edges.
    pub pane: Bounds<Pixels>,
    pub cell: Size<Pixels>,
    pub cols: usize,
    pub rows: usize,
}

/// Where the grid sits in a pane with `padding` on every side; never
/// smaller than nothing.
pub(crate) fn grid_bounds(pane: Bounds<Pixels>, padding: Pixels) -> Bounds<Pixels> {
    let inset = |length: Pixels| (length - padding * 2.0).max(Pixels::ZERO);
    Bounds::new(
        pane.origin + point(padding, padding),
        size(inset(pane.size.width), inset(pane.size.height)),
    )
}

/// A cell of the visible grid and the half of it a point is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hit {
    pub row: usize,
    pub col: usize,
    pub side: Side,
}

/// Where a point is relative to the pane, for scrolling while dragging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Edge {
    Inside,
    /// Lines to scroll toward the history.
    Above(i32),
    /// Lines to scroll toward the present.
    Below(i32),
}

impl Geometry {
    /// The cell under `position`, clamped to the grid.
    pub(crate) fn locate(&self, position: Point<Pixels>) -> Hit {
        let across = (position.x - self.bounds.origin.x) / self.cell.width;
        let down = (position.y - self.bounds.origin.y) / self.cell.height;
        let last_col = self.cols.saturating_sub(1);
        let (col, side) = if across < 0.0 {
            (0, Side::Left)
        } else if across >= self.cols as f32 {
            (last_col, Side::Right)
        } else {
            let col = across.floor() as usize;
            let side = if across - across.floor() < 0.5 {
                Side::Left
            } else {
                Side::Right
            };
            (col.min(last_col), side)
        };
        let row = (down.max(0.0).floor() as usize).min(self.rows.saturating_sub(1));
        Hit { row, col, side }
    }

    /// The bounds of the cell at `row` and `col` of the visible grid.
    pub(crate) fn cell_bounds(&self, row: usize, col: usize) -> Bounds<Pixels> {
        let origin =
            self.bounds.origin + point(self.cell.width * col as f32, self.cell.height * row as f32);
        Bounds::new(origin, self.cell)
    }

    pub(crate) fn edge(&self, position: Point<Pixels>) -> Edge {
        let lines = |distance: Pixels| {
            ((distance / self.cell.height).ceil() as i32).clamp(1, MAX_EDGE_LINES)
        };
        if position.y < self.pane.top() {
            Edge::Above(lines(self.pane.top() - position.y))
        } else if position.y > self.pane.bottom() {
            Edge::Below(lines(position.y - self.pane.bottom()))
        } else {
            Edge::Inside
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{point, px, size};

    use super::*;

    fn geometry() -> Geometry {
        let bounds = Bounds::new(point(px(10.), px(20.)), size(px(80.), px(48.)));
        Geometry {
            bounds,
            pane: bounds,
            cell: size(px(8.), px(16.)),
            cols: 10,
            rows: 3,
        }
    }

    /// A 100 by 70 pane with 10 px of padding: a 10 by 3 grid of 8 by 16
    /// cells starting at (10, 10).
    fn padded() -> Geometry {
        let pane = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(70.)));
        let bounds = grid_bounds(pane, px(10.));
        let cell = size(px(8.), px(16.));
        Geometry {
            bounds,
            pane,
            cell,
            cols: crate::frame::fit(bounds.size.width, cell.width).into(),
            rows: crate::frame::fit(bounds.size.height, cell.height).into(),
        }
    }

    #[test]
    fn padding_insets_the_grid_and_the_cell_count() {
        let g = padded();
        assert_eq!(
            g.bounds,
            Bounds::new(point(px(10.), px(10.)), size(px(80.), px(50.)))
        );
        assert_eq!((g.cols, g.rows), (10, 3));
        assert_eq!(g.cell_bounds(0, 0).origin, point(px(10.), px(10.)));
        let pane = Bounds::new(point(px(0.), px(0.)), size(px(12.), px(12.)));
        assert_eq!(grid_bounds(pane, px(10.)).size, size(px(0.), px(0.)));
    }

    #[test]
    fn with_padding_a_point_still_maps_to_its_cell() {
        let g = padded();
        let hit = g.locate(point(px(10. + 8. * 3. + 1.), px(10. + 16. + 1.)));
        assert_eq!((hit.row, hit.col, hit.side), (1, 3, Side::Left));
        for (row, col) in [(0, 0), (1, 5), (2, 9)] {
            let hit = g.locate(g.cell_bounds(row, col).center());
            assert_eq!((hit.row, hit.col), (row, col));
        }
    }

    #[test]
    fn a_press_in_the_padding_takes_the_nearest_cell_and_does_not_scroll() {
        let g = padded();
        let hit = g.locate(point(px(2.), px(2.)));
        assert_eq!((hit.row, hit.col, hit.side), (0, 0, Side::Left));
        let hit = g.locate(point(px(98.), px(68.)));
        assert_eq!((hit.row, hit.col, hit.side), (2, 9, Side::Right));
        assert_eq!(g.edge(point(px(50.), px(5.))), Edge::Inside);
        assert_eq!(g.edge(point(px(50.), px(65.))), Edge::Inside);
        assert_eq!(g.edge(point(px(50.), px(71.))), Edge::Below(1));
    }

    #[test]
    fn a_point_maps_to_a_cell_and_a_half() {
        let g = geometry();
        let hit = g.locate(point(px(10. + 8. * 2. + 1.), px(20. + 16. + 3.)));
        assert_eq!((hit.row, hit.col, hit.side), (1, 2, Side::Left));
        let hit = g.locate(point(px(10. + 8. * 2. + 6.), px(21.)));
        assert_eq!((hit.row, hit.col, hit.side), (0, 2, Side::Right));
    }

    #[test]
    fn a_cell_has_its_own_bounds_and_locates_back_to_itself() {
        let g = geometry();
        let bounds = g.cell_bounds(2, 4);
        assert_eq!(bounds.origin, point(px(10. + 8. * 4.), px(20. + 16. * 2.)));
        assert_eq!(bounds.size, g.cell);
        let hit = g.locate(bounds.center());
        assert_eq!((hit.row, hit.col), (2, 4));
    }

    #[test]
    fn points_outside_clamp_to_the_nearest_edge_cell() {
        let g = geometry();
        let hit = g.locate(point(px(0.), px(0.)));
        assert_eq!((hit.row, hit.col, hit.side), (0, 0, Side::Left));
        let hit = g.locate(point(px(500.), px(500.)));
        assert_eq!((hit.row, hit.col, hit.side), (2, 9, Side::Right));
    }

    #[test]
    fn dragging_past_an_edge_scrolls_by_the_distance() {
        let g = geometry();
        assert_eq!(g.edge(point(px(20.), px(30.))), Edge::Inside);
        assert_eq!(g.edge(point(px(20.), px(19.))), Edge::Above(1));
        assert_eq!(g.edge(point(px(20.), px(0.))), Edge::Above(2));
        assert_eq!(g.edge(point(px(20.), px(68. + 40.))), Edge::Below(3));
        assert_eq!(
            g.edge(point(px(20.), px(5000.))),
            Edge::Below(MAX_EDGE_LINES)
        );
    }
}
