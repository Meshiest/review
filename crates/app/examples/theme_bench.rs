//! Switches themes, modes and buffer fonts in the real shell with a pull
//! request open (its diff and a review thread on screen); prints `PERF` lines
//! and exits.
//!
//! cargo run -p app --release --example theme_bench -- --unthrottled [--switches 40] [--font-rounds 4]
//!
//! The shell runs on fixtures with the default dock (Diff, Conversation and
//! Checks tabbed, Log beside them). Pull request #41 is opened and scrolled to
//! its first thread, then each switch is issued through `appearance::update`,
//! the call the settings panel makes, from a deferred callback so it never
//! runs inside a draw (a notify from inside a draw reaches no observers in
//! gpui-pre). A sample runs from that call to the end of the first frame that
//! drew with the new state, read from the frame clock the shell ends in a
//! deferred element after everything else has painted. Every sample also
//! checks that the diff re-shaped its lines in that frame, so a frame that
//! painted stale text does not count as fast.
//!
//! - `theme_switch`: Vercel dark and Vercel light, around
//!   again, `--switches` times. p99 against 100 ms.
//! - `buffer_font_switch`: the buffer font cycles over Geist Mono, Cascadia
//!   Mono and Consolas at sizes 12, 13 and 14, `--font-rounds` rounds of the
//!   nine. One untimed round comes first: the first use of a family loads
//!   it, and that cost is reported as `cold_max_ms` instead. p99 against
//!   150 ms.
//!
//! `--unthrottled` keeps gpui from slowing frames while another window has
//! focus; `just bench` runs with it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use app::appearance::{self, ThemeFlags};
use app::panels::{ChecksPanel, ConversationPanel, DiffPanel, LogPanel};
use app::session::Session;
use app::shell::{Probe, Shell};
use app::{Setup, assets, new_dock, window_options};
use diff_view::{DiffView, FrameClock};
use gpui::{App, AppContext as _, Context, Entity, Window, px};
use gpui_kit::component::dock::{DockLayout, panel_handle};
use theme::ModeSetting;

const WARMUP_FRAMES: usize = 60;
/// Frames between two switches, as a user's clicks are apart.
const SETTLE_FRAMES: usize = 6;
const THEME_BUDGET_MS: f64 = 100.0;
const FONT_BUDGET_MS: f64 = 150.0;
const WAIT: Duration = Duration::from_secs(60);
const PR: u64 = 41;

const THEMES: [(&str, ModeSetting, &str); 2] = [
    ("Vercel", ModeSetting::Dark, "vercel-dark"),
    ("Vercel", ModeSetting::Light, "vercel-light"),
];
const FONTS: [&str; 3] = ["Geist Mono", "Cascadia Mono", "Consolas"];
const SIZES: [f32; 3] = [12.0, 13.0, 14.0];

struct Args {
    switches: usize,
    font_rounds: usize,
    unthrottled: bool,
}

impl Args {
    fn parse() -> Self {
        let mut args = Self {
            switches: 40,
            font_rounds: 4,
            unthrottled: false,
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut number = |name: &str| -> usize {
                it.next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| panic!("{name} takes a number"))
            };
            match arg.as_str() {
                "--switches" => args.switches = number("--switches").max(1),
                "--font-rounds" => args.font_rounds = number("--font-rounds").max(1),
                "--unthrottled" => args.unthrottled = true,
                other => panic!(
                    "unknown argument {other}; expected --switches N, --font-rounds N, --unthrottled"
                ),
            }
        }
        args
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Theme,
    Font,
}

#[derive(Clone, Copy)]
enum Change {
    Theme {
        family: &'static str,
        mode: ModeSetting,
        id: &'static str,
    },
    BufferFont {
        family: &'static str,
        size: f32,
    },
}

#[derive(Clone, Copy)]
struct Step {
    change: Change,
    /// `None` for a step that only sets the scene.
    kind: Option<Kind>,
    /// The first use of a font in this process: timed apart.
    cold: bool,
}

fn script(args: &Args) -> Vec<Step> {
    let theme = |ix: usize, kind| {
        let (family, mode, id) = THEMES[ix % THEMES.len()];
        Step {
            change: Change::Theme { family, mode, id },
            kind,
            cold: false,
        }
    };
    let font = |family, size, cold| Step {
        change: Change::BufferFont { family, size },
        kind: Some(Kind::Font),
        cold,
    };
    let round = |cold| {
        FONTS
            .into_iter()
            .flat_map(move |family| SIZES.map(move |size| font(family, size, cold)))
    };
    // The run starts on Vercel dark: the first step moves on from it.
    let mut steps: Vec<Step> = (1..=THEMES.len()).map(|ix| theme(ix, None)).collect();
    steps.extend((1..=args.switches).map(|ix| theme(ix, Some(Kind::Theme))));
    steps.push(theme(0, None));
    steps.extend(round(true));
    for _ in 0..args.font_rounds {
        steps.extend(round(false));
    }
    steps
}

fn apply(change: Change, cx: &mut App) {
    match change {
        Change::Theme { family, mode, .. } => appearance::update(cx, |settings| {
            settings.theme = family.to_owned();
            settings.mode = mode;
        }),
        Change::BufferFont { family, size } => appearance::update(cx, |settings| {
            settings.buffer_font.family = Some(family.to_owned());
            settings.buffer_font.size = Some(size);
        }),
    }
}

/// Panics when the active theme is not the one `change` asked for: a bench
/// that measures the wrong switch is worse than none.
fn check_applied(change: Change, cx: &App) {
    let theme = theme::current(cx);
    match change {
        Change::Theme { id, .. } => assert_eq!(theme.id, id, "the theme did not switch"),
        Change::BufferFont { family, size } => {
            assert_eq!(theme.fonts.buffer.family, family, "the font did not switch");
            assert_eq!(theme.fonts.buffer.size, size);
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    /// Deferred, not run yet.
    Issued,
    /// The frame that drew the new state has begun; its end is read next frame.
    Drawn,
}

struct Inflight {
    step: Step,
    started: Rc<Cell<Option<Instant>>>,
    stage: Stage,
}

enum Phase {
    Warmup,
    Opening,
    Jumping,
    Running,
    Done,
}

struct ThemeBench {
    session: Entity<Session>,
    diff: Entity<DiffPanel>,
    clock: FrameClock,
    phase: Phase,
    frame: usize,
    phase_started: Instant,
    steps: Vec<Step>,
    next: usize,
    settle: usize,
    inflight: Option<Inflight>,
    theme_samples: perf::Samples,
    font_samples: perf::Samples,
    cold_ms: Vec<f64>,
    /// Samples whose frame did not re-shape the diff's lines.
    stale: usize,
    blocks_visible: usize,
}

impl ThemeBench {
    fn view(&self, cx: &App) -> Option<Entity<DiffView>> {
        self.diff.read(cx).view().cloned()
    }

    fn open(&mut self, cx: &mut Context<Shell>) {
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

    /// Scrolls to the first thread that has a card in the diff.
    fn jump_to_thread(&mut self, cx: &mut Context<Shell>) {
        let session = self.session.clone();
        let thread = {
            let cards = self.diff.read(cx).cards().read(cx);
            let first = cards
                .placements()
                .first()
                .expect("the fixture has a thread");
            cards.threads()[first.thread].id.clone()
        };
        cx.defer(move |cx| {
            session.update(cx, |session, cx| session.jump_to_thread(thread, false, cx));
        });
        self.enter(Phase::Jumping);
    }

    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_started = Instant::now();
    }

    fn issue(&mut self, step: Step, cx: &mut Context<Shell>) {
        let started = Rc::new(Cell::new(None));
        let marker = started.clone();
        cx.defer(move |cx| {
            marker.set(Some(Instant::now()));
            apply(step.change, cx);
        });
        self.inflight = Some(Inflight {
            step,
            started,
            stage: Stage::Issued,
        });
    }

    fn running(&mut self, cx: &mut Context<Shell>) {
        let Some(inflight) = self.inflight.as_mut() else {
            if self.settle > 0 {
                self.settle -= 1;
            } else if let Some(&step) = self.steps.get(self.next) {
                self.next += 1;
                self.issue(step, cx);
            } else {
                self.report();
                self.phase = Phase::Done;
            }
            return;
        };
        match inflight.stage {
            Stage::Issued => {
                // The first frame after the command is the one that draws it.
                if inflight.started.get().is_some() {
                    check_applied(inflight.step.change, cx);
                    inflight.stage = Stage::Drawn;
                }
            }
            Stage::Drawn => {
                let step = inflight.step;
                let started = inflight.started.get().expect("the command ran");
                let ended = self.clock.last_frame_end().expect("a frame has ended");
                let elapsed = ended.saturating_duration_since(started);
                let reshaped = self
                    .view(cx)
                    .is_some_and(|view| view.read(cx).last_frame().shaped > 0);
                self.inflight = None;
                self.settle = SETTLE_FRAMES;
                self.record(step, elapsed, reshaped);
            }
        }
    }

    fn record(&mut self, step: Step, elapsed: Duration, reshaped: bool) {
        let Some(kind) = step.kind else {
            return;
        };
        if !reshaped {
            self.stale += 1;
        }
        match (kind, step.cold) {
            (Kind::Theme, _) => self.theme_samples.push(elapsed),
            (Kind::Font, true) => self.cold_ms.push(elapsed.as_secs_f64() * 1000.0),
            (Kind::Font, false) => self.font_samples.push(elapsed),
        }
    }

    fn report(&self) {
        let cold_max = self.cold_ms.iter().copied().fold(0.0, f64::max);
        let mut theme = self
            .theme_samples
            .report("theme_switch", 99.0, THEME_BUDGET_MS)
            .with("themes", "vercel dark/light")
            .with("diff_blocks_visible", self.blocks_visible);
        let mut font = self
            .font_samples
            .report("buffer_font_switch", 99.0, FONT_BUDGET_MS)
            .with("fonts", FONTS.join(", "))
            .with("cold_max_ms", cold_max)
            .with("diff_blocks_visible", self.blocks_visible);
        // A frame that did not re-shape the diff did not show the new state.
        theme.pass &= self.stale == 0;
        font.pass &= self.stale == 0;
        theme = theme.with("stale_frames", self.stale);
        font = font.with("stale_frames", self.stale);
        theme.print();
        font.print();
    }
}

impl Probe for ThemeBench {
    fn clock(&self) -> &FrameClock {
        &self.clock
    }

    fn frame(&mut self, window: &mut Window, cx: &mut Context<Shell>) {
        match self.phase {
            Phase::Warmup => {
                self.frame += 1;
                if self.frame >= WARMUP_FRAMES {
                    self.open(cx);
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
                    self.jump_to_thread(cx);
                }
            }
            Phase::Jumping => {
                let blocks = self
                    .view(cx)
                    .map_or(0, |view| view.read(cx).last_frame().blocks);
                assert!(
                    self.phase_started.elapsed() < WAIT,
                    "no thread card on screen within {WAIT:?}"
                );
                if blocks > 0 {
                    self.blocks_visible = blocks;
                    self.settle = SETTLE_FRAMES;
                    self.phase = Phase::Running;
                }
            }
            Phase::Running => self.running(cx),
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
    gpui_kit::application()
        .with_assets(assets::AppAssets)
        .run(move |cx: &mut App| {
            app::init(
                cx,
                &Setup {
                    fixtures: true,
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
                let checks = ChecksPanel::new(window, cx);
                let log = LogPanel::new(window, cx);
                let layout = DockLayout::h_split()
                    .child(
                        DockLayout::tabs()
                            .panel_view(panel_handle(diff.clone()), cx)
                            .panel_view(panel_handle(conversation), cx)
                            .panel_view(panel_handle(checks), cx),
                        Some(px(820.0)),
                    )
                    .child(
                        DockLayout::tabs().panel_view(panel_handle(log), cx),
                        Some(px(340.0)),
                    );
                dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
                let bench = ThemeBench {
                    session: Session::global(cx),
                    diff,
                    clock: FrameClock::new(),
                    phase: Phase::Warmup,
                    frame: 0,
                    phase_started: Instant::now(),
                    steps: script(&args),
                    next: 0,
                    settle: 0,
                    inflight: None,
                    theme_samples: perf::Samples::default(),
                    font_samples: perf::Samples::default(),
                    cold_ms: Vec::new(),
                    stale: 0,
                    blocks_visible: 0,
                };
                cx.new(|cx| Shell::new(dock, false, window, cx).with_probe(Box::new(bench)))
            })
            .expect("open the main window");
            cx.activate(true);
        });
}
