//! Middle-click autoscroll for an element scrolled by a [`ScrollHandle`]
//! (`track_scroll` + `overflow_y_scroll`), the same gesture [`Scroller`]
//! gives a custom-painted view.
//!
//! The view keeps a [`HandleAutoscroll`] next to its handle and adds
//! [`HandleAutoscroll::overlay`] as the last child of a `relative` container
//! around the scrolling element. The overlay draws only the marker and takes
//! no clicks: it listens to the window's mouse events and writes the handle's
//! offset each frame.
//!
//! [`Scroller`]: crate::Scroller

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use gpui::{
    App, DispatchPhase, EntityId, Hitbox, HitboxBehavior, IntoElement, KeystrokeEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollHandle, Styled as _,
    Subscription, Window, canvas, point, px,
};

use crate::autoscroll::{Autoscroll, Axes, Pos};
use crate::glue::StartFilter;
use crate::marker::paint_autoscroll_marker;
use crate::style::AutoscrollStyle;
use crate::time::{Time, now};

/// The gesture's state for one scroll handle. Cloning shares the state.
#[derive(Clone)]
pub struct HandleAutoscroll {
    handle: ScrollHandle,
    inner: Rc<RefCell<Inner>>,
}

#[derive(Default)]
struct Inner {
    auto: Autoscroll,
    start_filter: Option<StartFilter>,
    /// The Escape observer, alive while the gesture is.
    escape: Option<Subscription>,
    /// The window was active when the gesture started, so losing that stops it.
    window_was_active: bool,
}

impl HandleAutoscroll {
    #[must_use]
    pub fn new(handle: ScrollHandle) -> Self {
        Self {
            handle,
            inner: Rc::default(),
        }
    }

    /// Sets the hook that vetoes autoscroll at a point, for middle-click
    /// targets inside the area; see [`StartFilter`].
    pub fn set_start_filter(
        &self,
        filter: impl Fn(Point<Pixels>, &Window, &App) -> bool + 'static,
    ) {
        self.inner.borrow_mut().start_filter = Some(Rc::new(filter));
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.inner.borrow().auto.is_active()
    }

    pub fn stop(&self) {
        self.inner.borrow_mut().stop();
    }

    /// The element to put over the scrolling area: absolutely positioned,
    /// filling its parent.
    pub fn overlay(&self, style: AutoscrollStyle) -> impl IntoElement {
        let this = self.clone();
        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
            move |_, hitbox, window, _| this.paint(&hitbox, &style, window),
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    fn paint(&self, hitbox: &Hitbox, style: &AutoscrollStyle, window: &mut Window) {
        self.advance(window);
        self.listen(hitbox, window.current_view(), window);
        let mut inner = self.inner.borrow_mut();
        if let Some((anchor, axes)) = inner.auto.marker() {
            paint_autoscroll_marker(style, anchor, axes, window);
            window.request_animation_frame();
        } else {
            inner.escape = None;
        }
    }

    /// Moves the handle by the time passed since the last frame.
    fn advance(&self, window: &Window) {
        let mut inner = self.inner.borrow_mut();
        if inner.window_was_active && !window.is_window_active() {
            inner.stop();
        }
        let moved = inner.advance(now(), self.handle.offset(), self.handle.max_offset());
        if let Some(offset) = moved {
            self.handle.set_offset(offset);
        }
    }

    fn listen(&self, hitbox: &Hitbox, view: EntityId, window: &mut Window) {
        let weak = Rc::downgrade(&self.inner);
        window.on_mouse_event({
            let (handle, hitbox, weak) = (self.handle.clone(), hitbox.clone(), weak.clone());
            move |event: &MouseDownEvent, phase, window, cx| {
                let Some(inner) = weak.upgrade() else {
                    return;
                };
                if phase == DispatchPhase::Capture {
                    let mut state = inner.borrow_mut();
                    if state.auto.is_active() {
                        state.stop();
                        cx.notify(view);
                        cx.stop_propagation();
                    }
                    return;
                }
                if event.button != MouseButton::Middle
                    || !hitbox.is_hovered_at(event.position, window)
                {
                    return;
                }
                let filter = inner.borrow().start_filter.clone();
                if filter.is_some_and(|allow| !allow(event.position, window, cx)) {
                    return;
                }
                let mut state = inner.borrow_mut();
                if state.press(event.position, handle.max_offset(), now()) {
                    state.window_was_active = window.is_window_active();
                    state.escape = Some(cx.observe_keystrokes(escape_stops(weak.clone(), view)));
                    cx.notify(view);
                    cx.stop_propagation();
                }
            }
        });
        window.on_mouse_event({
            let weak = weak.clone();
            move |event: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                if let Some(inner) = weak.upgrade() {
                    let mut state = inner.borrow_mut();
                    if state.auto.is_active() {
                        state.moved(event.position);
                        cx.notify(view);
                    }
                }
            }
        });
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture || event.button != MouseButton::Middle {
                return;
            }
            if let Some(inner) = weak.upgrade()
                && inner.borrow_mut().release(now())
            {
                cx.notify(view);
            }
        });
    }
}

fn escape_stops(
    weak: Weak<RefCell<Inner>>,
    view: EntityId,
) -> impl FnMut(&KeystrokeEvent, &mut Window, &mut App) + 'static {
    move |event, _, cx| {
        if event.keystroke.key == "escape"
            && let Some(inner) = weak.upgrade()
        {
            inner.borrow_mut().stop();
            cx.notify(view);
        }
    }
}

impl Inner {
    fn axes(max: Point<Pixels>) -> Axes {
        Axes {
            x: max.x > px(0.0),
            y: max.y > px(0.0),
        }
    }

    fn stop(&mut self) {
        self.auto.stop();
    }

    /// The middle button went down at `position` over an area that scrolls up
    /// to `max`; returns whether the gesture started.
    fn press(&mut self, position: Point<Pixels>, max: Point<Pixels>, now: Time) -> bool {
        self.auto.start(pos(position), Self::axes(max), now)
    }

    fn moved(&mut self, position: Point<Pixels>) {
        self.auto.pointer_moved(pos(position));
    }

    /// The middle button went up; returns whether the gesture was running.
    fn release(&mut self, now: Time) -> bool {
        let was = self.auto.is_active();
        self.auto.release(now);
        was
    }

    /// The offset after the time since the last step, or `None` when it does
    /// not change.
    fn advance(
        &mut self,
        now: Time,
        offset: Point<Pixels>,
        max: Point<Pixels>,
    ) -> Option<Point<Pixels>> {
        if !self.auto.is_active() {
            return None;
        }
        let moved = shifted(offset, max, self.auto.step(now));
        (moved != offset).then_some(moved)
    }
}

fn pos(p: Point<Pixels>) -> Pos {
    Pos {
        x: f64::from(p.x),
        y: f64::from(p.y),
    }
}

/// `offset` moved toward the end by `step`, kept inside the content. A gpui
/// offset runs from zero down to `-max`, so scrolling forward lowers it.
fn shifted(offset: Point<Pixels>, max: Point<Pixels>, step: Pos) -> Point<Pixels> {
    let along = |offset: Pixels, max: Pixels, step: f64| {
        let max = f64::from(max).max(0.0);
        px(-(-f64::from(offset) + step).clamp(0.0, max) as f32)
    };
    point(
        along(offset.x, max.x, step.x),
        along(offset.y, max.y, step.y),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn ms(n: u64) -> Time {
        Duration::from_millis(n)
    }

    fn at(x: f32, y: f32) -> Point<Pixels> {
        point(px(x), px(y))
    }

    fn max() -> Point<Pixels> {
        at(0.0, 2000.0)
    }

    #[test]
    fn scrolling_forward_lowers_the_offset_and_stops_at_the_ends() {
        let step = |dy: f64| Pos { x: 0.0, y: dy };
        assert_eq!(shifted(at(0.0, 0.0), max(), step(30.0)), at(0.0, -30.0));
        assert_eq!(shifted(at(0.0, -30.0), max(), step(-10.0)), at(0.0, -20.0));
        assert_eq!(
            shifted(at(0.0, -1990.0), max(), step(50.0)),
            at(0.0, -2000.0)
        );
        assert_eq!(shifted(at(0.0, -5.0), max(), step(-50.0)), at(0.0, 0.0));
    }

    #[test]
    fn an_axis_with_nothing_to_scroll_stays_put() {
        let moved = shifted(at(0.0, -10.0), at(0.0, 0.0), Pos { x: 40.0, y: 40.0 });
        assert_eq!(moved, at(0.0, 0.0));
        assert_eq!(Inner::axes(at(0.0, 0.0)), Axes { x: false, y: false });
        assert_eq!(Inner::axes(max()), Axes { x: false, y: true });
    }

    #[test]
    fn nothing_starts_where_there_is_nothing_to_scroll() {
        let mut inner = Inner::default();
        assert!(!inner.press(at(10.0, 10.0), at(0.0, 0.0), ms(0)));
        assert!(!inner.auto.is_active());
        assert_eq!(inner.advance(ms(100), at(0.0, 0.0), at(0.0, 0.0)), None);
    }

    #[test]
    fn holding_below_the_anchor_scrolls_down_and_release_ends_it() {
        let mut inner = Inner::default();
        assert!(inner.press(at(100.0, 100.0), max(), ms(0)));
        inner.moved(at(100.0, 220.0));
        let first = inner.advance(ms(50), at(0.0, 0.0), max()).expect("moves");
        assert!(first.y < px(0.0));
        let second = inner.advance(ms(100), first, max()).expect("keeps moving");
        assert!(second.y < first.y);
        assert!(inner.release(ms(1000)));
        assert!(!inner.auto.is_active());
        assert_eq!(inner.advance(ms(1100), second, max()), None);
    }

    #[test]
    fn a_quick_click_keeps_scrolling_until_stopped() {
        let mut inner = Inner::default();
        inner.press(at(100.0, 100.0), max(), ms(0));
        assert!(inner.release(ms(80)));
        assert!(inner.auto.is_active());
        inner.moved(at(100.0, 20.0));
        let up = inner.advance(ms(150), at(0.0, -500.0), max()).expect("up");
        assert!(up.y > px(-500.0));
        inner.stop();
        assert_eq!(inner.advance(ms(300), up, max()), None);
    }

    #[test]
    fn at_the_end_there_is_no_further_change() {
        let mut inner = Inner::default();
        inner.press(at(100.0, 100.0), max(), ms(0));
        inner.moved(at(100.0, 400.0));
        assert_eq!(inner.advance(ms(100), at(0.0, -2000.0), max()), None);
    }
}
