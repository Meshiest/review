//! The Log panel: the job picked in the Checks panel, through `log_view`.
//!
//! While the job runs the log is read from the last offset about every two
//! seconds (sooner right after new bytes) and pushed into a [`LogFeed`];
//! a forge that starts the log over (offset 0) gets a fresh feed, and the
//! reads stop once the forge says the log is complete. GitHub has no log
//! until a job ends, so there the panel says so, counts the job's time and
//! loads the log at the end. When a failed job's log is in, every group is
//! folded except the one with the error, which is scrolled to.

use std::time::Duration;

use forge::model::{CheckStep, CiState};
use gpui::{
    App, AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement, Render, SharedString, Styled, Task, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_kit::component::{dock::Panel, h_flex, v_flex};
use log_view::{LineKind, LogEvent, LogFeed, LogStyle, LogView};
use theme::ActiveTheme;

use crate::ci::{self, Piece, Tail};
use crate::panels::{Binding, PanelKind, dock_panel, header};
use crate::session::{LogTarget, Session};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, Button, ButtonKind, CI_HEADER, ci_icon};

/// How often a shown duration counts up.
const TICK: Duration = Duration::from_secs(1);
/// How often a finished feed is checked for having parsed everything.
const SETTLE_POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Status {
    Loading,
    /// GitHub: the job runs and its log does not exist yet.
    Waiting,
    Following,
    Complete,
    /// The forge has no log API: its web page has the log.
    Unavailable,
    Failed(SharedString),
}

/// One job's log on screen.
struct Tailing {
    generation: u64,
    feed: LogFeed,
    view: Entity<LogView>,
    status: Status,
    /// A step to scroll to once its lines are in, with its ask's generation.
    step: Option<(u32, u64)>,
    steps: Vec<CheckStep>,
    _reads: Task<()>,
    _tick: Option<Task<()>>,
}

/// The last Log panel built, for `--ci-demo fold`.
struct Latest(gpui::WeakEntity<LogPanel>);

impl gpui::Global for Latest {}

pub struct LogPanel {
    focus: FocusHandle,
    session: Entity<Session>,
    binding: Binding,
    tailing: Option<Tailing>,
    spinners: SpinnerSlots,
}

dock_panel!(LogPanel, "Log", bound, spinners);

impl LogPanel {
    pub fn new(_: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let session = Session::global(cx);
            cx.observe(&session, |this: &mut Self, _, cx| this.follow_session(cx))
                .detach();
            cx.observe_global::<ActiveTheme>(|this: &mut Self, cx| {
                let style = LogStyle::from_theme(&ui::theme(cx));
                if let Some(tailing) = &this.tailing {
                    tailing
                        .view
                        .update(cx, |view, cx| view.set_style(style, cx));
                }
            })
            .detach();
            let binding = Binding::new(None, PanelKind::Log, cx);
            let latest = Latest(cx.weak_entity());
            cx.set_global(latest);
            let mut panel = Self {
                focus: cx.focus_handle(),
                session,
                binding,
                tailing: None,
                spinners: SpinnerSlots::for_dock(cx),
            };
            panel.follow_session(cx);
            panel
        })
    }

    /// The log of the picked job, while one is picked.
    pub fn feed(&self) -> Option<&LogFeed> {
        self.tailing.as_ref().map(|tailing| &tailing.feed)
    }

    fn target(&self, cx: &App) -> Option<LogTarget> {
        self.session.read(cx).open_pr()?.ci.log.clone()
    }

    /// Starts the picked job's log, or passes on a step to scroll to.
    fn follow_session(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target(cx) else {
            if self.tailing.take().is_some() {
                cx.notify();
            }
            return;
        };
        match &mut self.tailing {
            Some(tailing) if tailing.generation == target.generation => {
                tailing.steps.clone_from(&target.job.steps);
                if target.step.is_some_and(|step| Some(step) != tailing.step) {
                    tailing.step = target.step;
                    self.scroll_to_step(cx);
                }
            }
            _ => self.start(target, cx),
        }
        cx.notify();
    }

    fn new_view(feed: &LogFeed, cx: &mut Context<Self>) -> Entity<LogView> {
        let style = LogStyle::from_theme(&ui::theme(cx));
        let view = cx.new(|cx| LogView::new(feed, style, cx));
        cx.subscribe(&view, |this: &mut Self, _, event: &LogEvent, cx| {
            let LogEvent::Appended(_) = event;
            this.scroll_to_step(cx);
        })
        .detach();
        view
    }

    fn start(&mut self, target: LogTarget, cx: &mut Context<Self>) {
        let feed = LogFeed::new();
        let view = Self::new_view(&feed, cx);
        let fixture = self.session.read(cx).fixture_log(&target.job);
        let reads = match fixture {
            Some(bytes) => {
                feed.push(bytes);
                feed.finish();
                self.after_complete(target.generation, cx)
            }
            None => self.read_live(&target, cx),
        };
        self.tailing = Some(Tailing {
            generation: target.generation,
            feed,
            view,
            status: Status::Loading,
            step: target.step,
            steps: target.job.steps.clone(),
            _reads: reads,
            _tick: Some(self.tick(cx)),
        });
    }

    /// Repaints once a second while a duration counts up.
    fn tick(&self, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let going = this.update(cx, |this, cx| {
                    let going = this.tailing.as_ref().is_some_and(|t| {
                        matches!(
                            t.status,
                            Status::Waiting | Status::Following | Status::Loading
                        )
                    });
                    if going {
                        cx.notify();
                    }
                    going
                });
                if !matches!(going, Ok(true)) {
                    return;
                }
            }
        })
    }

    /// Reads the log from the forge until it is complete.
    fn read_live(&self, target: &LogTarget, cx: &mut Context<Self>) -> Task<()> {
        let Some(services) = self.session.read(cx).services().cloned() else {
            return Task::ready(());
        };
        let (generation, repo, job) = (
            target.generation,
            target.repo.clone(),
            target.job.id.clone(),
        );
        let mut wait_for_end = Tail::of(target.forge) == Tail::AfterFinish;
        cx.spawn(async move |this, cx| {
            let set = |status: Status, cx: &mut gpui::AsyncApp| {
                this.update(cx, |this, cx| this.set_status(generation, status, cx))
                    .is_ok()
            };
            let mut offset = 0u64;
            let mut retry = ci::RETRY_MIN;
            loop {
                if wait_for_end {
                    let (repo, job) = (repo.clone(), job.clone());
                    let read = services
                        .forge(&repo.host.clone(), move |forge| async move {
                            forge.ci_job(&repo, &job).await
                        })
                        .await;
                    match read {
                        Ok(job) if !job.is_finished() => {
                            if !set(Status::Waiting, cx) {
                                return;
                            }
                            cx.background_executor().timer(ci::FINISH_POLL).await;
                            continue;
                        }
                        Ok(_) => wait_for_end = false,
                        Err(error) => {
                            if !set(Status::Failed(error.to_string().into()), cx) {
                                return;
                            }
                            cx.background_executor().timer(retry).await;
                            retry = (retry * 2).min(ci::RETRY_MAX);
                            continue;
                        }
                    }
                }
                let (repo, job) = (repo.clone(), job.clone());
                let read = services
                    .forge(&repo.host.clone(), move |forge| async move {
                        forge.job_log(&repo, &job, offset).await
                    })
                    .await;
                let chunk = match read {
                    Ok(chunk) => chunk,
                    Err(forge::Error::Unsupported { .. }) => {
                        set(Status::Unavailable, cx);
                        return;
                    }
                    Err(error) => {
                        if !set(Status::Failed(error.to_string().into()), cx) {
                            return;
                        }
                        cx.background_executor().timer(retry).await;
                        retry = (retry * 2).min(ci::RETRY_MAX);
                        continue;
                    }
                };
                retry = ci::RETRY_MIN;
                let asked = offset;
                offset = chunk.next_offset;
                let complete = chunk.complete;
                let got = !chunk.bytes.is_empty();
                let piece = ci::piece(asked, chunk);
                let alive = this
                    .update(cx, |this, cx| this.take(generation, piece, complete, cx))
                    .is_ok();
                if !alive {
                    return;
                }
                if complete {
                    let settle = this.update(cx, |this, cx| this.after_complete(generation, cx));
                    if let Ok(settle) = settle {
                        settle.await;
                    }
                    return;
                }
                cx.background_executor().timer(ci::tail_delay(got)).await;
            }
        })
    }

    fn current(&mut self, generation: u64) -> Option<&mut Tailing> {
        self.tailing
            .as_mut()
            .filter(|tailing| tailing.generation == generation)
    }

    fn set_status(&mut self, generation: u64, status: Status, cx: &mut Context<Self>) {
        if let Some(tailing) = self.current(generation)
            && tailing.status != status
        {
            tailing.status = status;
            cx.notify();
        }
    }

    fn take(&mut self, generation: u64, piece: Piece, complete: bool, cx: &mut Context<Self>) {
        let restart = matches!(piece, Piece::Restart(_));
        if restart && self.current(generation).is_some() {
            let feed = LogFeed::new();
            let view = Self::new_view(&feed, cx);
            if let Some(tailing) = self.current(generation) {
                tailing.feed = feed;
                tailing.view = view;
            }
        }
        let Some(tailing) = self.current(generation) else {
            return;
        };
        let (Piece::Append(bytes) | Piece::Restart(bytes)) = piece;
        if !bytes.is_empty() {
            tailing.feed.push(bytes);
        }
        if complete {
            tailing.feed.finish();
        }
        tailing.status = if complete {
            Status::Complete
        } else {
            Status::Following
        };
        cx.notify();
    }

    /// Once the parser has every line: a failed job folds its groups but
    /// the one with the error, and a step asked for is scrolled to.
    fn after_complete(&mut self, generation: u64, cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                let settled = this.update(cx, |this, _| {
                    this.current(generation)
                        .is_none_or(|tailing| tailing.feed.is_settled())
                });
                match settled {
                    Ok(true) => break,
                    Ok(false) => cx.background_executor().timer(SETTLE_POLL).await,
                    Err(_) => return,
                }
            }
            this.update(cx, |this, cx| this.settled(generation, cx))
                .ok();
        })
    }

    fn settled(&mut self, generation: u64, cx: &mut Context<Self>) {
        let failed = self
            .target(cx)
            .is_some_and(|target| target.job.state == CiState::Fail);
        let Some(tailing) = self.current(generation) else {
            return;
        };
        tailing.status = Status::Complete;
        if failed && tailing.step.is_none() {
            let error = tailing.feed.read(|buffer| {
                buffer
                    .lines(buffer.dropped()..buffer.total())
                    .filter(|line| line.kind == LineKind::Error)
                    .last()
                    .map(|line| {
                        let group = buffer.group_containing(line.id).map(|group| group.start);
                        (line.id, group)
                    })
            });
            let view = tailing.view.clone();
            view.update(cx, |view, cx| {
                view.collapse_all(cx);
                if let Some((_, Some(group))) = error {
                    view.set_group_collapsed(group, false, cx);
                }
            });
            if let Some((line, _)) = error {
                self.reveal(generation, line, cx);
            }
        }
        self.scroll_to_step(cx);
        cx.notify();
    }

    /// Scrolls to the asked-for step once its first line is in.
    fn scroll_to_step(&mut self, cx: &mut Context<Self>) {
        let Some(tailing) = self.tailing.as_mut() else {
            return;
        };
        let Some((number, _)) = tailing.step else {
            return;
        };
        let Some(step) = tailing.steps.iter().find(|s| s.number == number).cloned() else {
            return;
        };
        let Some(line) = tailing.feed.read(|buffer| ci::step_line(buffer, &step)) else {
            return;
        };
        let generation = tailing.generation;
        tailing.step = None;
        let view = tailing.view.clone();
        let group = tailing
            .feed
            .read(|buffer| buffer.group_containing(line).map(|group| group.start));
        if let Some(group) = group {
            view.update(cx, |view, cx| view.set_group_collapsed(group, false, cx));
        }
        self.reveal(generation, line, cx);
    }

    /// Puts `line` near the top of the view, groups folded above it
    /// counted out.
    fn reveal(&mut self, generation: u64, line: u64, cx: &mut Context<Self>) {
        let Some(tailing) = self.current(generation) else {
            return;
        };
        let view = tailing.view.clone();
        let row = tailing.feed.read(|buffer| {
            let view = view.read(cx);
            ci::row_of_line(buffer, line, |header| view.is_group_collapsed(header))
        });
        view.update(cx, |view, cx| {
            let height = f64::from(view.style().line_height);
            let top = (row as f64 * height - height * 3.0).max(0.0);
            view.set_scroll_y(top);
            cx.notify();
        });
    }

    /// Folds every group of the log on screen and goes to its top.
    pub fn fold_all(cx: &mut App) {
        let Some(panel) = cx.try_global::<Latest>().and_then(|l| l.0.upgrade()) else {
            return;
        };
        panel.update(cx, |this, cx| {
            if let Some(tailing) = &this.tailing {
                tailing.view.update(cx, |view, cx| {
                    view.collapse_all(cx);
                    view.set_scroll_y(0.0);
                });
            }
        });
    }

    fn status_text(&self, target: &LogTarget, tailing: &Tailing) -> String {
        let lines = tailing.feed.read(|buffer| buffer.total());
        let lines = format!("{lines} lines");
        let ran = ci::duration(target.job.started_at, target.job.completed_at);
        match &tailing.status {
            Status::Loading => "loading".to_owned(),
            Status::Waiting => match ran {
                Some(ran) => format!("log available when the job finishes, running {ran}"),
                None => "log available when the job finishes, queued".to_owned(),
            },
            Status::Following => match ran {
                Some(ran) => format!("following, {lines}, running {ran}"),
                None => format!("following, {lines}"),
            },
            Status::Complete => lines,
            Status::Unavailable => "this forge has no log API; open the job on the web".to_owned(),
            Status::Failed(error) => format!("not read: {error}"),
        }
    }
}

impl Panel for LogPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.binding.title(None, cx)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for LogPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.spinners.clear();
        let t = ui::theme(cx);
        let c = &t.colors;
        let target = self.target(cx);
        let (Some(target), Some(tailing)) = (target, self.tailing.as_ref()) else {
            let note = if self.session.read(cx).open_pr().is_some() {
                "Pick a job in Checks to see its log"
            } else {
                "No pull request open"
            };
            return v_flex()
                .id("log-panel")
                .size_full()
                .bg(c.surface)
                .child(header(cx, div().child("Job log"), div()))
                .child(ui::empty_note(cx, note));
        };
        let status = self.status_text(&target, tailing);
        let view = tailing.view.clone();
        let following = view.read(cx).is_following();
        let (collapse, expand, latest) = (view.clone(), view.clone(), view.clone());
        let title = h_flex()
            .gap(px(8.0))
            .min_w_0()
            .items_center()
            .child(ci_icon(&t, target.job.state, CI_HEADER, &self.spinners))
            .child(
                div()
                    .truncate()
                    .text_color(c.text)
                    .child(format!("{} / {}", target.run_name, target.job.name)),
            )
            .child(div().truncate().text_color(c.text_muted).child(status));
        let tools = h_flex()
            .gap(px(4.0))
            .flex_none()
            .when(!following, |el| {
                el.child(
                    Button::new("log-latest", "Latest", ButtonKind::Ghost).on_click(
                        move |_, _, cx| latest.update(cx, |view, cx| view.scroll_to_latest(cx)),
                    ),
                )
            })
            .child(
                Button::new("log-collapse", "Fold all", ButtonKind::Ghost).on_click(
                    move |_, _, cx| collapse.update(cx, |view, cx| view.collapse_all(cx)),
                ),
            )
            .child(
                Button::new("log-expand", "Unfold all", ButtonKind::Ghost)
                    .on_click(move |_, _, cx| expand.update(cx, |view, cx| view.expand_all(cx))),
            );
        let body = match tailing.status {
            Status::Waiting | Status::Unavailable => ui::empty_note(
                cx,
                if tailing.status == Status::Waiting {
                    "GitHub keeps a job's log until it finishes; it loads here then"
                } else {
                    "No log here for this forge"
                },
            )
            .into_any_element(),
            _ => div().flex_1().min_h_0().child(view).into_any_element(),
        };
        v_flex()
            .id("log-panel")
            .size_full()
            .bg(c.surface)
            .child(header(cx, title, tools))
            .child(body)
    }
}
