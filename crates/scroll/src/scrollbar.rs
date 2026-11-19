//! Scrollbar behavior along one axis: thumb mapping, dragging, paging on
//! track clicks and the hover animation. No gpui types; the caller does hit
//! testing against its own geometry and passes positions along the track.

use std::time::Duration;

use crate::axis::ScrollAxis;
use crate::time::Time;

/// Delay before a held track click repeats.
pub const REPEAT_DELAY: Duration = Duration::from_millis(300);
/// Interval between repeats after the first.
pub const REPEAT_INTERVAL: Duration = Duration::from_millis(50);
/// How long the bar takes to widen or narrow.
pub const HOVER_EASE: Duration = Duration::from_millis(100);
/// A page is this fraction of the viewport, so some context stays in view.
pub const PAGE_FRACTION: f64 = 0.9;
/// Repeats applied by one tick at most, so a stalled frame does not jump.
const MAX_REPEATS_PER_TICK: usize = 3;

/// The track along the scroll direction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Track {
    pub len: f64,
    /// The shortest the thumb may get, so it stays grabbable.
    pub min_thumb: f64,
}

/// The thumb's extent along the track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Thumb {
    pub start: f64,
    pub len: f64,
}

impl Thumb {
    #[must_use]
    pub fn end(&self) -> f64 {
        self.start + self.len
    }

    #[must_use]
    pub fn contains(&self, pos: f64) -> bool {
        pos >= self.start && pos < self.end()
    }
}

/// Where the thumb sits for `axis`; `None` when the content fits or the track
/// is empty. The thumb is proportional to the viewport unless that is shorter
/// than `min_thumb`; it then travels the remaining `len - thumb` of the track,
/// so the ends of the track still map to the ends of the content.
#[must_use]
pub fn thumb(axis: &ScrollAxis, track: Track) -> Option<Thumb> {
    if !axis.overflows() || track.len <= 0.0 {
        return None;
    }
    let len = (track.len * axis.viewport / axis.content)
        .max(track.min_thumb)
        .min(track.len);
    let travel = track.len - len;
    Some(Thumb {
        start: travel * axis.offset / axis.max(),
        len,
    })
}

/// The offset that puts the thumb's start at `start`.
#[must_use]
pub fn offset_for_thumb_start(axis: &ScrollAxis, track: Track, start: f64) -> f64 {
    let Some(thumb) = thumb(axis, track) else {
        return 0.0;
    };
    let travel = track.len - thumb.len;
    if travel <= 0.0 {
        return 0.0;
    }
    (start / travel).clamp(0.0, 1.0) * axis.max()
}

/// What a press landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Outside the track, or nothing to scroll.
    Miss,
    /// A drag started.
    Thumb,
    /// The first page was applied; it repeats while held.
    Track,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    Backward,
    Forward,
}

#[derive(Clone, Copy, Debug)]
struct Hold {
    page: Page,
    /// The pointer along the track; paging stops when the thumb reaches it.
    pos: f64,
    next: Time,
}

/// The interaction state of one scrollbar. The view's offset lives in a
/// [`ScrollAxis`] the caller passes to each method.
#[derive(Clone, Debug, Default)]
pub struct ScrollbarState {
    /// Pointer minus thumb start at the press, so the thumb does not jump.
    grab: Option<f64>,
    hold: Option<Hold>,
    hovered: bool,
    /// Eased progress toward the hover width, 0 to 1.
    progress: f64,
    updated: Time,
}

impl ScrollbarState {
    /// Handles a primary-button press at `pos` along the track.
    pub fn press(&mut self, pos: f64, track: Track, axis: &mut ScrollAxis, now: Time) -> Press {
        let Some(thumb) = thumb(axis, track) else {
            return Press::Miss;
        };
        if !(0.0..=track.len).contains(&pos) {
            return Press::Miss;
        }
        self.advance(now);
        if thumb.contains(pos) {
            self.grab = Some(pos - thumb.start);
            self.hold = None;
            return Press::Thumb;
        }
        let page = if pos < thumb.start {
            Page::Backward
        } else {
            Page::Forward
        };
        self.hold = Some(Hold {
            page,
            pos,
            next: now + REPEAT_DELAY,
        });
        self.page_once(track, axis);
        Press::Track
    }

    /// Moves the dragged thumb to follow the pointer at `pos`, or retargets a
    /// held track click. Returns whether the offset changed.
    pub fn pointer_moved(&mut self, pos: f64, track: Track, axis: &mut ScrollAxis) -> bool {
        if let Some(grab) = self.grab {
            return axis.set(offset_for_thumb_start(axis, track, pos - grab));
        }
        if let Some(hold) = &mut self.hold {
            hold.pos = pos;
        }
        false
    }

    /// Ends a drag or a held track click.
    pub fn release(&mut self, now: Time) {
        self.advance(now);
        self.grab = None;
        self.hold = None;
    }

    /// Applies track-click repeats that are due and moves the hover animation
    /// to `now`. Returns whether the offset changed.
    pub fn tick(&mut self, now: Time, track: Track, axis: &mut ScrollAxis) -> bool {
        self.advance(now);
        let mut changed = false;
        for _ in 0..MAX_REPEATS_PER_TICK {
            let Some(hold) = &mut self.hold else { break };
            if hold.next > now {
                return changed;
            }
            hold.next += REPEAT_INTERVAL;
            changed |= self.page_once(track, axis);
        }
        if let Some(hold) = &mut self.hold {
            hold.next = hold.next.max(now);
        }
        changed
    }

    fn page_once(&mut self, track: Track, axis: &mut ScrollAxis) -> bool {
        let Some(hold) = self.hold else { return false };
        let Some(thumb) = thumb(axis, track) else {
            self.hold = None;
            return false;
        };
        let reached = match hold.page {
            Page::Backward => thumb.start <= hold.pos,
            Page::Forward => thumb.end() >= hold.pos,
        };
        if reached {
            self.hold = None;
            return false;
        }
        let page = axis.viewport * PAGE_FRACTION;
        axis.by(match hold.page {
            Page::Backward => -page,
            Page::Forward => page,
        })
    }

    /// Tells the bar whether the pointer is over it.
    pub fn set_hovered(&mut self, hovered: bool, now: Time) {
        self.advance(now);
        self.hovered = hovered;
    }

    #[must_use]
    pub fn is_hovered(&self) -> bool {
        self.hovered
    }

    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.grab.is_some()
    }

    /// True while a track click is held down.
    #[must_use]
    pub fn is_paging(&self) -> bool {
        self.hold.is_some()
    }

    fn target(&self) -> f64 {
        if self.hovered || self.grab.is_some() {
            1.0
        } else {
            0.0
        }
    }

    fn advance(&mut self, now: Time) {
        let step = now.saturating_sub(self.updated).as_secs_f64() / HOVER_EASE.as_secs_f64();
        self.updated = now;
        let target = self.target();
        self.progress = if self.progress < target {
            (self.progress + step).min(target)
        } else {
            (self.progress - step).max(target)
        };
    }

    /// How far the bar is toward its hover width, eased, 0 to 1.
    #[must_use]
    pub fn hover_amount(&self) -> f64 {
        let t = self.progress;
        t * t * (3.0 - 2.0 * t)
    }

    /// True when the next frame will look different with no input: a held
    /// track click or a width still easing.
    #[must_use]
    pub fn wants_frame(&self) -> bool {
        self.hold.is_some() || self.progress != self.target()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRACK: Track = Track {
        len: 400.0,
        min_thumb: 24.0,
    };

    fn ms(n: u64) -> Time {
        Duration::from_millis(n)
    }

    fn axis(content: f64) -> ScrollAxis {
        ScrollAxis {
            offset: 0.0,
            content,
            viewport: 400.0,
        }
    }

    #[test]
    fn thumb_is_proportional_and_hidden_when_content_fits() {
        assert_eq!(thumb(&axis(400.0), TRACK), None);
        let mut a = axis(800.0);
        a.offset = 400.0;
        let t = thumb(&a, TRACK).unwrap();
        assert_eq!((t.start, t.len), (200.0, 200.0));
    }

    #[test]
    fn short_thumb_still_maps_both_ends_of_the_content() {
        let mut a = axis(4_000_000.0);
        let t = thumb(&a, TRACK).unwrap();
        assert_eq!(t.len, 24.0);
        a.offset = a.max();
        let t = thumb(&a, TRACK).unwrap();
        assert!((t.end() - 400.0).abs() < 1e-9);
        assert_eq!(offset_for_thumb_start(&a, TRACK, 376.0), a.max());
        assert_eq!(offset_for_thumb_start(&a, TRACK, 188.0), a.max() / 2.0);
        assert_eq!(offset_for_thumb_start(&a, TRACK, -50.0), 0.0);
        assert_eq!(offset_for_thumb_start(&a, TRACK, 999.0), a.max());
    }

    #[test]
    fn drag_keeps_the_grab_offset() {
        let mut a = axis(800.0);
        a.offset = 100.0;
        let start = thumb(&a, TRACK).unwrap().start;
        let mut bar = ScrollbarState::default();
        // Grab 30 px into the thumb: a zero-distance move must not scroll.
        assert_eq!(bar.press(start + 30.0, TRACK, &mut a, ms(0)), Press::Thumb);
        assert!(!bar.pointer_moved(start + 30.0, TRACK, &mut a));
        assert_eq!(a.offset, 100.0);
        assert!(bar.pointer_moved(start + 80.0, TRACK, &mut a));
        assert_eq!(thumb(&a, TRACK).unwrap().start, start + 50.0);
        bar.release(ms(10));
        assert!(!bar.is_dragging());
        assert!(!bar.pointer_moved(0.0, TRACK, &mut a));
    }

    #[test]
    fn track_click_pages_then_repeats_while_held() {
        let mut a = axis(40_000.0);
        let mut bar = ScrollbarState::default();
        assert_eq!(bar.press(390.0, TRACK, &mut a, ms(0)), Press::Track);
        assert_eq!(a.offset, 360.0);
        assert!(!bar.tick(ms(299), TRACK, &mut a));
        assert!(bar.tick(ms(300), TRACK, &mut a));
        assert_eq!(a.offset, 720.0);
        assert!(!bar.tick(ms(349), TRACK, &mut a));
        assert!(bar.tick(ms(350), TRACK, &mut a));
        assert_eq!(a.offset, 1080.0);
        bar.release(ms(360));
        assert!(!bar.tick(ms(1000), TRACK, &mut a));
        assert_eq!(a.offset, 1080.0);
    }

    #[test]
    fn a_stalled_frame_does_not_page_more_than_a_few_times() {
        let mut a = axis(400_000.0);
        let mut bar = ScrollbarState::default();
        bar.press(390.0, TRACK, &mut a, ms(0));
        let before = a.offset;
        bar.tick(ms(5000), TRACK, &mut a);
        assert_eq!(a.offset, before + 3.0 * 360.0);
    }

    #[test]
    fn paging_stops_when_the_thumb_reaches_the_pointer() {
        let mut a = axis(1000.0);
        let mut bar = ScrollbarState::default();
        // The thumb is 160 long; a click at 300 needs one page to get there.
        bar.press(300.0, TRACK, &mut a, ms(0));
        bar.tick(ms(300), TRACK, &mut a);
        assert!(thumb(&a, TRACK).unwrap().end() >= 300.0);
        let at = a.offset;
        bar.tick(ms(600), TRACK, &mut a);
        assert_eq!(a.offset, at);
        assert!(!bar.is_paging());
    }

    #[test]
    fn clicking_above_the_thumb_pages_backward() {
        let mut a = axis(4000.0);
        a.offset = 2000.0;
        let mut bar = ScrollbarState::default();
        assert_eq!(bar.press(2.0, TRACK, &mut a, ms(0)), Press::Track);
        assert_eq!(a.offset, 1640.0);
    }

    #[test]
    fn press_outside_the_track_or_with_nothing_to_scroll_misses() {
        let mut bar = ScrollbarState::default();
        assert_eq!(bar.press(10.0, TRACK, &mut axis(400.0), ms(0)), Press::Miss);
        assert_eq!(
            bar.press(500.0, TRACK, &mut axis(800.0), ms(0)),
            Press::Miss
        );
    }

    #[test]
    fn hover_eases_in_over_the_ease_time_and_back_out() {
        let mut bar = ScrollbarState::default();
        bar.set_hovered(true, ms(1000));
        assert_eq!(bar.hover_amount(), 0.0);
        assert!(bar.wants_frame());
        let mut a = axis(800.0);
        bar.tick(ms(1050), TRACK, &mut a);
        assert!((bar.hover_amount() - 0.5).abs() < 1e-9);
        bar.tick(ms(1100), TRACK, &mut a);
        assert_eq!(bar.hover_amount(), 1.0);
        assert!(!bar.wants_frame());
        bar.set_hovered(false, ms(2000));
        bar.tick(ms(2100), TRACK, &mut a);
        assert_eq!(bar.hover_amount(), 0.0);
    }

    #[test]
    fn dragging_keeps_the_bar_wide_after_the_pointer_leaves() {
        let mut a = axis(800.0);
        let mut bar = ScrollbarState::default();
        bar.set_hovered(true, ms(0));
        bar.press(10.0, TRACK, &mut a, ms(0));
        bar.set_hovered(false, ms(10));
        bar.tick(ms(500), TRACK, &mut a);
        assert_eq!(bar.hover_amount(), 1.0);
        bar.release(ms(500));
        bar.tick(ms(700), TRACK, &mut a);
        assert_eq!(bar.hover_amount(), 0.0);
    }
}
