//! Browser-style middle-click autoscroll as a state machine. No gpui types:
//! positions are plain pixels in any shared space, time is [`Time`].

use std::time::Duration;

use crate::time::Time;

/// Tuning of the gesture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoscrollConfig {
    /// Distance from the anchor below which nothing scrolls.
    pub dead_zone: f64,
    /// A release within this long of the press, with little movement, is a
    /// click that toggles the mode on.
    pub click_window: Duration,
    /// The most the pointer may stray from the anchor and still be a click.
    pub click_slop: f64,
    /// Pixels per second per pixel past the dead zone.
    pub linear: f64,
    /// Pixels per second per squared pixel past the dead zone.
    pub quadratic: f64,
    /// Upper bound of the scroll speed, in pixels per second.
    pub max_speed: f64,
    /// Longest time one step may cover, so a stalled frame does not jump.
    pub max_step: Duration,
}

impl Default for AutoscrollConfig {
    fn default() -> Self {
        Self {
            dead_zone: 12.0,
            click_window: Duration::from_millis(200),
            click_slop: 4.0,
            linear: 6.0,
            quadratic: 0.1,
            max_speed: 10_000.0,
            max_step: Duration::from_millis(100),
        }
    }
}

impl AutoscrollConfig {
    /// Speed in pixels per second for a pointer `distance` from the anchor.
    #[must_use]
    pub fn speed(&self, distance: f64) -> f64 {
        let past = distance.abs() - self.dead_zone;
        if past <= 0.0 {
            return 0.0;
        }
        (self.linear * past + self.quadratic * past * past).min(self.max_speed)
    }
}

/// A position in pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pos {
    pub x: f64,
    pub y: f64,
}

/// Which axes the gesture may scroll.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Axes {
    pub x: bool,
    pub y: bool,
}

impl Axes {
    #[must_use]
    pub fn any(self) -> bool {
        self.x || self.y
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// The button is down; it scrolls until released.
    Held,
    /// A quick click toggled it on; it scrolls until stopped.
    Toggled,
}

#[derive(Clone, Copy, Debug)]
struct Active {
    mode: Mode,
    anchor: Pos,
    pointer: Pos,
    axes: Axes,
    pressed: Time,
    last_step: Time,
    /// The farthest the pointer has been from the anchor since the press.
    strayed: f64,
}

/// The gesture's state. Idle until [`Autoscroll::start`].
#[derive(Clone, Debug, Default)]
pub struct Autoscroll {
    config: AutoscrollConfig,
    active: Option<Active>,
}

impl Autoscroll {
    #[must_use]
    pub fn new(config: AutoscrollConfig) -> Self {
        Self {
            config,
            active: None,
        }
    }

    /// The middle button went down at `anchor`. Does nothing, and returns
    /// false, when no axis can scroll.
    pub fn start(&mut self, anchor: Pos, axes: Axes, now: Time) -> bool {
        if !axes.any() {
            return false;
        }
        self.active = Some(Active {
            mode: Mode::Held,
            anchor,
            pointer: anchor,
            axes,
            pressed: now,
            last_step: now,
            strayed: 0.0,
        });
        true
    }

    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }

    /// The anchor and the axes it scrolls, for drawing the marker.
    #[must_use]
    pub fn marker(&self) -> Option<(Pos, Axes)> {
        self.active.map(|a| (a.anchor, a.axes))
    }

    pub fn pointer_moved(&mut self, pos: Pos) {
        let Some(active) = &mut self.active else {
            return;
        };
        active.pointer = pos;
        let (dx, dy) = (pos.x - active.anchor.x, pos.y - active.anchor.y);
        active.strayed = active.strayed.max(dx.hypot(dy));
    }

    /// The middle button went up. A quick click on the anchor keeps the
    /// gesture on in toggle mode; anything else ends it. Returns whether it
    /// is still active.
    pub fn release(&mut self, now: Time) -> bool {
        let Some(active) = &mut self.active else {
            return false;
        };
        if active.mode == Mode::Held {
            let quick = now.saturating_sub(active.pressed) < self.config.click_window
                && active.strayed < self.config.click_slop;
            if quick {
                active.mode = Mode::Toggled;
                // The clock for velocity starts fresh in toggle mode.
                active.last_step = now;
            } else {
                self.active = None;
            }
        }
        self.active.is_some()
    }

    /// Ends the gesture: another button, Escape or lost focus.
    pub fn stop(&mut self) {
        self.active = None;
    }

    /// The scroll to apply for the time since the last step: pixels to move
    /// along x and y, positive toward the end. Depends on elapsed time only,
    /// so the speed does not follow the frame rate.
    pub fn step(&mut self, now: Time) -> Pos {
        let Some(active) = &mut self.active else {
            return Pos::default();
        };
        let dt = now
            .saturating_sub(active.last_step)
            .min(self.config.max_step)
            .as_secs_f64();
        active.last_step = now;
        let axis = |enabled: bool, delta: f64| {
            if enabled {
                self.config.speed(delta).copysign(delta) * dt
            } else {
                0.0
            }
        };
        Pos {
            x: axis(active.axes.x, active.pointer.x - active.anchor.x),
            y: axis(active.axes.y, active.pointer.y - active.anchor.y),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Time {
        Duration::from_millis(n)
    }

    const ANCHOR: Pos = Pos { x: 100.0, y: 100.0 };
    const Y: Axes = Axes { x: false, y: true };

    fn started(now: Time) -> Autoscroll {
        let mut auto = Autoscroll::default();
        assert!(auto.start(ANCHOR, Y, now));
        auto
    }

    #[test]
    fn speed_has_a_dead_zone_grows_and_is_capped() {
        let c = AutoscrollConfig::default();
        assert_eq!(c.speed(0.0), 0.0);
        assert_eq!(c.speed(12.0), 0.0);
        assert_eq!(c.speed(-12.0), 0.0);
        let slow = c.speed(20.0);
        let fast = c.speed(100.0);
        let faster = c.speed(200.0);
        assert!(0.0 < slow && slow < fast && fast < faster);
        // Quadratic: the second hundred pixels add more than the first.
        assert!(faster - fast > fast - slow);
        assert_eq!(c.speed(1.0e6), c.max_speed);
    }

    #[test]
    fn quick_click_toggles_and_the_next_press_stops_it() {
        let mut auto = started(ms(0));
        assert!(auto.release(ms(150)));
        assert!(auto.is_active());
        auto.pointer_moved(Pos { x: 100.0, y: 300.0 });
        assert!(auto.step(ms(200)).y > 0.0);
        auto.stop();
        assert!(!auto.is_active());
        assert_eq!(auto.step(ms(300)), Pos::default());
    }

    #[test]
    fn slow_release_ends_the_gesture() {
        let mut auto = started(ms(0));
        assert!(!auto.release(ms(250)));
        assert!(!auto.is_active());
    }

    #[test]
    fn a_drag_release_ends_it_even_when_quick() {
        let mut auto = started(ms(0));
        auto.pointer_moved(Pos { x: 100.0, y: 160.0 });
        assert!(!auto.release(ms(100)));
    }

    #[test]
    fn holding_scrolls_while_down_and_stops_on_release() {
        let mut auto = started(ms(0));
        auto.pointer_moved(Pos { x: 100.0, y: 60.0 });
        let step = auto.step(ms(100));
        assert!(step.y < 0.0);
        assert_eq!(step.x, 0.0);
        assert!(!auto.release(ms(1000)));
    }

    #[test]
    fn distance_is_independent_of_the_frame_rate() {
        let travel = |frame: u64| {
            let mut auto = started(ms(0));
            auto.pointer_moved(Pos { x: 100.0, y: 220.0 });
            let mut total = 0.0;
            let mut t = 0;
            while t < 1000 {
                t += frame;
                total += auto.step(ms(t)).y;
            }
            total
        };
        let (a, b, c) = (travel(8), travel(10), travel(20));
        assert!((a - b).abs() < 1e-6 && (b - c).abs() < 1e-6);
        assert!(a > 0.0);
    }

    #[test]
    fn a_stalled_frame_is_capped() {
        let mut auto = started(ms(0));
        auto.pointer_moved(Pos { x: 100.0, y: 220.0 });
        let capped = auto.step(ms(60_000)).y;
        let mut again = started(ms(0));
        again.pointer_moved(Pos { x: 100.0, y: 220.0 });
        let normal = again.step(ms(100)).y;
        assert_eq!(capped, normal);
    }

    #[test]
    fn only_enabled_axes_move() {
        let mut auto = started(ms(0));
        auto.pointer_moved(Pos { x: 300.0, y: 100.0 });
        assert_eq!(auto.step(ms(50)), Pos::default());
        let mut both = Autoscroll::default();
        both.start(ANCHOR, Axes { x: true, y: true }, ms(0));
        both.pointer_moved(Pos { x: 300.0, y: 100.0 });
        assert!(both.step(ms(50)).x > 0.0);
    }

    #[test]
    fn nothing_starts_without_a_scrollable_axis() {
        let mut auto = Autoscroll::default();
        assert!(!auto.start(ANCHOR, Axes::default(), ms(0)));
        assert!(!auto.is_active());
    }
}
