//! Types into a focused comment composer in the real shell, with a pull
//! request open, and prints a `PERF` line for key press to paint, then exits.
//!
//! cargo run -p app --release --example composer_bench -- --unthrottled [--keys 300] [--store PATH]
//!
//! The shell runs on fixtures with the default dock. Pull request #41 is
//! opened, a line composer is opened under the first line of its first file
//! and focused. Each key goes through `Window::dispatch_keystroke`, the
//! path a platform key event takes: key bindings, then the focused text
//! box's input handler. Drafts are saved to a real store, so the cost of
//! the draft path is in the numbers.
//!
//! - `composer_keypress`: the key is dispatched from a deferred callback
//!   (never inside a draw: no input handler is registered while one runs),
//!   the frame is drawn at once, and the sample runs to the end of that
//!   frame, read from the frame clock the shell ends in a deferred element
//!   after everything else has painted. This is what the app spends from
//!   key to painted character. A sample counts only if that frame's paint
//!   saw the new text. p99 against 16 ms.
//! - `with_vsync_*` fields of the same line: the same keys dispatched and
//!   left for the next frame the window draws, so a sample also waits for
//!   the display's next refresh (gpui draws on the vsync tick), as a real
//!   key press does. Reported, not judged: it cannot be under one refresh
//!   interval, 16.7 ms at 60 Hz.
//!
//! `--unthrottled` keeps gpui from slowing frames while another window has
//! focus; `just bench` runs with it.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use app::appearance::ThemeFlags;
use app::panels::{ConversationPanel, DiffPanel, LogPanel};
use app::scenario::{first_line, keystroke_for};
use app::session::Session;
use app::shell::{Probe, Shell};
use app::writing::Writing;
use app::{Setup, assets, new_dock, window_options};
use diff_view::{DiffView, FrameClock};
use gpui::{App, AppContext as _, Context, Entity, Window, px};
use gpui_kit::component::dock::{DockLayout, panel_handle};
use store::DraftAnchor;
use theme::ModeSetting;

const WARMUP_FRAMES: usize = 60;
/// Frames between two keys, so each key is a frame of its own.
const SETTLE_FRAMES: usize = 3;
const WARMUP_KEYS: usize = 12;
const BUDGET_MS: f64 = 16.0;
const WAIT: Duration = Duration::from_secs(60);
const PR: u64 = 41;
const TEXT: &str = "the quick brown fox jumps over the lazy dog 0123456789 Sphinx of black quartz";
/// A new line after this many characters, so the box grows as a comment does.
const LINE: usize = 48;

struct Args {
    keys: usize,
    unthrottled: bool,
    store: Option<PathBuf>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            keys: 300,
            unthrottled: false,
            store: None,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--keys" => {
                    args.keys = it
                        .next()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or_else(|| panic!("--keys takes a number"));
                }
                "--store" => args.store = it.next().map(PathBuf::from),
                "--unthrottled" => args.unthrottled = true,
                other => panic!(
                    "unknown argument {other}; expected --keys N, --store PATH, --unthrottled"
                ),
            }
        }
        args
    }
}

/// How a key is issued.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The frame is drawn as soon as the key has been handled.
    Immediate,
    /// The key waits for the window's next frame, after the refresh wait.
    NextFrame,
}

enum Phase {
    Warmup,
    Opening,
    Composing,
    Running,
    Done,
}

struct Sample {
    mode: Mode,
    started: Rc<Cell<Option<Instant>>>,
    /// Bytes of text the frame that shows this key must have painted.
    expected: usize,
    /// Warm-up keys are typed but not counted.
    timed: bool,
}

struct ComposerBench {
    session: Entity<Session>,
    diff: Entity<DiffPanel>,
    anchor: Option<DraftAnchor>,
    clock: FrameClock,
    phase: Phase,
    frame: usize,
    phase_started: Instant,
    settle: usize,
    typed: usize,
    keys: usize,
    sample: Option<Sample>,
    immediate: perf::Samples,
    next_frame: perf::Samples,
    /// Samples whose frame did not paint the new text.
    stale: usize,
}

impl ComposerBench {
    fn view(&self, cx: &App) -> Option<Entity<DiffView>> {
        self.diff.read(cx).view().cloned()
    }

    fn writing(&self, cx: &App) -> Entity<Writing> {
        self.session
            .read(cx)
            .writing()
            .expect("the pull request is open")
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_started = Instant::now();
    }

    fn open_pr(&mut self, cx: &mut Context<Shell>) {
        let session = self.session.clone();
        let key = session
            .read(cx)
            .inbox
            .prs
            .iter()
            .find(|pr| pr.number() == PR)
            .map(|pr| pr.key.clone())
            .unwrap_or_else(|| panic!("fixture #{PR} exists"));
        cx.defer(move |cx| session.update(cx, |session, cx| session.open(&key, cx)));
        self.enter(Phase::Opening);
    }

    /// Opens the composer; deferred, because a notify during a draw reaches
    /// no observers in gpui-pre.
    fn open_composer(&mut self, cx: &mut Context<Shell>) {
        let (path, line) = first_line(cx).expect("the diff is up");
        let anchor = DraftAnchor::Line {
            path,
            line,
            start: None,
        };
        let writing = self.writing(cx);
        let open = anchor.clone();
        cx.defer(move |cx| writing.update(cx, |writing, cx| writing.open(open, None, cx)));
        self.anchor = Some(anchor);
        self.enter(Phase::Composing);
    }

    fn composer(&self, cx: &App) -> Option<Entity<app::composer::Composer>> {
        let anchor = self.anchor.as_ref()?;
        self.writing(cx).read(cx).slot(anchor)?.composer.clone()
    }

    /// The key to type next: the text cycled, a new line every `LINE`.
    fn next_key(&self) -> char {
        if self.typed % LINE == LINE - 1 {
            return '\n';
        }
        TEXT.chars()
            .cycle()
            .nth(self.typed)
            .expect("the text is not empty")
    }

    fn total(&self) -> usize {
        WARMUP_KEYS + self.keys + self.keys / 3
    }

    fn mode_of(&self, typed: usize) -> Option<Mode> {
        if typed < WARMUP_KEYS + self.keys {
            Some(Mode::Immediate)
        } else if typed < self.total() {
            Some(Mode::NextFrame)
        } else {
            None
        }
    }

    fn running(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        if let Some(sample) = self.sample.take() {
            if !self.resolve(&sample, cx) {
                self.sample = Some(sample);
                return;
            }
            self.settle = SETTLE_FRAMES;
        }
        if self.settle > 0 {
            self.settle -= 1;
            return;
        }
        let Some(mode) = self.mode_of(self.typed) else {
            self.report(window, cx);
            self.phase = Phase::Done;
            return;
        };
        let ch = self.next_key();
        let timed = self.typed >= WARMUP_KEYS;
        self.typed += 1;
        let started = Rc::new(Cell::new(None));
        let len = |bench: &Self, cx: &App| {
            bench
                .composer(cx)
                .map_or(0, |composer| composer.read(cx).text(cx).len())
        };
        let expected = len(self, cx) + ch.len_utf8();
        let (started_at, composer) = (started.clone(), self.composer(cx));
        window.defer(cx, move |window, cx| {
            started_at.set(Some(Instant::now()));
            window.dispatch_keystroke(keystroke_for(ch), cx);
            if mode == Mode::Immediate {
                window.draw(cx).clear(cx);
            }
            let held = composer.map_or(0, |composer| composer.read(cx).text(cx).len());
            assert_eq!(held, expected, "the key {ch:?} did not reach the text box");
        });
        self.sample = Some(Sample {
            mode,
            started,
            expected,
            timed,
        });
    }

    /// Reads a finished sample; false while its frame has not ended.
    fn resolve(&mut self, sample: &Sample, cx: &mut Context<Shell>) -> bool {
        // The key waits for its deferred callback.
        let Some(started) = sample.started.get() else {
            return false;
        };
        // The frame that ended since the key ran is the one that drew it.
        let Some(ended) = self.clock.last_frame_end().filter(|end| *end > started) else {
            return false;
        };
        let painted = self
            .composer(cx)
            .map_or(0, |composer| composer.read(cx).painted_len());
        if !sample.timed {
            return true;
        }
        if painted != sample.expected {
            self.stale += 1;
        }
        let elapsed = ended.saturating_duration_since(started);
        match sample.mode {
            Mode::Immediate => self.immediate.push(elapsed),
            Mode::NextFrame => self.next_frame.push(elapsed),
        }
        true
    }

    fn report(&self, window: &Window, cx: &App) {
        let interval = window
            .refresh_interval(cx)
            .map_or(0.0, |interval| interval.as_secs_f64() * 1000.0);
        let with_vsync = self.next_frame.report("with_vsync", 99.0, f64::INFINITY);
        let mut report = self
            .immediate
            .report("composer_keypress", 99.0, BUDGET_MS)
            .with("keys", self.immediate.len())
            .with("stale_frames", self.stale)
            .with("refresh_interval_ms", interval)
            .with("with_vsync_keys", with_vsync.count)
            .with("with_vsync_p50_ms", with_vsync.p50_ms)
            .with("with_vsync_p99_ms", with_vsync.p99_ms)
            .with("with_vsync_max_ms", with_vsync.max_ms);
        // A frame that did not paint the new text did not show it.
        report.pass &= self.stale == 0;
        report.print();
    }
}

impl Probe for ComposerBench {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::Warmup => {
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.open_pr(cx);
                }
            }
            Phase::Opening => {
                let painted = self
                    .view(cx)
                    .is_some_and(|view| view.read(cx).last_frame().rows > 0);
                assert!(
                    self.phase_started.elapsed() < WAIT,
                    "#{PR} did not paint within {WAIT:?}"
                );
                if painted {
                    self.open_composer(cx);
                }
            }
            Phase::Composing => {
                assert!(
                    self.phase_started.elapsed() < WAIT,
                    "the composer did not show within {WAIT:?}"
                );
                let focused = self
                    .composer(cx)
                    .is_some_and(|composer| composer.read(cx).input_focus(cx).is_focused(window));
                let blocks = self
                    .view(cx)
                    .map_or(0, |view| view.read(cx).last_frame().blocks);
                if focused && blocks > 0 {
                    self.settle = SETTLE_FRAMES * 4;
                    self.phase = Phase::Running;
                }
            }
            Phase::Running => self.running(window, cx),
            Phase::Done => {
                cx.quit();
                return;
            }
        }
        window.request_animation_frame();
    }
}

fn main() {
    let args = Args::parse();
    let store_path = args.store.clone().unwrap_or_else(|| {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/composer-bench");
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("cache.db")
    });
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    fixtures: true,
                    store_path: Some(store_path),
                    theme: ThemeFlags {
                        family: Some("Vercel".to_owned()),
                        mode: Some(ModeSetting::Dark),
                    },
                    ..Setup::default()
                },
            );
            let options = window_options(cx, !args.unthrottled);
            gpui_kit::open_window(options, cx, move |window, cx| {
                let dock = new_dock(window, cx);
                let diff = DiffPanel::new(window, cx);
                let conversation = ConversationPanel::new(window, cx);
                let log = LogPanel::new(window, cx);
                let layout = DockLayout::h_split()
                    .child(
                        DockLayout::tabs()
                            .panel_view(panel_handle(diff.clone()), cx)
                            .panel_view(panel_handle(conversation), cx),
                        Some(px(820.0)),
                    )
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(log), cx),
                        Some(px(340.0)),
                    );
                dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
                let bench = ComposerBench {
                    session: Session::global(cx),
                    diff,
                    anchor: None,
                    clock: FrameClock::new(),
                    phase: Phase::Warmup,
                    frame: 0,
                    phase_started: Instant::now(),
                    settle: 0,
                    typed: 0,
                    keys: args.keys,
                    sample: None,
                    immediate: perf::Samples::default(),
                    next_frame: perf::Samples::default(),
                    stale: 0,
                };
                cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
