//! The real shell with 300 pull requests in the inbox's "Created by me"
//! section, each with its review marks; prints a `PERF` line and exits.
//!
//! cargo run -p app --release --example inbox_bench -- --unthrottled [--rows 300] [--frames 600]
//!
//! The wheel scrolls that one section in-process, as its own handler does, a
//! step per frame: down through all its rows and back up. Frame time is
//! measured by `diff_view::FrameClock`, started in the shell's render and
//! ended in a deferred element, so it covers the sidebar and the list.
//! The metric is `inbox_scroll`, p99 against 8 ms. Renders of the inbox body,
//! rows built and section panels rendered per frame show what a scroll
//! step re-renders.

use std::sync::atomic::Ordering;

use app::fixtures::Fixtures;
use app::inbox::{BODY_RENDERS, ROWS_BUILT};
use app::session::Session;
use app::shell::{Probe, Shell};
use app::sidebar::Sidebar;
use app::sidebar_panels::PANEL_RENDERS;
use app::{Setup, assets, new_dock, window_options};
use diff_view::FrameClock;
use gpui::{
    App, AppContext as _, Context, MouseMoveEvent, PlatformInput, ScrollDelta, ScrollEvent, Window,
    point, px,
};

const WARMUP_FRAMES: usize = 60;
const BUDGET_MS: f64 = 8.0;
/// Pixels one frame's wheel step scrolls: about a row.
const STEP: f64 = 40.0;
const SECTION: usize = 1;
/// A point inside the section's list, in window coordinates.
const LIST_Y: f32 = 250.0;

struct Args {
    rows: usize,
    frames: usize,
    unthrottled: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            rows: 300,
            frames: 600,
            unthrottled: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.replace('_', "").parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--rows" => args.rows = number("--rows"),
                "--frames" => args.frames = number("--frames"),
                "--unthrottled" => args.unthrottled = true,
                other => {
                    panic!("unknown argument {other}; expected --rows N, --frames N, --unthrottled")
                }
            }
        }
        args
    }
}

/// The fixture inbox with `rows` pull requests in section [`SECTION`].
fn big_inbox(rows: usize) -> app::model::Inbox {
    let mut inbox = Fixtures::load().inbox;
    let base = inbox.prs.clone();
    let mut items = Vec::with_capacity(rows);
    for n in 0..rows {
        let mut pr = base[n % base.len()].clone();
        pr.key.number.0 = 50_000 + n as u64;
        pr.title = format!("{} ({n})", pr.title).into();
        items.push(inbox.prs.len());
        inbox.prs.push(pr);
    }
    inbox.sections[SECTION].items = items;
    inbox
}

struct InboxBench {
    clock: FrameClock,
    frames: usize,
    frame: usize,
    started: bool,
    direction: f64,
    counts: Counts,
    max_scroll: f64,
}

#[derive(Default, Clone, Copy)]
struct Counts {
    body: usize,
    rows: usize,
    panels: usize,
}

impl Counts {
    fn now() -> Self {
        Self {
            body: BODY_RENDERS.load(Ordering::Relaxed),
            rows: ROWS_BUILT.load(Ordering::Relaxed),
            panels: PANEL_RENDERS.load(Ordering::Relaxed),
        }
    }
}

impl Probe for InboxBench {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        let sidebar = Sidebar::global(cx);
        if self.frame == WARMUP_FRAMES && !self.started {
            self.started = true;
            self.frame = 0;
            self.clock.set_recording(true);
            self.counts = Counts::now();
        }
        if self.started && self.frame >= self.frames {
            self.clock.set_recording(false);
            let now = Counts::now();
            let frames = self.frames.max(1) as f64;
            self.clock
                .samples()
                .report("inbox_scroll", 99.0, BUDGET_MS)
                .with("dropped_frames", self.clock.dropped_frames())
                .with("max_scroll_px", self.max_scroll)
                .with(
                    "body_renders_per_frame",
                    (now.body - self.counts.body) as f64 / frames,
                )
                .with(
                    "rows_built_per_frame",
                    (now.rows - self.counts.rows) as f64 / frames,
                )
                .with(
                    "panels_per_frame",
                    (now.panels - self.counts.panels) as f64 / frames,
                )
                .print();
            cx.quit();
            return;
        }
        if self.frame >= WARMUP_FRAMES / 2 || self.started {
            // The wheel goes through the window's own event dispatch, over the
            // list, after this frame has been drawn.
            let (offset, max) =
                sidebar.update(cx, |sidebar, cx| sidebar.scroll_section(SECTION, 0.0, cx));
            self.max_scroll = max;
            if offset + STEP * self.direction > max {
                self.direction = -1.0;
            } else if offset + STEP * self.direction < 0.0 {
                self.direction = 1.0;
            }
            let position = point(px(120.0), px(LIST_Y));
            let delta = ScrollDelta::Pixels(point(px(0.0), px(-(STEP * self.direction) as f32)));
            cx.defer_in(window, move |_, window, cx| {
                window.dispatch_event(
                    PlatformInput::MouseMove(MouseMoveEvent {
                        position,
                        ..MouseMoveEvent::default()
                    }),
                    cx,
                );
                window.dispatch_event(
                    PlatformInput::Scroll(ScrollEvent {
                        position,
                        delta,
                        ..ScrollEvent::default()
                    }),
                    cx,
                );
            });
        }
        self.frame += 1;
        window.request_animation_frame();
    }
}

fn main() {
    let args = Args::parse();
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    fixtures: true,
                    ..Setup::default()
                },
            );
            let inbox = big_inbox(args.rows);
            Session::global(cx).update(cx, |session, cx| session.set_inbox(inbox, cx));
            // Progress bars, unread dots and reviewed marks on the rows.
            app::inbox::marks::demo(cx);
            Sidebar::global(cx).update(cx, |sidebar, cx| {
                // Only the one section, so it has the room.
                for ix in 0..5 {
                    if sidebar.section_collapsed(ix) == Some(ix == SECTION) {
                        sidebar.toggle_section(ix, cx);
                    }
                }
            });
            gpui_kit::open_window(
                window_options(cx, !args.unthrottled),
                cx,
                move |window, cx| {
                    let dock = new_dock(window, cx);
                    let bench = InboxBench {
                        clock: FrameClock::new(),
                        frames: args.frames,
                        frame: 0,
                        started: false,
                        direction: 1.0,
                        counts: Counts::default(),
                        max_scroll: 0.0,
                    };
                    cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
                },
            )
            .expect("open the main window");
            cx.activate(true);
        });
}
