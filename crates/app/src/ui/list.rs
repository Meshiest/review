//! A virtualized list of rows with known heights: only the rows in view are
//! built, laid out and painted. Scrolling, the scrollbar and middle-click
//! autoscroll come from the view's [`Scroller`].

use std::rc::Rc;

use gpui::{
    AnyElement, App, AvailableSpace, Bounds, ContentMask, Context, DispatchPhase, Element,
    ElementId, Entity, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId, IntoElement,
    LayoutId, Pixels, ScrollEvent, Style, Window, point, px, relative, size,
};
use scroll::Scroller;

type RenderRow<V> = Rc<dyn Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement>;

/// Row tops as a running sum: `tops[i]` is where row `i` starts and the last
/// entry is the total height.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowTops(Rc<[f32]>);

impl RowTops {
    pub fn new(heights: impl IntoIterator<Item = f32>) -> Self {
        let mut tops = vec![0.0];
        let mut y = 0.0;
        for height in heights {
            y += height;
            tops.push(y);
        }
        Self(tops.into())
    }

    pub fn len(&self) -> usize {
        self.0.len() - 1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total(&self) -> f32 {
        self.0.last().copied().unwrap_or(0.0)
    }

    pub fn top(&self, row: usize) -> f32 {
        self.0[row.min(self.len())]
    }

    pub fn height(&self, row: usize) -> f32 {
        self.top(row + 1) - self.top(row)
    }

    /// The rows that overlap `y..y + height`.
    pub fn visible(&self, y: f32, height: f32) -> std::ops::Range<usize> {
        let first = self.0.partition_point(|&top| top <= y).saturating_sub(1);
        let end = self.0.partition_point(|&top| top < y + height);
        first.min(self.len())..end.min(self.len())
    }

    /// The offset that brings `row` fully into a viewport of `height` at `y`,
    /// moving as little as possible.
    pub fn reveal(&self, row: usize, y: f64, height: f64) -> f64 {
        let (top, bottom) = (f64::from(self.top(row)), f64::from(self.top(row + 1)));
        if top < y {
            top
        } else if bottom > y + height {
            bottom - height
        } else {
            y
        }
    }
}

/// The element; build it in the view's render with [`virtual_list`].
pub struct VirtualList<V: 'static> {
    view: Entity<V>,
    access: fn(&mut V) -> &mut Scroller,
    tops: RowTops,
    render_row: RenderRow<V>,
    /// What one wheel line scrolls.
    line: Pixels,
}

/// A list of `tops.len()` rows; `render_row` builds row `ix` when it is in view.
pub fn virtual_list<V: 'static>(
    view: Entity<V>,
    access: fn(&mut V) -> &mut Scroller,
    tops: RowTops,
    render_row: impl Fn(&mut V, usize, &mut Window, &mut Context<V>) -> AnyElement + 'static,
) -> VirtualList<V> {
    VirtualList {
        view,
        access,
        tops,
        render_row: Rc::new(render_row),
        line: px(28.0),
    }
}

impl<V: 'static> IntoElement for VirtualList<V> {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

pub struct ListPrepaint {
    hitbox: Hitbox,
    rows: Vec<AnyElement>,
}

impl<V: 'static> Element for VirtualList<V> {
    type RequestLayoutState = ();
    type PrepaintState = ListPrepaint;

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
    ) -> ListPrepaint {
        let access = self.access;
        let total = f64::from(self.tops.total());
        let offset = self.view.update(cx, |view, _| {
            let scroller = access(view);
            scroller.state.y.content = total;
            scroller.frame(bounds, window);
            scroller.state.y.clamp();
            scroller.state.y.offset as f32
        });
        // Under the rows, so a row's own hover and clicks still reach it.
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let range = self.tops.visible(offset, f32::from(bounds.size.height));
        let mut rows = Vec::with_capacity(range.len());
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for ix in range {
                let render = self.render_row.clone();
                let mut row = self
                    .view
                    .update(cx, |view, cx| render(view, ix, window, cx));
                let origin = point(bounds.left(), bounds.top() + px(self.tops.top(ix) - offset));
                let space = size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::Definite(px(self.tops.height(ix))),
                );
                row.prepaint_as_root(origin, space, window, cx);
                rows.push(row);
            }
        });
        ListPrepaint { hitbox, rows }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        state: &mut ListPrepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let access = self.access;
        scroll::install(&self.view, &state.hitbox, access, window);
        let view = self.view.clone();
        let (hitbox, line) = (state.hitbox.id, self.line);
        window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || !hitbox.should_handle_scroll(window) {
                return;
            }
            view.update(cx, |view, cx| {
                let delta = event.delta.pixel_delta(line);
                if access(view).state.y.by(-f64::from(delta.y)) {
                    cx.notify();
                }
            });
            cx.stop_propagation();
        });
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for row in &mut state.rows {
                row.paint(window, cx);
            }
        });
        let view = self.view.clone();
        view.update(cx, |view, _| access(view).paint(window));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_rows_cover_the_viewport() {
        let tops = RowTops::new([28.0, 46.0, 46.0, 28.0, 46.0]);
        assert_eq!(tops.len(), 5);
        assert_eq!(tops.total(), 194.0);
        assert_eq!(tops.visible(0.0, 50.0), 0..2);
        assert_eq!(tops.visible(30.0, 46.0), 1..3);
        assert_eq!(tops.visible(74.0, 1.0), 2..3);
        assert_eq!(tops.visible(500.0, 50.0), 5..5);
        assert!(RowTops::new([]).visible(0.0, 100.0).is_empty());
    }

    #[test]
    fn reveal_moves_as_little_as_possible() {
        let tops = RowTops::new([10.0; 20]);
        assert_eq!(tops.reveal(5, 0.0, 100.0), 0.0);
        assert_eq!(tops.reveal(12, 0.0, 100.0), 30.0);
        assert_eq!(tops.reveal(2, 50.0, 100.0), 20.0);
    }
}
