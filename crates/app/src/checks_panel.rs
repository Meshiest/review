//! The Checks panel: the open pull request's runs at its head, each with
//! its jobs (grouped by stage on GitLab) and, where the forge has them,
//! steps. Rows show state, duration and attempt, open a job's log, and
//! re-run what failed; what the forge cannot do here is a link to its web
//! page. It moves when the watch engine reports a change, never on a
//! timer of its own.

use forge::model::{CheckStep, CiJob, CiRun, CiState, JobId};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FocusHandle, FontWeight,
    InteractiveElement as _, IntoElement, ParentElement, Render, ScrollHandle, SharedString,
    StatefulInteractiveElement as _, Styled, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, dock::Panel, h_flex, v_flex};
use scroll::HandleAutoscroll;
use theme::Theme;

pub mod job_menu;

use self::job_menu::JobMenu;
use crate::ci::{self, CiActions, CiLoad, Retry, RetryConfirm};
use crate::model::{Check, PrKey};
use crate::panels::{Binding, PanelKind, dock_panel, header};
use crate::session::{CiNotice, OpenPr, Session};
use crate::subscriptions::Subscriptions;
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{self, Button, ButtonKind, CI_ROW, TextExt as _, ci_color, ci_icon, fold};
use crate::untrusted::is_web_url;

const RUN_ROW: f32 = 32.0;
const JOB_ROW: f32 = 28.0;
const INDENT: f32 = 22.0;

pub struct ChecksPanel {
    focus: FocusHandle,
    session: Entity<Session>,
    binding: Binding,
    spinners: SpinnerSlots,
    autoscroll: HandleAutoscroll,
    scroll: ScrollHandle,
}

dock_panel!(ChecksPanel, "Checks", bound, spinners);

impl ChecksPanel {
    /// A Checks panel that follows the focused pull request.
    pub fn new(window: &mut Window, cx: &mut App) -> Entity<Self> {
        Self::for_pr(None, window, cx)
    }

    /// A Checks panel bound to `pr`, or following the focus for `None`.
    pub fn for_pr(pr: Option<PrKey>, _: &mut Window, cx: &mut App) -> Entity<Self> {
        cx.new(|cx| {
            let session = Session::global(cx);
            cx.observe(&session, |_, _, cx| cx.notify()).detach();
            if let Some(subscriptions) = Subscriptions::global(cx) {
                cx.observe(&subscriptions, |_, _, cx| cx.notify()).detach();
            }
            let scroll = ScrollHandle::new();
            Self {
                focus: cx.focus_handle(),
                binding: Binding::new(pr, PanelKind::Checks, cx),
                session,
                spinners: SpinnerSlots::for_dock(cx),
                autoscroll: HandleAutoscroll::new(scroll.clone()),
                scroll,
            }
        })
    }

    /// The pull request this panel draws now.
    pub fn pr(&self, cx: &App) -> Option<PrKey> {
        self.binding.key(cx)
    }
}

impl Panel for ChecksPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let t = ui::theme(cx);
        let badge = Subscriptions::global(cx).is_some_and(|s| s.read(cx).checks_badge());
        self.binding
            .title(badge.then(|| dot(&t).into_any_element()), cx)
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

fn dot(t: &Theme) -> gpui::Div {
    div()
        .size(px(6.0))
        .flex_none()
        .rounded_full()
        .bg(t.colors.accent)
}

/// A text link to a forge page, only for web URLs.
fn web_link(
    t: &Theme,
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    url: Option<&str>,
) -> Option<AnyElement> {
    let url = url.filter(|url| is_web_url(url))?.to_owned();
    let id = id.into();
    let selector = format!("web-link-{id}");
    let accent = t.colors.accent_fg;
    Some(
        h_flex()
            .id(id)
            .flex_none()
            .gap(px(3.0))
            .items_center()
            .cursor_pointer()
            .small(t)
            .text_color(accent)
            .hover(|s| s.underline())
            .debug_selector(move || selector)
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                cx.open_url(&url);
            })
            .child(label)
            .child(
                Icon::new(IconName::ExternalLink)
                    .size(px(12.0))
                    .text_color(accent),
            )
            .into_any_element(),
    )
}

impl ChecksPanel {
    /// A click handler that changes the session, on this panel's pull
    /// request: it takes the focus first.
    fn listener<E>(
        &self,
        f: impl Fn(&mut Session, &mut Context<Session>) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let session = self.session.clone();
        let pr = self.binding.bound().cloned();
        move |_, _, cx| {
            // A row's own controls sit inside a toggle row: it must not also take the click.
            cx.stop_propagation();
            session.update(cx, |session, cx| {
                if let Some(pr) = &pr {
                    session.focus(pr, cx);
                }
                f(session, cx);
            });
        }
    }

    fn watch_button(&self, open: &OpenPr, cx: &App) -> Option<AnyElement> {
        let subscriptions = Subscriptions::global(cx)?;
        let watched = subscriptions.read(cx).is_watched(&open.key, cx);
        let key = open.key.clone();
        let (label, kind) = if watched {
            ("Watching", ButtonKind::Secondary)
        } else {
            ("Watch", ButtonKind::Ghost)
        };
        Some(
            Button::new("watch-pr", label, kind)
                .on_click(move |_, _, cx| {
                    subscriptions.update(cx, |s, cx| s.toggle(&key, cx));
                })
                .into_any_element(),
        )
    }

    fn summary(open: &OpenPr) -> String {
        let jobs: Vec<CiState> = open
            .ci
            .runs
            .iter()
            .flat_map(|run| run.jobs.iter().map(|job| job.state))
            .collect();
        let states: Vec<CiState> = if jobs.is_empty() {
            open.checks.iter().map(|check| check.state).collect()
        } else {
            jobs
        };
        let count = |state| states.iter().filter(|s| **s == state).count();
        format!(
            "{} passed, {} failed, {} running, {} queued",
            count(CiState::Pass),
            count(CiState::Fail),
            count(CiState::Running),
            count(CiState::Queued)
        )
    }

    fn notice(&self, notice: &CiNotice, t: &Theme, cx: &App) -> AnyElement {
        let color = if notice.error {
            t.colors.danger
        } else {
            t.colors.text_secondary
        };
        h_flex()
            .px(px(12.0))
            .py(px(6.0))
            .gap(px(8.0))
            .items_center()
            .border_b_1()
            .border_color(t.colors.border)
            .small(t)
            .child(div().flex_1().text_color(color).child(notice.text.clone()))
            .child(ui::icon_button(
                cx,
                "dismiss-ci-notice",
                IconName::Close,
                self.listener(|session, cx| session.dismiss_ci_notice(cx)),
            ))
            .into_any_element()
    }

    /// Asks before re-running more than one job, naming each.
    fn confirm(&self, confirm: &RetryConfirm, t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let jobs = confirm.jobs.iter().map(|name| {
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(ci_icon(t, CiState::Fail, CI_ROW, &self.spinners))
                .child(div().mono(t).text_color(c.text).child(name.clone()))
        });
        div()
            .p(px(10.0))
            .child(
                ui::card(cx)
                    .p(px(12.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .border_color(c.warn)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(c.text)
                            .child(format!(
                                "Re-run {} failed jobs of {}?",
                                confirm.jobs.len(),
                                confirm.run_name
                            )),
                    )
                    .child(
                        div()
                            .small(t)
                            .text_color(c.text_secondary)
                            .child("Each job starts again on the forge and uses runner time."),
                    )
                    .child(v_flex().gap(px(4.0)).py(px(2.0)).children(jobs))
                    .child(
                        h_flex()
                            .gap(px(8.0))
                            .child(
                                Button::new("confirm-retry", "Re-run jobs", ButtonKind::Primary)
                                    .on_click(
                                        self.listener(|session, cx| session.confirm_retry(cx)),
                                    ),
                            )
                            .child(
                                Button::new("cancel-retry", "Cancel", ButtonKind::Ghost).on_click(
                                    self.listener(|session, cx| session.cancel_retry(cx)),
                                ),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn run_rows(
        &self,
        open: &OpenPr,
        run: &CiRun,
        actions: CiActions,
        t: &Theme,
        cx: &App,
    ) -> Vec<AnyElement> {
        let c = &t.colors;
        let folded = open.ci.folded.contains(&run.id);
        let failed = ci::failed_jobs(run).count();
        let run_id = run.id.clone();
        let title = match run.number {
            Some(number) => format!("{} #{number}", run.name),
            None => run.name.clone(),
        };
        let context: Vec<&str> = [run.event.as_deref(), run.branch.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let retry = if failed == 0 {
            None
        } else if actions.retry {
            let retry = Retry::FailedJobs {
                run: run.id.clone(),
            };
            let label = if failed == 1 {
                "Re-run failed job"
            } else {
                "Re-run failed jobs"
            };
            Some(
                Button::new(("retry-run", ix_of(run)), label, ButtonKind::Secondary)
                    .on_click(self.listener(move |session, cx| {
                        session.request_retry(retry.clone(), cx);
                    }))
                    .into_any_element(),
            )
        } else {
            web_link(
                t,
                ("retry-run-web", ix_of(run)),
                "Re-run on the web",
                run.url.as_deref(),
            )
        };
        let watch = Subscriptions::global(cx).map(|subscriptions| {
            let watched = subscriptions.read(cx).is_run_watched(&open.key, &run.id);
            let (key, run_id) = (open.key.clone(), run.id.clone());
            let icon = if watched {
                IconName::Bell
            } else {
                IconName::BellOff
            };
            ui::icon_button(cx, ("watch-run", ix_of(run)), icon, move |_, _, cx| {
                subscriptions.update(cx, |s, cx| s.toggle_run(&key, &run_id, cx));
            })
        });
        let header = h_flex()
            .id(("run", ix_of(run)))
            .debug_selector(|| format!("run-{}", run.id))
            .h(px(RUN_ROW))
            .flex_none()
            .px(px(10.0))
            .gap(px(8.0))
            .items_center()
            .bg(c.surface_raised)
            .border_b_1()
            .border_color(c.border);
        let header = fold::toggle_row(
            header,
            t,
            self.listener(move |session, cx| {
                session.toggle_run_folded(&run_id, cx);
            }),
        )
        .child(
            Icon::new(if folded {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(px(14.0))
            .text_color(c.text_muted),
        )
        .child(ci_icon(t, run.state, CI_ROW, &self.spinners))
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(c.text)
                .child(title),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .small(t)
                .text_color(c.text_muted)
                .child(context.join(" · ")),
        )
        .children(ci::duration(run.started_at, run.completed_at).map(|d| {
            div()
                .flex_none()
                .mono(t)
                .tiny(t)
                .text_color(ci_color(t, run.state))
                .child(d)
        }))
        .children(retry)
        .children(watch)
        .children(web_link(
            t,
            ("run-web", ix_of(run)),
            "Web",
            run.url.as_deref(),
        ))
        .into_any_element();
        let mut rows = vec![header];
        if folded {
            return rows;
        }
        for (stage, jobs) in ci::stages(run) {
            if let Some(stage) = stage {
                rows.push(
                    div()
                        .h(px(22.0))
                        .pl(px(10.0 + INDENT))
                        .flex()
                        .items_end()
                        .tiny(t)
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_muted)
                        .child(stage.to_uppercase())
                        .into_any_element(),
                );
            }
            for job in jobs {
                rows.push(self.job_row(open, run, job, actions, t, cx));
                if open.ci.expanded.contains(&job.id) {
                    rows.extend(
                        job.steps
                            .iter()
                            .map(|step| self.step_row(job, step, actions, t)),
                    );
                }
            }
        }
        rows
    }

    fn job_row(
        &self,
        open: &OpenPr,
        run: &CiRun,
        job: &CiJob,
        actions: CiActions,
        t: &Theme,
        cx: &App,
    ) -> AnyElement {
        let c = &t.colors;
        let id = SharedString::from(format!("job-{}", job.id));
        let has_steps = !job.steps.is_empty();
        let expanded = open.ci.expanded.contains(&job.id);
        let shown = open.ci.log.as_ref().is_some_and(|log| log.job.id == job.id);
        let job_id = job.id.clone();
        let chevron = div().size(px(14.0)).flex_none().when(has_steps, |el| {
            el.child(
                Icon::new(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .size(px(14.0))
                .text_color(c.text_muted),
            )
        });
        let attempt = job
            .attempt
            .filter(|n| *n > 1)
            .map(|n| ui::chip(cx, format!("attempt {n}"), c.info));
        let started = job.started_at.is_some();
        let log = if !started {
            None
        } else if actions.logs {
            let job_id = job.id.clone();
            Some(
                Button::new(
                    SharedString::from(format!("log-{}", job.id)),
                    "View log",
                    ButtonKind::Ghost,
                )
                .on_click(self.listener(move |session, cx| {
                    session.open_log(&job_id, None, cx);
                }))
                .into_any_element(),
            )
        } else {
            web_link(
                t,
                SharedString::from(format!("log-web-{}", job.id)),
                "Log on the web",
                job.url.as_deref(),
            )
        };
        let retry = (job.state == CiState::Fail)
            .then(|| {
                if actions.retry {
                    let retry = Retry::Job {
                        run: run.id.clone(),
                        job: job.id.clone(),
                    };
                    Some(
                        Button::new(
                            SharedString::from(format!("retry-{}", job.id)),
                            "Retry job",
                            ButtonKind::Secondary,
                        )
                        .on_click(self.listener(move |session, cx| {
                            session.request_retry(retry.clone(), cx);
                        }))
                        .into_any_element(),
                    )
                } else {
                    web_link(
                        t,
                        SharedString::from(format!("retry-web-{}", job.id)),
                        "Retry on the web",
                        job.url.as_deref().or(run.url.as_deref()),
                    )
                }
            })
            .flatten();
        let row = h_flex()
            .debug_selector(|| format!("job-{}", job.id))
            .id(id)
            .w_full()
            .h(px(JOB_ROW))
            .flex_none()
            .pl(px(10.0 + INDENT))
            .pr(px(10.0))
            .gap(px(8.0))
            .items_center()
            .when(shown, |el| el.bg(c.selected));
        fold::toggle_row(
            row,
            t,
            self.listener(move |session, cx| {
                if has_steps {
                    session.toggle_job_steps(&job_id, cx);
                } else {
                    session.open_log(&job_id, None, cx);
                }
            }),
        )
        .child(chevron)
        .child(ci_icon(t, job.state, CI_ROW, &self.spinners))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.text)
                .child(job.name.clone()),
        )
        .children(attempt)
        .child(
            div()
                .flex_none()
                .w(px(70.0))
                .flex()
                .justify_end()
                .mono(t)
                .tiny(t)
                .text_color(ci_color(t, job.state))
                .child(
                    ci::duration(job.started_at, job.completed_at)
                        .unwrap_or_else(|| ui::ci_label(job.state).to_owned()),
                ),
        )
        .children(retry)
        .children(log)
        .context_menu({
            let menu = JobMenu::new(open.key.clone(), Some(job.id.clone()));
            move |popup, _, cx| menu.popup(popup, cx)
        })
        .into_any_element()
    }

    fn step_row(&self, job: &CiJob, step: &CheckStep, actions: CiActions, t: &Theme) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let (job_id, number): (JobId, u32) = (job.id.clone(), step.number);
        h_flex()
            .id(SharedString::from(format!(
                "step-{}-{}",
                job.id, step.number
            )))
            .h(px(24.0))
            .flex_none()
            .pl(px(10.0 + INDENT * 2.0 + 14.0))
            .pr(px(10.0))
            .gap(px(8.0))
            .items_center()
            .small(t)
            .hover(move |s| s.bg(hover))
            .when(actions.logs, |el| {
                el.cursor_pointer()
                    .on_click(self.listener(move |session, cx| {
                        session.open_log(&job_id, Some(number), cx);
                    }))
            })
            .child(ci_icon(t, step.state, 12.0, &self.spinners))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(c.text_secondary)
                    .child(format!("{}. {}", step.number, step.name)),
            )
            .children(ci::duration(step.started_at, step.completed_at).map(|d| {
                div()
                    .flex_none()
                    .mono(t)
                    .tiny(t)
                    .text_color(c.text_muted)
                    .child(d)
            }))
            .into_any_element()
    }

    /// The rollup as a flat list, where the forge has no run list.
    fn check_rows(&self, checks: &[Check], t: &Theme) -> Vec<AnyElement> {
        let c = &t.colors;
        let hover = c.hover;
        checks
            .iter()
            .enumerate()
            .map(|(ix, check)| {
                h_flex()
                    .id(("check", ix))
                    .h(px(JOB_ROW))
                    .px(px(12.0))
                    .gap(px(10.0))
                    .items_center()
                    .hover(move |s| s.bg(hover))
                    .child(ci_icon(t, check.state, CI_ROW, &self.spinners))
                    .child(
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(c.text)
                            .child(check.name.clone()),
                    )
                    .child(
                        div()
                            .small(t)
                            .text_color(ci_color(t, check.state))
                            .child(check.detail.clone()),
                    )
                    .into_any_element()
            })
            .collect()
    }

    fn body(&self, open: &OpenPr, t: &Theme, cx: &App) -> AnyElement {
        let caps = self
            .session
            .read(cx)
            .capabilities_for(&open.key)
            .unwrap_or_else(|| forge::Capabilities::baseline(open.pr.forge));
        let actions = CiActions::of(&caps);
        let note = |text: &str| {
            div()
                .px(px(12.0))
                .py(px(8.0))
                .small(t)
                .text_color(t.colors.text_muted)
                .child(text.to_owned())
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match &open.ci.load {
            CiLoad::Failed(error) => rows.push(note(&format!("Runs not read: {error}"))),
            CiLoad::Unsupported => {
                rows.push(note(
                    "This forge lists no runs; the checks are all there is",
                ));
            }
            CiLoad::Idle | CiLoad::Loading if open.ci.runs.is_empty() => {
                rows.push(note("Loading runs"));
            }
            _ => {}
        }
        if open.ci.runs.is_empty() {
            if open.checks.is_empty() {
                if open.ci.load == CiLoad::Ready {
                    return ui::empty_note(cx, "No CI runs for this commit").into_any_element();
                }
            } else {
                rows.extend(self.check_rows(&open.checks, t));
            }
        }
        for run in &open.ci.runs {
            rows.extend(self.run_rows(open, run, actions, t, cx));
        }
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id("checks-scroll")
                    .debug_selector(|| "checks-scroll".into())
                    .size_full()
                    .track_scroll(&self.scroll)
                    .overflow_y_scroll()
                    .child(v_flex().w_full().children(rows)),
            )
            .child(self.autoscroll.overlay(ui::scroll_style(t).autoscroll))
            .into_any_element()
    }
}

/// A stable small number for element ids of a run, such as its
/// `("retry-run", ix)` button.
pub fn ix_of(run: &CiRun) -> usize {
    run.id.as_str().bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    })
}

impl Render for ChecksPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.spinners.clear();
        let t = ui::theme(cx);
        let c = &t.colors;
        if self.spinners.is_shown()
            && let Some(subscriptions) = Subscriptions::global(cx)
            && subscriptions.read(cx).checks_badge()
        {
            cx.defer(move |cx| subscriptions.update(cx, |s, cx| s.clear_checks_badge(cx)));
        }
        let session = self.session.read(cx);
        let Some(open) = self.binding.open(session) else {
            return v_flex()
                .size_full()
                .bg(c.surface)
                .child(ui::empty_note(cx, "No pull request open"));
        };
        let runs = open.ci.runs.len();
        let lead = if runs == 1 {
            "1 run".to_owned()
        } else {
            format!("{runs} runs")
        };
        let summary = Self::summary(open);
        let watch = self.watch_button(open, cx);
        let notice = open.ci.notice.as_ref().map(|n| self.notice(n, &t, cx));
        let confirm = open.ci.confirm.as_ref().map(|n| self.confirm(n, &t, cx));
        let body = self.body(open, &t, cx);
        v_flex()
            .size_full()
            .bg(c.surface)
            .child(header(
                cx,
                h_flex()
                    .gap(px(10.0))
                    .items_center()
                    .child(div().text_color(c.text).child(lead))
                    .child(div().child(summary)),
                h_flex().gap(px(6.0)).items_center().children(watch),
            ))
            .children(notice)
            .children(confirm)
            .child(body)
    }
}
