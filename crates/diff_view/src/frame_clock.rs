//! Per-frame CPU time of a gpui-pre window, for the scroll benchmarks.
//!
//! # How frame time is measured
//!
//! `docs/design.md` defines frame time as the CPU time from the start of the
//! window's draw to the end of its last paint. gpui-pre's `Window::draw`
//! runs a few O(1) bookkeeping steps, then `draw_roots`: the root view's
//! `render` (inside the root element's `request_layout`), layout, prepaint,
//! the paint of the root element tree, then the paint of deferred draws in
//! priority order, then the tooltip / prompt / drag overlay if any. So:
//!
//! - **start**: [`FrameClock::begin`], called first thing in the root view's
//!   `render`, which is the first app code of every draw.
//! - **end**: [`FrameClock::end_marker`], a zero-size element the root view
//!   adds as a child. It is a `deferred` draw with the highest priority, so
//!   it paints after the whole tree and after every other deferred draw
//!   (popovers, dock drag previews). Only a tooltip, prompt or drag overlay
//!   could paint after it; benchmarks have none.
//!
//! The scene's post-processing at the end of `draw` and the GPU `present`
//! are excluded, as the definition asks. gpui-pre's own `profiler` feature
//! records the same span (`WindowProfiler::begin_draw`/`end_draw`), but
//! enabling it changes gpui's features for the whole workspace build and
//! adds its overlay and journal costs, so the benchmarks do not use it.
//!
//! **Dropped frames**: the interval between two consecutive frame starts is
//! compared with the display refresh period from `Window::refresh_interval`
//! (60 Hz when the platform does not report one). An interval over 1.5
//! periods counts as one dropped frame. Benchmarks open their window with
//! `inactive_frame_interval`, `low_power_frame_interval` and
//! `thermal_frame_interval` set to `None` so gpui does not throttle it when
//! another window has focus.
//!
//! A benchmark drives frames with `Window::request_animation_frame` from
//! `render`, calls [`FrameClock::set_recording`] after a warm-up, and reads
//! [`FrameClock::samples`] and [`FrameClock::dropped_frames`] at the end.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Position, Style, Window, deferred, px,
};
use perf::Samples;

const FALLBACK_REFRESH: Duration = Duration::from_nanos(16_666_667);

#[derive(Default)]
struct ClockState {
    recording: bool,
    frame_start: Option<Instant>,
    /// When the last frame finished painting, recorded or not.
    last_frame_end: Option<Instant>,
    last_frame_start: Option<Instant>,
    refresh: Option<Duration>,
    samples: Samples,
    dropped: usize,
}

/// Shared handle; clones record into the same samples.
#[derive(Clone, Default)]
pub struct FrameClock(Rc<RefCell<ClockState>>);

impl FrameClock {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks the start of a frame. Call it first in the root view's `render`.
    pub fn begin(&self, window: &Window, cx: &App) {
        let now = Instant::now();
        let mut state = self.0.borrow_mut();
        let refresh = window.refresh_interval(cx).unwrap_or(FALLBACK_REFRESH);
        state.refresh = Some(refresh);
        if state.recording
            && let Some(last) = state.last_frame_start
            && now.duration_since(last) > refresh.mul_f64(1.5)
        {
            state.dropped += 1;
        }
        state.last_frame_start = Some(now);
        state.frame_start = Some(now);
    }

    /// The element that marks the end of the frame; add it as a child of the
    /// root view's element.
    pub fn end_marker(&self) -> impl IntoElement + use<> {
        deferred(FrameEnd(self.clone())).priority(usize::MAX)
    }

    /// Frames are timed only while recording, so warm-up is left out.
    pub fn set_recording(&self, recording: bool) {
        let mut state = self.0.borrow_mut();
        state.recording = recording;
        if !recording {
            state.last_frame_start = None;
        }
    }

    pub fn samples(&self) -> Samples {
        self.0.borrow().samples.clone()
    }

    pub fn dropped_frames(&self) -> usize {
        self.0.borrow().dropped
    }

    /// The display refresh period seen at the last frame.
    pub fn refresh_interval(&self) -> Duration {
        self.0.borrow().refresh.unwrap_or(FALLBACK_REFRESH)
    }

    /// When the last finished frame ended its paint, whether or not it was
    /// recorded. For timing from an event to the frame that shows it.
    pub fn last_frame_end(&self) -> Option<Instant> {
        self.0.borrow().last_frame_end
    }

    fn end(&self) {
        let now = Instant::now();
        let mut state = self.0.borrow_mut();
        state.last_frame_end = Some(now);
        if let Some(start) = state.frame_start.take()
            && state.recording
        {
            state.samples.push(now.duration_since(start));
        }
    }
}

/// Zero-size and absolutely positioned, so it never moves layout.
struct FrameEnd(FrameClock);

impl IntoElement for FrameEnd {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for FrameEnd {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        let mut style = Style {
            position: Position::Absolute,
            ..Style::default()
        };
        style.size.width = px(0.).into();
        style.size.height = px(0.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        _: &mut Window,
        _: &mut App,
    ) {
        self.0.end();
    }
}
