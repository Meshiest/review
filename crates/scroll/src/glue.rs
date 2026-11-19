//! The gpui side: a [`Scroller`] a view owns, painting, and mouse input.

use std::rc::Rc;

use gpui::{
    App, Bounds, DispatchPhase, Entity, Hitbox, Hsla, MouseButton, MouseDownEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, Subscription, Window, fill, point, px, size,
};

use crate::autoscroll::{Autoscroll, Axes, Pos};
use crate::axis::{ScrollAxis, ScrollState};
use crate::marker::paint_autoscroll_marker;
use crate::scrollbar::{Press, ScrollbarState, Track, thumb};
use crate::style::{ScrollStyle, mix};
use crate::time::now;

/// The direction a scrollbar runs in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    Vertical,
    Horizontal,
}

impl Orientation {
    const ALL: [Self; 2] = [Self::Vertical, Self::Horizontal];

    fn index(self) -> usize {
        match self {
            Self::Vertical => 0,
            Self::Horizontal => 1,
        }
    }
}

/// Decides whether a middle press at a point may start autoscroll. A view
/// with a text input returns false over it, where middle click pastes.
pub type StartFilter = Rc<dyn Fn(Point<Pixels>, &Window, &App) -> bool>;

/// What a mouse event did, for the code that dispatched it.
#[derive(Clone, Copy, Debug, Default)]
struct Reaction {
    /// The view must redraw.
    notify: bool,
    /// The event was used and must not reach other handlers.
    consume: bool,
    autoscroll_started: bool,
}

/// Scroll position, scrollbars and autoscroll of one view.
pub struct Scroller {
    /// The offsets; the view sets `content` before each [`Scroller::frame`].
    pub state: ScrollState,
    style: ScrollStyle,
    bars: [ScrollbarState; 2],
    auto: Autoscroll,
    /// The scroll area as of the last frame.
    bounds: Bounds<Pixels>,
    start_filter: Option<StartFilter>,
    /// Escape and focus loss observers, alive while autoscroll is.
    subscriptions: Vec<Subscription>,
}

impl Scroller {
    #[must_use]
    pub fn new(style: ScrollStyle) -> Self {
        Self {
            state: ScrollState::default(),
            style,
            bars: Default::default(),
            auto: Autoscroll::default(),
            bounds: Bounds::default(),
            start_filter: None,
            subscriptions: Vec::new(),
        }
    }

    pub fn set_style(&mut self, style: ScrollStyle) {
        self.style = style;
    }

    /// Sets the hook that vetoes autoscroll at a point; see [`StartFilter`].
    pub fn set_start_filter(
        &mut self,
        filter: impl Fn(Point<Pixels>, &Window, &App) -> bool + 'static,
    ) {
        self.start_filter = Some(Rc::new(filter));
    }

    #[must_use]
    pub fn is_autoscrolling(&self) -> bool {
        self.auto.is_active()
    }

    pub fn stop_autoscroll(&mut self) {
        self.auto.stop();
    }

    /// True while a scrollbar thumb is being dragged.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.bars.iter().any(ScrollbarState::is_dragging)
    }

    /// Advances time-driven scrolling to now: autoscroll, held track clicks
    /// and the hover animation. Call it first when preparing to paint, after
    /// setting the content lengths. Asks for another frame while anything is
    /// moving. Returns whether an offset changed.
    pub fn frame(&mut self, bounds: Bounds<Pixels>, window: &Window) -> bool {
        self.bounds = bounds;
        self.state.x.viewport = f64::from(bounds.size.width);
        self.state.y.viewport = f64::from(bounds.size.height);
        let now = now();
        let step = self.auto.step(now);
        let mut changed = self.state.x.by(step.x);
        changed |= self.state.y.by(step.y);
        for o in Orientation::ALL {
            let track = self.track(o);
            let axis = axis_of(&mut self.state, o);
            changed |= self.bars[o.index()].tick(now, track, axis);
        }
        let moving = self.auto.is_active() || self.bars.iter().any(ScrollbarState::wants_frame);
        if moving {
            window.request_animation_frame();
        }
        if !self.auto.is_active() {
            self.subscriptions.clear();
        }
        changed
    }

    /// Paints the scrollbars and the autoscroll marker over the content.
    pub fn paint(&self, window: &mut Window) {
        for o in Orientation::ALL {
            self.paint_bar(o, window);
        }
        if let Some((anchor, axes)) = self.auto.marker() {
            paint_autoscroll_marker(&self.style.autoscroll, anchor, axes, window);
        }
    }

    fn axis(&self, o: Orientation) -> &ScrollAxis {
        match o {
            Orientation::Vertical => &self.state.y,
            Orientation::Horizontal => &self.state.x,
        }
    }

    fn extent(&self, o: Orientation) -> Pixels {
        match o {
            Orientation::Vertical => self.bounds.size.height,
            Orientation::Horizontal => self.bounds.size.width,
        }
    }

    fn other(o: Orientation) -> Orientation {
        match o {
            Orientation::Vertical => Orientation::Horizontal,
            Orientation::Horizontal => Orientation::Vertical,
        }
    }

    /// The track's length: the area less the corner the other bar occupies.
    fn track(&self, o: Orientation) -> Track {
        let corner = if self.axis(Self::other(o)).overflows() {
            self.style.scrollbar.width
        } else {
            px(0.0)
        };
        Track {
            len: f64::from((self.extent(o) - corner).max(px(0.0))),
            min_thumb: f64::from(self.style.scrollbar.min_thumb),
        }
    }

    /// The strip the bar occupies when it is `thickness` wide.
    fn zone(&self, o: Orientation, thickness: Pixels) -> Bounds<Pixels> {
        let len = px(self.track(o).len as f32);
        match o {
            Orientation::Vertical => Bounds::new(
                point(self.bounds.right() - thickness, self.bounds.top()),
                size(thickness, len),
            ),
            Orientation::Horizontal => Bounds::new(
                point(self.bounds.left(), self.bounds.bottom() - thickness),
                size(len, thickness),
            ),
        }
    }

    /// Whether `p` is where the bar of `o` can be grabbed: the widest it gets,
    /// so hovering and pressing agree.
    fn in_zone(&self, o: Orientation, p: Point<Pixels>) -> bool {
        self.axis(o).overflows() && self.zone(o, self.style.scrollbar.hover_width).contains(&p)
    }

    fn along(&self, o: Orientation, p: Point<Pixels>) -> f64 {
        match o {
            Orientation::Vertical => f64::from(p.y - self.bounds.top()),
            Orientation::Horizontal => f64::from(p.x - self.bounds.left()),
        }
    }

    fn paint_bar(&self, o: Orientation, window: &mut Window) {
        let Some(thumb) = thumb(self.axis(o), self.track(o)) else {
            return;
        };
        let style = &self.style.scrollbar;
        let bar = &self.bars[o.index()];
        let amount = bar.hover_amount() as f32;
        let thickness = style.width + (style.hover_width - style.width) * amount;
        let zone = self.zone(o, thickness);
        let track = Hsla {
            a: style.track.a * amount,
            ..style.track
        };
        if track.a > 0.0 {
            window.paint_quad(fill(zone, track));
        }
        let (start, len) = (px(thumb.start as f32), px(thumb.len as f32));
        let bounds = match o {
            Orientation::Vertical => {
                Bounds::new(zone.origin + point(px(0.0), start), size(thickness, len))
            }
            Orientation::Horizontal => {
                Bounds::new(zone.origin + point(start, px(0.0)), size(len, thickness))
            }
        };
        let color = if bar.is_dragging() {
            style.thumb_active
        } else {
            mix(style.thumb, style.thumb_hover, amount)
        };
        window.paint_quad(fill(bounds, color).corner_radii(thickness / 2.0));
    }

    fn press_bar(&mut self, position: Point<Pixels>) -> bool {
        let now = now();
        for o in Orientation::ALL {
            if !self.in_zone(o, position) {
                continue;
            }
            let (along, track) = (self.along(o, position), self.track(o));
            let axis = axis_of(&mut self.state, o);
            if self.bars[o.index()].press(along, track, axis, now) != Press::Miss {
                return true;
            }
        }
        false
    }

    /// Whether the start filter lets a middle press at `position` start
    /// autoscroll.
    fn allows_autoscroll(&self, position: Point<Pixels>, window: &Window, cx: &App) -> bool {
        self.start_filter
            .as_ref()
            .is_none_or(|allow| allow(position, window, cx))
    }

    fn mouse_down(
        &mut self,
        event: &MouseDownEvent,
        phase: DispatchPhase,
        hovered: bool,
        may_start: bool,
    ) -> Reaction {
        let mut reaction = Reaction::default();
        match phase {
            DispatchPhase::Capture => {
                if self.auto.is_active() {
                    self.auto.stop();
                    reaction.notify = true;
                    reaction.consume = true;
                } else if event.button == MouseButton::Left && hovered {
                    let used = self.press_bar(event.position);
                    reaction.notify = used;
                    reaction.consume = used;
                }
            }
            DispatchPhase::Bubble => {
                if event.button == MouseButton::Middle
                    && hovered
                    && !Orientation::ALL
                        .iter()
                        .any(|&o| self.in_zone(o, event.position))
                    && may_start
                {
                    let axes = Axes {
                        x: self.state.x.overflows(),
                        y: self.state.y.overflows(),
                    };
                    let anchor = Pos {
                        x: f64::from(event.position.x),
                        y: f64::from(event.position.y),
                    };
                    let started = self.auto.start(anchor, axes, now());
                    reaction = Reaction {
                        notify: started,
                        consume: started,
                        autoscroll_started: started,
                    };
                }
            }
        }
        reaction
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent, hovered: bool) -> bool {
        let now = now();
        let position = event.position;
        self.auto.pointer_moved(Pos {
            x: f64::from(position.x),
            y: f64::from(position.y),
        });
        let mut notify = false;
        for o in Orientation::ALL {
            let (along, track) = (self.along(o, position), self.track(o));
            let over = hovered && self.in_zone(o, position);
            let axis = axis_of(&mut self.state, o);
            let bar = &mut self.bars[o.index()];
            if bar.is_dragging() && event.pressed_button != Some(MouseButton::Left) {
                // The release happened outside the window.
                bar.release(now);
                notify = true;
            }
            notify |= bar.pointer_moved(along, track, axis);
            if over != bar.is_hovered() {
                bar.set_hovered(over, now);
                notify = true;
            }
        }
        notify
    }

    fn mouse_up(&mut self, event: &MouseUpEvent) -> bool {
        let now = now();
        match event.button {
            MouseButton::Left => {
                let was = self.bars.iter().any(|b| b.is_dragging() || b.is_paging());
                for bar in &mut self.bars {
                    bar.release(now);
                }
                was
            }
            MouseButton::Middle => {
                let was = self.auto.is_active();
                self.auto.release(now);
                was
            }
            _ => false,
        }
    }

    fn mouse_exit(&mut self) -> bool {
        let now = now();
        let mut notify = false;
        for bar in &mut self.bars {
            if bar.is_hovered() {
                bar.set_hovered(false, now);
                notify = true;
            }
        }
        notify
    }
}

fn axis_of(state: &mut ScrollState, o: Orientation) -> &mut ScrollAxis {
    match o {
        Orientation::Vertical => &mut state.y,
        Orientation::Horizontal => &mut state.x,
    }
}

/// Registers the mouse listeners of the view's [`Scroller`] for the frame.
/// Call it from the view's element `paint`, with the hitbox of the scroll
/// area; `access` finds the scroller in the view. Drag, paging and the
/// pointer follow the window, so a drag survives leaving the area. Any button
/// press, Escape and the window losing focus stop autoscroll.
pub fn install<V: 'static>(
    view: &Entity<V>,
    hitbox: &Hitbox,
    access: fn(&mut V) -> &mut Scroller,
    window: &mut Window,
) {
    let id = hitbox.id;
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            let hovered = id.is_hovered(window);
            let reaction = view.update(cx, |v, cx| {
                let scroller = access(v);
                let may_start = event.button == MouseButton::Middle
                    && scroller.allows_autoscroll(event.position, window, cx);
                let reaction = scroller.mouse_down(event, phase, hovered, may_start);
                if reaction.autoscroll_started {
                    let activation = cx.observe_window_activation(window, move |v, window, cx| {
                        if !window.is_window_active() {
                            access(v).stop_autoscroll();
                            cx.notify();
                        }
                    });
                    let escape = cx.observe_keystrokes(move |v, event, _, cx| {
                        if event.keystroke.key == "escape" {
                            access(v).stop_autoscroll();
                            cx.notify();
                        }
                    });
                    access(v).subscriptions = vec![activation, escape];
                }
                if reaction.notify {
                    cx.notify();
                }
                reaction
            });
            if reaction.consume {
                cx.stop_propagation();
            }
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            let hovered = id.is_hovered(window);
            view.update(cx, |v, cx| {
                if access(v).mouse_move(event, hovered) {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |event: &MouseUpEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            view.update(cx, |v, cx| {
                if access(v).mouse_up(event) {
                    cx.notify();
                }
            });
        }
    });
    window.on_mouse_event({
        let view = view.clone();
        move |_: &MouseExitEvent, phase, _, cx| {
            if phase != DispatchPhase::Capture {
                return;
            }
            view.update(cx, |v, cx| {
                if access(v).mouse_exit() {
                    cx.notify();
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scroller(content_x: f64, content_y: f64) -> Scroller {
        let mut scroller = Scroller::new(ScrollStyle::default());
        scroller.bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(400.0), px(300.0)));
        scroller.state.x = ScrollAxis {
            offset: 0.0,
            content: content_x,
            viewport: 400.0,
        };
        scroller.state.y = ScrollAxis {
            offset: 0.0,
            content: content_y,
            viewport: 300.0,
        };
        scroller
    }

    fn down(button: MouseButton, x: f32, y: f32) -> MouseDownEvent {
        MouseDownEvent {
            button,
            position: point(px(x), px(y)),
            ..Default::default()
        }
    }

    fn moved(x: f32, y: f32, pressed: Option<MouseButton>) -> MouseMoveEvent {
        MouseMoveEvent {
            position: point(px(x), px(y)),
            pressed_button: pressed,
            ..Default::default()
        }
    }

    fn up(button: MouseButton) -> MouseUpEvent {
        MouseUpEvent {
            button,
            ..Default::default()
        }
    }

    #[test]
    fn dragging_the_thumb_follows_the_pointer_until_release() {
        let mut s = scroller(0.0, 3000.0);
        // Thumb is 30 px at the top of the 300 px track; grab its middle.
        let r = s.mouse_down(
            &down(MouseButton::Left, 396.0, 15.0),
            DispatchPhase::Capture,
            true,
            false,
        );
        assert!(r.consume && r.notify);
        assert!(s.is_dragging());
        assert!(s.mouse_move(&moved(396.0, 135.0, Some(MouseButton::Left)), true));
        assert_eq!(s.state.y.offset, 120.0 / 270.0 * s.state.y.max());
        assert!(s.mouse_up(&up(MouseButton::Left)));
        assert!(!s.is_dragging());
        let at = s.state.y.offset;
        s.mouse_move(&moved(396.0, 250.0, Some(MouseButton::Left)), true);
        assert_eq!(s.state.y.offset, at);
    }

    #[test]
    fn a_missed_release_ends_the_drag_on_the_next_move() {
        let mut s = scroller(0.0, 3000.0);
        s.mouse_down(
            &down(MouseButton::Left, 396.0, 15.0),
            DispatchPhase::Capture,
            true,
            false,
        );
        s.mouse_move(&moved(100.0, 100.0, None), true);
        assert!(!s.is_dragging());
    }

    #[test]
    fn clicking_the_track_pages_toward_the_click() {
        let mut s = scroller(0.0, 3000.0);
        let r = s.mouse_down(
            &down(MouseButton::Left, 396.0, 250.0),
            DispatchPhase::Capture,
            true,
            false,
        );
        assert!(r.consume);
        assert_eq!(s.state.y.offset, 270.0);
        // Clicks off the bar are left to the content.
        let r = s.mouse_down(
            &down(MouseButton::Left, 200.0, 250.0),
            DispatchPhase::Capture,
            true,
            false,
        );
        assert!(!r.consume);
    }

    #[test]
    fn the_bar_widens_on_hover_and_narrows_after_leaving() {
        let mut s = scroller(0.0, 3000.0);
        assert!(s.mouse_move(&moved(396.0, 100.0, None), true));
        assert!(s.bars[0].is_hovered());
        // 10 px from the edge is inside the widened bar only.
        assert!(s.in_zone(Orientation::Vertical, point(px(390.0), px(100.0))));
        assert!(!s.in_zone(Orientation::Vertical, point(px(380.0), px(100.0))));
        assert!(s.mouse_move(&moved(200.0, 100.0, None), true));
        assert!(!s.bars[0].is_hovered());
        s.mouse_move(&moved(396.0, 100.0, None), true);
        assert!(s.mouse_exit());
        assert!(!s.bars[0].is_hovered());
    }

    #[test]
    fn content_that_fits_has_no_bar_to_grab() {
        let mut s = scroller(0.0, 200.0);
        assert!(!s.in_zone(Orientation::Vertical, point(px(396.0), px(10.0))));
        let r = s.mouse_down(
            &down(MouseButton::Left, 396.0, 10.0),
            DispatchPhase::Capture,
            true,
            false,
        );
        assert!(!r.consume);
    }

    #[test]
    fn two_bars_leave_each_other_the_corner() {
        let s = scroller(900.0, 3000.0);
        assert_eq!(s.track(Orientation::Vertical).len, 292.0);
        assert_eq!(s.track(Orientation::Horizontal).len, 392.0);
    }

    #[test]
    fn a_quick_middle_click_toggles_autoscroll_and_any_press_stops_it() {
        let mut s = scroller(0.0, 3000.0);
        let r = s.mouse_down(
            &down(MouseButton::Middle, 200.0, 100.0),
            DispatchPhase::Bubble,
            true,
            true,
        );
        assert!(r.autoscroll_started && r.consume);
        assert!(s.mouse_up(&up(MouseButton::Middle)));
        assert!(s.is_autoscrolling());
        let r = s.mouse_down(
            &down(MouseButton::Right, 10.0, 10.0),
            DispatchPhase::Capture,
            false,
            false,
        );
        assert!(r.consume);
        assert!(!s.is_autoscrolling());
    }

    #[test]
    fn middle_press_does_not_start_where_it_must_not() {
        let press = |s: &mut Scroller, x, may_start| {
            s.mouse_down(
                &down(MouseButton::Middle, x, 100.0),
                DispatchPhase::Bubble,
                true,
                may_start,
            )
            .autoscroll_started
        };
        assert!(!press(&mut scroller(0.0, 3000.0), 200.0, false));
        assert!(!press(&mut scroller(0.0, 3000.0), 396.0, true));
        assert!(!press(&mut scroller(0.0, 200.0), 200.0, true));
        let mut s = scroller(0.0, 3000.0);
        let r = s.mouse_down(
            &down(MouseButton::Middle, 200.0, 100.0),
            DispatchPhase::Bubble,
            false,
            true,
        );
        assert!(!r.autoscroll_started);
    }

    #[test]
    fn autoscroll_scrolls_only_the_axes_that_overflow() {
        let mut s = scroller(0.0, 3000.0);
        s.mouse_down(
            &down(MouseButton::Middle, 200.0, 100.0),
            DispatchPhase::Bubble,
            true,
            true,
        );
        assert_eq!(
            s.auto.marker().map(|(_, axes)| axes),
            Some(Axes { x: false, y: true })
        );
        let mut both = scroller(900.0, 3000.0);
        both.mouse_down(
            &down(MouseButton::Middle, 200.0, 100.0),
            DispatchPhase::Bubble,
            true,
            true,
        );
        assert_eq!(
            both.auto.marker().map(|(_, axes)| axes),
            Some(Axes { x: true, y: true })
        );
    }
}
