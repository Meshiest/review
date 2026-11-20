//! A terminal as a dock panel: a shell on this PC or over SSH, in the main
//! area or beside the CI log, moved by drag like any panel. Its tab says
//! the machine, the directory and whether the program ended; closing the
//! tab ends the program. It follows the theme and the buffer font.
//!
//! A launch that makes a worktree first opens its tab at once, with the
//! numbered steps it goes through ([`Checklist`]); the terminal replaces
//! them in the same tab when its program starts.
//!
//! Terminals are not restarted with the app: a restored layout holds a
//! placeholder that offers to open the same terminal again.

use std::rc::Rc;

use gpui::prelude::FluentBuilder as _;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement, Render, SharedString, Styled,
    Subscription, Task, Window, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::dock::{
    BasePanel, Panel, PanelEvent, PanelInfo, PanelState, panel_handle, register_panel,
};
use gpui_kit::component::{Icon, h_flex, v_flex};
use term::{ExitState, SessionConfig};
use term_view::{TermStyle, TerminalEvent, TerminalView};
use theme::ActiveTheme;

use crate::checklist::{Checklist, StepState};
use crate::panels::{close_button, tab_label};
use crate::terminals::TerminalSpec;
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::worktrees::Step;

pub const TERMINAL: &str = "Terminal";

/// Where the panel's program is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    /// Making the worktree before the program starts: the tab shows the
    /// steps.
    Preparing,
    /// Finding the programs on the PATH, off the UI thread.
    Starting,
    Running,
    /// The program ended; the screen stays readable.
    Exited(Option<i32>),
    /// Restored from a saved layout: nothing runs until the user asks.
    NotStarted,
    Failed(SharedString),
}

/// Runs a launch again from its first step.
pub type Retry = Rc<dyn Fn(&mut Window, &mut App)>;

/// A launch whose program has not started: what the tab says and the steps.
struct Launching {
    title: String,
    checklist: Checklist,
    retry: Option<Retry>,
}

pub struct TerminalPanel {
    id: EntityId,
    focus: FocusHandle,
    spec: TerminalSpec,
    view: Option<Entity<TerminalView>>,
    state: RunState,
    launching: Option<Launching>,
    spinners: SpinnerSlots,
    /// The view takes focus once it is up, if the panel holds it then.
    focus_view: bool,
    _starting: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl gpui::EventEmitter<PanelEvent> for TerminalPanel {}

/// The theme's terminal look, with the grid kept off the pane's edges.
fn term_style(cx: &App) -> TermStyle {
    let mut style = TermStyle::from_theme(&ui::theme(cx));
    style.padding = px(ui::space::PANE_INSET);
    style
}

pub fn register(cx: &mut App) {
    register_panel(cx, TERMINAL, |build, _, cx| {
        let spec = match build.info() {
            PanelInfo::Panel(value) => serde_json::from_value(value.clone()).unwrap_or_default(),
            _ => TerminalSpec::default(),
        };
        panel_handle(TerminalPanel::placeholder(spec, cx))
    });
}

impl TerminalPanel {
    /// Starts `spec`'s program in a new panel.
    pub fn spawn(spec: TerminalSpec, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut panel = Self::new(spec, cx);
            panel.start(cx);
            panel
        })
    }

    /// Starts `config` (a benchmark's own program) in a panel titled as
    /// `spec`, which "Run again" starts instead.
    pub fn spawn_config(spec: TerminalSpec, config: SessionConfig, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let mut panel = Self::new(spec, cx);
            panel.start_config(Ok(config), cx);
            panel
        })
    }

    /// A panel that runs nothing until "Reopen".
    pub fn placeholder(spec: TerminalSpec, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| Self::new(spec, cx))
    }

    /// A launch's tab before its program starts, titled `title` and showing
    /// `checklist`; [`Self::run`] starts the program in it. `retry` starts
    /// the launch again after a failed step.
    pub fn launching(
        title: String,
        spec: TerminalSpec,
        checklist: Checklist,
        retry: Option<Retry>,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let mut panel = Self::new(spec, cx);
            panel.state = RunState::Preparing;
            panel.launching = Some(Launching {
                title,
                checklist,
                retry,
            });
            panel
        })
    }

    /// The launch's steps while its program has not started.
    #[must_use]
    pub fn checklist(&self) -> Option<&Checklist> {
        self.launching
            .as_ref()
            .map(|launching| &launching.checklist)
    }

    /// Changes the launch's steps; nothing once the program started.
    pub fn update_checklist(&mut self, f: impl FnOnce(&mut Checklist), cx: &mut Context<Self>) {
        if let Some(launching) = &mut self.launching {
            f(&mut launching.checklist);
            cx.notify();
        }
    }

    /// Shows the launch's steps from the start again, in this tab.
    pub fn relaunch(&mut self, checklist: Checklist, cx: &mut Context<Self>) {
        if let Some(launching) = &mut self.launching {
            launching.checklist = checklist;
            self.state = RunState::Preparing;
            cx.notify();
        }
    }

    /// Starts `spec`'s program in place of the launch's steps.
    pub fn run(&mut self, spec: TerminalSpec, cx: &mut Context<Self>) {
        self.launching = None;
        self.spec = spec;
        self.start(cx);
    }

    fn new(spec: TerminalSpec, cx: &mut Context<Self>) -> Self {
        let theme = cx.observe_global::<ActiveTheme>(|this: &mut Self, cx| {
            let style = term_style(cx);
            if let Some(view) = &this.view {
                view.update(cx, |view, cx| view.set_style(style, cx));
            }
            cx.notify();
        });
        Self {
            id: cx.entity_id(),
            focus: cx.focus_handle(),
            spec,
            view: None,
            state: RunState::NotStarted,
            launching: None,
            spinners: SpinnerSlots::for_dock(cx),
            focus_view: false,
            _starting: None,
            _subscriptions: vec![theme],
        }
    }

    /// Starts the program; a running one is ended first. The programs are
    /// looked up on a background thread, so it runs a moment later.
    pub fn start(&mut self, cx: &mut Context<Self>) {
        self.stop(cx);
        self.view = None;
        self.state = RunState::Starting;
        self.focus_view = true;
        let spec = self.spec.clone();
        let config = cx
            .background_executor()
            .spawn(async move { spec.session_config() });
        self._starting = Some(cx.spawn(async move |this, cx| {
            let config = config.await;
            let _ = this.update(cx, |this, cx| this.start_config(config, cx));
        }));
        cx.notify();
    }

    fn start_config(&mut self, config: Result<SessionConfig, String>, cx: &mut Context<Self>) {
        self.stop(cx);
        let config = match config {
            Ok(config) => config,
            Err(error) => {
                self.state = RunState::Failed(error.into());
                cx.notify();
                return;
            }
        };
        let style = term_style(cx);
        match TerminalView::spawn(config, style, cx) {
            Ok(view) => {
                self._subscriptions.truncate(1);
                self._subscriptions.push(cx.subscribe(
                    &view,
                    |this, _, event: &TerminalEvent, cx| {
                        let TerminalEvent::Exited(state) = event;
                        let code = match state {
                            ExitState::Exited(status) => status.and_then(|s| s.code()),
                            ExitState::Running => return,
                        };
                        this.state = RunState::Exited(code);
                        cx.notify();
                    },
                ));
                self.view = Some(view);
                self.state = RunState::Running;
            }
            Err(error) => {
                self.state = RunState::Failed(format!("{error}").into());
            }
        }
        cx.notify();
    }

    /// Ends the program and everything it started, without waiting.
    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = &self.view {
            view.update(cx, |view, cx| view.close(cx));
        }
    }

    /// Starts it again from a placeholder, or after it ended.
    pub fn reopen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.start(cx);
        window.focus(&self.focus, cx);
    }

    #[must_use]
    pub fn spec(&self) -> &TerminalSpec {
        &self.spec
    }

    #[must_use]
    pub fn state(&self) -> &RunState {
        &self.state
    }

    #[must_use]
    pub fn view(&self) -> Option<&Entity<TerminalView>> {
        self.view.as_ref()
    }

    fn badge(&self) -> Option<(String, bool)> {
        match &self.state {
            RunState::Running => None,
            RunState::Preparing => Some(match self.checklist() {
                Some(list) if list.error().is_some() => ("failed".to_owned(), true),
                Some(list) if list.stopped() => ("waiting".to_owned(), false),
                _ => ("starting".to_owned(), false),
            }),
            RunState::Starting => Some(("starting".to_owned(), false)),
            RunState::Exited(Some(0)) => Some(("exited".to_owned(), false)),
            RunState::Exited(Some(code)) => Some((format!("exited {code}"), true)),
            RunState::Exited(None) => Some(("ended".to_owned(), false)),
            RunState::NotStarted => Some(("closed".to_owned(), false)),
            RunState::Failed(_) => Some(("failed".to_owned(), true)),
        }
    }

    fn closed_note(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let (text, error) = match &self.state {
            RunState::NotStarted => (
                format!(
                    "This terminal on {} was closed when the app quit.",
                    self.spec.machine
                ),
                false,
            ),
            RunState::Failed(error) => (format!("Could not start: {error}"), true),
            RunState::Starting => ("Starting...".to_owned(), false),
            RunState::Preparing | RunState::Running | RunState::Exited(_) => (String::new(), false),
        };
        // An agent's review token ended with it; "Review with agent" starts
        // a new one.
        let reopen = (!matches!(self.state, RunState::Starting | RunState::Preparing)
            && self.spec.agent.is_none())
        .then(|| {
            Button::toolbar("terminal-reopen", "Reopen")
                .on_click(cx.listener(|this, _, window, cx| this.reopen(window, cx)))
        });
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap(px(10.0))
            .child(
                div()
                    .text_color(if error { c.danger } else { c.text_secondary })
                    .child(text),
            )
            .when_some(self.spec.cwd.clone(), |el, cwd| {
                el.child(div().mono(&t).small(&t).text_color(c.text_muted).child(cwd))
            })
            .children(reopen)
            .into_any_element()
    }

    /// The launch's numbered steps: a spinner on the one under way, check
    /// marks on finished ones, the error and "Retry" on a failed one.
    fn steps_view(&self, launching: &Launching, cx: &mut Context<Self>) -> gpui::AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let rows = launching
            .checklist
            .steps()
            .iter()
            .enumerate()
            .map(|(ix, (step, state))| {
                let mark = match state {
                    StepState::Running => self
                        .spinners
                        .slot(STEP_MARK - 4.0, c.accent)
                        .into_any_element(),
                    StepState::Done => step_icon(IconName::CircleCheck, c.ok),
                    StepState::Failed(_) => step_icon(IconName::CircleX, c.danger),
                    StepState::Blocked(_) => step_icon(IconName::TriangleAlert, c.warn),
                    StepState::Waiting => div()
                        .size(px(STEP_MARK))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_1()
                        .border_color(c.border_strong)
                        .tiny(&t)
                        .text_color(c.text_muted)
                        .child((ix + 1).to_string())
                        .into_any_element(),
                };
                let failed = matches!(state, StepState::Failed(_));
                let (label_color, detail) = match state {
                    StepState::Waiting => (c.text_muted, None),
                    StepState::Running | StepState::Done => (c.text, None),
                    StepState::Failed(text) => (c.danger, Some(text.clone())),
                    StepState::Blocked(text) => (c.text, Some(text.clone())),
                };
                // A failed step starts over; a clone in a terminal goes on
                // once the user says it is done.
                let action = match (state, &launching.retry) {
                    (StepState::Failed(_), Some(retry)) => Some(("launch-retry", "Retry", retry)),
                    (StepState::Blocked(_), Some(retry)) if *step == Step::CloneInTerminal => {
                        Some(("launch-continue", "Continue", retry))
                    }
                    _ => None,
                };
                let action = action.map(|(id, label, retry)| {
                    let retry = retry.clone();
                    h_flex().pt(px(4.0)).child(
                        Button::new(id, label, ButtonKind::Secondary)
                            .on_click(move |_, window, cx| retry(window, cx)),
                    )
                });
                h_flex()
                    .debug_selector(move || format!("launch-step-{ix}"))
                    .gap(px(10.0))
                    .items_start()
                    .child(
                        div()
                            .flex_none()
                            .size(px(STEP_MARK))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(mark),
                    )
                    .child(
                        v_flex()
                            .min_w_0()
                            .gap(px(2.0))
                            .child(div().text_color(label_color).child(step.label()))
                            .children(detail.map(|detail| {
                                div()
                                    .small(&t)
                                    .text_color(if failed { c.danger } else { c.text_secondary })
                                    .child(detail)
                            }))
                            .children(action),
                    )
            });
        v_flex()
            .debug_selector(|| "launch-steps".into())
            .size_full()
            .p(px(24.0))
            .gap(px(12.0))
            .bg(c.surface)
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(c.text)
                    .child(launching.title.clone()),
            )
            .children(rows)
            .into_any_element()
    }
}

/// The size of a step's number, spinner or icon.
const STEP_MARK: f32 = 18.0;

fn step_icon(icon: IconName, color: gpui::Hsla) -> gpui::AnyElement {
    Icon::new(icon)
        .size(px(STEP_MARK))
        .text_color(color)
        .into_any_element()
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        match &self.view {
            Some(view) => view.focus_handle(cx),
            None => self.focus.clone(),
        }
    }
}

impl BasePanel for TerminalPanel {
    fn panel_name(&self) -> &'static str {
        TERMINAL
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        self.spinners.set_shown(active);
        cx.notify();
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.spinners.set_shown(false);
        self.stop(cx);
    }

    fn dump(&self, _: &App) -> PanelState {
        PanelState {
            panel_name: TERMINAL.to_owned(),
            children: Vec::new(),
            info: PanelInfo::panel(serde_json::to_value(&self.spec).unwrap_or_default()),
        }
    }
}

impl Panel for TerminalPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let badge = self.badge().map(|(text, bad)| {
            div()
                .flex_none()
                .tiny(&t)
                .text_color(if bad { c.danger } else { c.text_muted })
                .child(text)
        });
        let selector = format!("close-terminal-{}", self.id.as_u64());
        h_flex()
            .cursor_pointer()
            .gap(px(6.0))
            .items_center()
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size(px(12.0))
                    .text_color(c.text_muted),
            )
            .child(tab_label(div()).child(match &self.launching {
                Some(launching) => launching.title.clone(),
                None => self.spec.title(),
            }))
            .children(badge)
            .child(close_button(self.id, selector, cx))
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_view
            && let Some(view) = self.view.clone()
        {
            self.focus_view = false;
            if self.focus.is_focused(window) {
                cx.defer_in(window, move |_, window, cx| {
                    window.focus(&view.focus_handle(cx), cx);
                });
            }
        }
        self.spinners.clear();
        let t = ui::theme(cx);
        let c = &t.colors;
        let body = match (&self.view, &self.state, &self.launching) {
            (_, _, Some(launching)) => self.steps_view(launching, cx),
            (Some(view), RunState::Running | RunState::Exited(_), None) => div()
                .flex_1()
                .min_h_0()
                .child(view.clone())
                .into_any_element(),
            _ => self.closed_note(cx),
        };
        let ended = matches!(self.state, RunState::Exited(_)).then(|| {
            h_flex()
                .flex_none()
                .h(px(28.0))
                .px(px(10.0))
                .gap(px(10.0))
                .items_center()
                .border_t_1()
                .border_color(c.border)
                .small(&t)
                .text_color(c.text_muted)
                .child(self.badge().map(|(text, _)| text).unwrap_or_default())
                .child(div().flex_1())
                .when(self.spec.agent.is_some(), |el| {
                    el.child("Review with agent starts a new session")
                })
                .when(self.spec.agent.is_none(), |el| {
                    el.child(
                        Button::new("terminal-restart", "Run again", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, window, cx| this.reopen(window, cx))),
                    )
                })
        });
        v_flex()
            .id(("terminal-panel", self.id.as_u64()))
            .size_full()
            .bg(gpui::Rgba::from(t.terminal.background))
            .track_focus(&self.focus)
            .child(body)
            .children(ended)
    }
}
