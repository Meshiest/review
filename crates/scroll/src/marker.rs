//! The autoscroll anchor marker and its cursor, shared by every adapter that
//! runs the gesture.

use gpui::{
    BorderStyle, Bounds, CursorStyle, PathBuilder, Pixels, Point, Window, fill, point, px, quad,
    size,
};

use crate::autoscroll::{Axes, Pos};
use crate::style::AutoscrollStyle;

/// Sets the cursor for the axes the gesture scrolls and paints the marker at
/// `anchor`.
pub fn paint_autoscroll_marker(
    style: &AutoscrollStyle,
    anchor: Pos,
    axes: Axes,
    window: &mut Window,
) {
    window.set_window_cursor_style(match (axes.x, axes.y) {
        (true, true) => CursorStyle::Crosshair,
        (true, false) => CursorStyle::ResizeLeftRight,
        _ => CursorStyle::ResizeUpDown,
    });
    paint_marker(
        style,
        point(px(anchor.x as f32), px(anchor.y as f32)),
        axes,
        window,
    );
}

fn paint_marker(style: &AutoscrollStyle, center: Point<Pixels>, axes: Axes, window: &mut Window) {
    let radius = style.diameter / 2.0;
    window.paint_quad(quad(
        Bounds::centered_at(center, size(style.diameter, style.diameter)),
        radius,
        style.fill,
        px(1.0),
        style.border,
        BorderStyle::Solid,
    ));
    let dot = radius * 0.14;
    window.paint_quad(
        fill(
            Bounds::centered_at(center, size(dot * 2.0, dot * 2.0)),
            style.arrow,
        )
        .corner_radii(dot),
    );
    let directions = [
        (axes.y, (0.0, -1.0)),
        (axes.y, (0.0, 1.0)),
        (axes.x, (-1.0, 0.0)),
        (axes.x, (1.0, 0.0)),
    ];
    for (enabled, (dx, dy)) in directions {
        if !enabled {
            continue;
        }
        // A triangle pointing along (dx, dy), its tip near the rim.
        let tip = radius * 0.78;
        let base = tip - radius * 0.32;
        let half = radius * 0.26;
        let at = |along: Pixels, across: Pixels| {
            point(
                center.x + along * dx - across * dy,
                center.y + along * dy + across * dx,
            )
        };
        let mut path = PathBuilder::fill();
        path.move_to(at(tip, px(0.0)));
        path.line_to(at(base, half));
        path.line_to(at(base, -half));
        path.close();
        if let Ok(path) = path.build() {
            window.paint_path(path, style.arrow);
        }
    }
}
