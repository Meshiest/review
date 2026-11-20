//! The pipelines card at the top of the Conversation's merge box: how the
//! checks stand in one header row, and, once that row is clicked (or Enter
//! or Space pressed on it), each run with its jobs. A run folds to its
//! header; a job shows its state, duration and failing step, opens its log
//! in the Log tab, unfolds its steps and, when it failed, the tail of its
//! log. Re-runs ask first when they would start more than one job; what the
//! forge cannot do here is a link to its web page.
//!
//! Whether the card is open is the session's ([`crate::session::CiPanel`]),
//! so a toast or the inbox's CI icon can open it. A log tail is read only
//! when its job is first unfolded, off the main thread, and only its last
//! lines are kept, in [`LogTails`].

use std::collections::HashMap;
use std::time::Duration;

use forge::model::{CheckStep, CiJob, CiRun, CiState, JobId, RepoRef};
use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, FontWeight, Global,
    InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Task, Window, div, prelude::FluentBuilder as _,
    px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, h_flex, v_flex};
use log_view::{LineKind, LogFeed, LogStyle, LogView};
use theme::Theme;

use super::job_menu::JobMenu;
use crate::ci::{self, CiActions, CiLoad, Retry, RetryConfirm};
use crate::model::{Check, PrKey};
use crate::session::{CiNotice, OpenPr, Session};
use crate::subscriptions::Subscriptions;
use crate::threads;
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{
    self, Button, ButtonKind, CI_HEADER, CI_ROW, TextExt as _, ci_color, ci_icon, ci_label, fold,
};
use crate::untrusted::is_web_url;

/// How many of a log's last lines a tail keeps.
pub const TAIL_LINES: usize = 30;
/// How many lines of the tail show at once.
const VIEW_LINES: f32 = 15.0;
/// The most reads of one log; a forge that never says "complete" stops here.
const MAX_READS: usize = 256;
/// The longest a tail waits for its lines to be parsed.
const SETTLE_WAIT: Duration = Duration::from_secs(10);
/// A job's indent under its run, and a step's under its job.
const INDENT: f32 = 22.0;
const RUN_ROW: f32 = 30.0;
const JOB_ROW: f32 = 26.0;

/// Where a job's log tail stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TailStatus {
    Loading,
    Ready,
    /// The forge has no log API: its web page has the log.
    Unavailable,
    Failed(SharedString),
}

struct Tail {
    status: TailStatus,
    view: Option<Entity<LogView>>,
    _task: Task<()>,
}

/// The log tails of failed jobs unfolded this session, per pull request.
#[derive(Default)]
pub struct LogTails {
    by_pr: HashMap<PrKey, HashMap<JobId, Tail>>,
}

struct Shared(Entity<LogTails>);

impl Global for Shared {}

impl LogTails {
    /// The one shared state; made on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(shared) = cx.try_global::<Shared>() {
            return shared.0.clone();
        }
        let tails = cx.new(|_| Self::default());
        cx.set_global(Shared(tails.clone()));
        tails
    }

    #[must_use]
    pub fn status(&self, key: &PrKey, job: &JobId) -> Option<TailStatus> {
        Some(self.by_pr.get(key)?.get(job)?.status.clone())
    }

    /// How many log tails are read and drawn, for tests.
    #[must_use]
    pub fn ready(&self) -> usize {
        self.by_pr
            .values()
            .flat_map(HashMap::values)
            .filter(|tail| tail.status == TailStatus::Ready)
            .count()
    }

    fn view(&self, key: &PrKey, job: &JobId) -> Option<Entity<LogView>> {
        self.by_pr.get(key)?.get(job)?.view.clone()
    }

    /// Starts reading `job`'s log tail, unless it was read already.
    pub fn ensure(
        &mut self,
        key: &PrKey,
        job: &CiJob,
        session: &Entity<Session>,
        cx: &mut Context<Self>,
    ) {
        if self
            .by_pr
            .get(key)
            .is_some_and(|tails| tails.contains_key(&job.id))
        {
            return;
        }
        let task = Self::read(key.clone(), job.clone(), session, cx);
        self.by_pr.entry(key.clone()).or_default().insert(
            job.id.clone(),
            Tail {
                status: TailStatus::Loading,
                view: None,
                _task: task,
            },
        );
        cx.notify();
    }

    fn read(key: PrKey, job: CiJob, session: &Entity<Session>, cx: &mut Context<Self>) -> Task<()> {
        let (fixture, services) = {
            let session = session.read(cx);
            (session.fixture_log(&job), session.services().cloned())
        };
        let repo: RepoRef = key.repo.clone();
        cx.spawn(async move |this, cx| {
            let bytes = match fixture {
                Some(bytes) => Ok(bytes),
                None => match services {
                    Some(services) => read_log(&services, &repo, &job.id).await,
                    None => Err(TailStatus::Unavailable),
                },
            };
            let bytes = match bytes {
                Ok(bytes) => bytes,
                Err(status) => {
                    this.update(cx, |this, cx| this.set_status(&key, &job.id, status, cx))
                        .ok();
                    return;
                }
            };
            let tail = tail_of(&bytes, TAIL_LINES).to_vec();
            let feed = LogFeed::new();
            feed.push(tail);
            feed.finish();
            // The parser runs on a thread of its own; wait for it off the main one.
            let feed = cx
                .background_executor()
                .spawn(async move {
                    feed.wait_settled(SETTLE_WAIT);
                    feed
                })
                .await;
            this.update(cx, |this, cx| this.loaded(&key, &job.id, &feed, cx))
                .ok();
        })
    }

    fn set_status(&mut self, key: &PrKey, job: &JobId, status: TailStatus, cx: &mut Context<Self>) {
        if let Some(tail) = self.by_pr.get_mut(key).and_then(|tails| tails.get_mut(job)) {
            tail.status = status;
            cx.notify();
        }
    }

    /// The tail's lines are parsed: a view over them, at the first error.
    fn loaded(&mut self, key: &PrKey, job: &JobId, feed: &LogFeed, cx: &mut Context<Self>) {
        let style = LogStyle::from_theme(&ui::theme(cx));
        let line_height = f64::from(style.line_height);
        let view = cx.new(|cx| LogView::new(feed, style, cx));
        let first_error = feed.read(|buffer| {
            buffer
                .lines(buffer.dropped()..buffer.total())
                .find(|line| line.kind == LineKind::Error)
                .map(|line| line.id - buffer.dropped())
        });
        if let Some(row) = first_error {
            view.update(cx, |view, cx| {
                view.set_scroll_y((row as f64 * line_height - line_height * 2.0).max(0.0));
                cx.notify();
            });
        }
        if let Some(tail) = self.by_pr.get_mut(key).and_then(|tails| tails.get_mut(job)) {
            tail.view = Some(view);
            tail.status = TailStatus::Ready;
            cx.notify();
        }
    }
}

/// The whole log of a finished job, read from the forge.
async fn read_log(
    services: &crate::services::Services,
    repo: &RepoRef,
    job: &JobId,
) -> Result<Vec<u8>, TailStatus> {
    let mut all = Vec::new();
    let mut offset = 0u64;
    for _ in 0..MAX_READS {
        let (repo, job) = (repo.clone(), job.clone());
        let read = services
            .forge(&repo.host.clone(), move |forge| async move {
                forge.job_log(&repo, &job, offset).await
            })
            .await;
        let chunk = match read {
            Ok(chunk) => chunk,
            Err(forge::Error::Unsupported { .. }) => return Err(TailStatus::Unavailable),
            Err(error) => return Err(TailStatus::Failed(error.to_string().into())),
        };
        if chunk.offset == 0 {
            all.clear();
        }
        let empty = chunk.bytes.is_empty();
        all.extend_from_slice(&chunk.bytes);
        offset = chunk.next_offset;
        if chunk.complete || empty {
            break;
        }
    }
    Ok(all)
}

/// The last `lines` lines of `bytes`.
#[must_use]
pub fn tail_of(bytes: &[u8], lines: usize) -> &[u8] {
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let mut seen = 0;
    for (at, byte) in body.iter().enumerate().rev() {
        if *byte == b'\n' {
            seen += 1;
            if seen == lines {
                return &bytes[at + 1..];
            }
        }
    }
    bytes
}

/// A stable small number for element ids of a run, such as its
/// `("ci-retry-run", ix)` button.
#[must_use]
pub fn ix_of(run: &CiRun) -> usize {
    run.id.as_str().bytes().fold(0usize, |h, b| {
        h.wrapping_mul(31).wrapping_add(usize::from(b))
    })
}

/// The state of every job, or of every rollup check where the forge lists
/// no runs.
pub(super) fn job_states(open: &OpenPr) -> Vec<CiState> {
    if open.ci.runs.is_empty() {
        return open.checks.iter().map(|check| check.state).collect();
    }
    open.ci
        .runs
        .iter()
        .flat_map(|run| run.jobs.iter().map(|job| job.state))
        .collect()
}

/// The overall state, a headline and the counts by state.
pub(super) fn summary(states: &[CiState], rollup: CiState) -> (CiState, &'static str, String) {
    let count = |state: CiState| states.iter().filter(|s| **s == state).count();
    let failing = count(CiState::Fail);
    let active = states.iter().filter(|s| s.is_active()).count();
    let passed = count(CiState::Pass);
    let skipped = count(CiState::Skipped) + count(CiState::Canceled);
    let counts = [
        (failing, "failing"),
        (active, "in progress"),
        (passed, "successful"),
        (skipped, "skipped"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect::<Vec<_>>()
    .join(", ");
    let (state, headline) = if states.is_empty() {
        let headline = match rollup {
            CiState::None => "No checks",
            CiState::Fail => "Some checks were not successful",
            CiState::Running | CiState::Queued => "Some checks have not finished",
            CiState::Pass => "All checks have passed",
            CiState::Skipped | CiState::Canceled => "The checks did not run",
        };
        (rollup, headline)
    } else if failing > 0 {
        (CiState::Fail, "Some checks were not successful")
    } else if active > 0 {
        (CiState::Running, "Some checks have not finished")
    } else if passed > 0 {
        (CiState::Pass, "All checks have passed")
    } else {
        (CiState::Skipped, "The checks did not run")
    };
    (state, headline, counts)
}

/// What a job's row shows on its right: how long it took, or its state while
/// it has not finished.
fn job_detail(job: &CiJob) -> String {
    if job.state.is_active() {
        return ci_label(job.state).to_owned();
    }
    ci::duration(job.started_at, job.completed_at).unwrap_or_else(|| ci_label(job.state).to_owned())
}

/// The same for a run; nothing before it starts.
fn run_detail(run: &CiRun) -> Option<String> {
    if run.state.is_active() {
        return Some(ci_label(run.state).to_owned());
    }
    ci::duration(run.started_at, run.completed_at)
}

/// A text link to a forge page, only for web URLs. The row under it does not
/// take the click.
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

/// The card's content for one render.
pub(super) struct Pipelines<'a> {
    pub open: &'a OpenPr,
    pub session: &'a Entity<Session>,
    pub tails: &'a Entity<LogTails>,
    pub spinners: &'a SpinnerSlots,
}

impl Pipelines<'_> {
    /// A click handler that changes the session on this pull request, which
    /// takes the focus first. It stops the click, so the rows it sits in do
    /// not also act.
    fn on_session<E>(
        &self,
        f: impl Fn(&mut Session, &mut Context<Session>) + 'static,
    ) -> impl Fn(&E, &mut Window, &mut App) + 'static {
        let (session, key) = (self.session.clone(), self.open.key.clone());
        move |_, _, cx| {
            cx.stop_propagation();
            session.update(cx, |session, cx| {
                session.focus(&key, cx);
                f(session, cx);
            });
        }
    }

    fn actions(&self, cx: &App) -> CiActions {
        let caps = self
            .session
            .read(cx)
            .capabilities_for(&self.open.key)
            .unwrap_or_else(|| forge::Capabilities::baseline(self.open.pr.forge));
        CiActions::of(&caps)
    }

    /// The header row, the notice and confirmation under it, and the runs
    /// while it is open.
    pub fn render(&self, t: &Theme, cx: &App) -> AnyElement {
        let open = self.open;
        let listed = open.ci.listed;
        let notice = open.ci.notice.as_ref().map(|n| self.notice(n, t, cx));
        let confirm = open.ci.confirm.as_ref().map(|c| self.confirm(c, t, cx));
        let list = listed.then(|| self.list(t, cx));
        v_flex()
            .debug_selector(|| "merge-box-checks-summary".into())
            .px(px(12.0))
            .py(px(10.0))
            .gap(px(8.0))
            .child(self.header(t, cx))
            .children(notice)
            .children(confirm)
            .children(list)
            .into_any_element()
    }

    /// The whole row is the fold toggle; the chevron sits in a box of its
    /// own size, so it stays put when it turns.
    fn header(&self, t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let open = self.open;
        let listed = open.ci.listed;
        let (state, headline, counts) = summary(&job_states(open), open.pr.ci);
        let watch = Subscriptions::global(cx).map(|subscriptions| {
            let watched = subscriptions.read(cx).is_watched(&open.key, cx);
            let key = open.key.clone();
            let icon = if watched {
                IconName::Bell
            } else {
                IconName::BellOff
            };
            ui::icon_button(cx, "watch-pr", icon, move |_, _, cx| {
                cx.stop_propagation();
                subscriptions.update(cx, |s, cx| s.toggle(&key, cx));
            })
            .debug_selector(|| "watch-pr".into())
        });
        let row = h_flex()
            .id("merge-box-checks")
            .debug_selector(|| "merge-box-checks".into())
            .gap(px(10.0))
            .px(px(6.0))
            .mx(px(-6.0))
            .py(px(4.0))
            .my(px(-4.0))
            .items_center()
            .rounded(t.radius.sm);
        let (session, key) = (self.session.clone(), open.key.clone());
        fold::toggle_row(row, t, move |_, _, cx| {
            session.update(cx, |session, cx| session.toggle_pipelines(&key, cx));
        })
        .child(
            threads::chevron(!open.ci.listed, t).debug_selector(move || {
                if listed {
                    "merge-box-checks-chevron-open".into()
                } else {
                    "merge-box-checks-chevron-folded".into()
                }
            }),
        )
        .child(ci_icon(t, state, CI_HEADER, self.spinners))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text)
                        .child(headline),
                )
                .when(!counts.is_empty(), |el| {
                    el.child(div().text_color(c.text_muted).child(counts))
                }),
        )
        .children(watch)
        .into_any_element()
    }

    fn notice(&self, notice: &CiNotice, t: &Theme, cx: &App) -> AnyElement {
        let color = if notice.error {
            t.colors.danger
        } else {
            t.colors.text_secondary
        };
        h_flex()
            .debug_selector(|| "ci-notice".into())
            .pl(px(26.0))
            .gap(px(8.0))
            .items_center()
            .child(div().flex_1().text_color(color).child(notice.text.clone()))
            .child(ui::icon_button(
                cx,
                "dismiss-ci-notice",
                IconName::Close,
                self.on_session(|session, cx| session.dismiss_ci_notice(cx)),
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
                .child(ci_icon(t, CiState::Fail, CI_ROW, self.spinners))
                .child(div().mono(t).text_color(c.text).child(name.clone()))
        });
        ui::card(cx)
            .debug_selector(|| "ci-retry-confirm".into())
            .ml(px(26.0))
            .p(px(10.0))
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
                    .text_color(c.text_secondary)
                    .child("Each job starts again on the forge and uses runner time."),
            )
            .child(v_flex().gap(px(4.0)).py(px(2.0)).children(jobs))
            .child(
                h_flex()
                    .gap(px(8.0))
                    .child(
                        Button::new("confirm-retry", "Re-run jobs", ButtonKind::Primary)
                            .on_click(self.on_session(|session, cx| session.confirm_retry(cx))),
                    )
                    .child(
                        Button::new("cancel-retry", "Cancel", ButtonKind::Ghost)
                            .on_click(self.on_session(|session, cx| session.cancel_retry(cx))),
                    ),
            )
            .into_any_element()
    }

    /// Every run with its jobs, or the rollup where the forge lists no runs,
    /// with a line on how reading them went.
    fn list(&self, t: &Theme, cx: &App) -> AnyElement {
        let open = self.open;
        let actions = self.actions(cx);
        let note = |text: String| {
            div()
                .text_color(t.colors.text_muted)
                .child(text)
                .into_any_element()
        };
        let mut rows: Vec<AnyElement> = Vec::new();
        match &open.ci.load {
            CiLoad::Failed(error) => rows.push(note(format!("Runs not read: {error}"))),
            CiLoad::Unsupported => {
                rows.push(note(
                    "This forge lists no runs; the checks are all there is".to_owned(),
                ));
            }
            CiLoad::Idle | CiLoad::Loading if open.ci.runs.is_empty() => {
                rows.push(note("Loading runs".to_owned()));
            }
            _ => {}
        }
        if open.ci.runs.is_empty() {
            if open.checks.is_empty() && open.ci.load == CiLoad::Ready {
                rows.push(note("No CI runs for this commit".to_owned()));
            }
            rows.extend(
                open.checks
                    .iter()
                    .enumerate()
                    .map(|(ix, check)| self.check_row(ix, check, t)),
            );
        }
        for run in &open.ci.runs {
            rows.extend(self.run_rows(run, actions, t, cx));
        }
        v_flex()
            .debug_selector(|| "merge-box-jobs".into())
            .pl(px(26.0))
            .children(rows)
            .into_any_element()
    }

    /// A rollup check with no job behind it.
    fn check_row(&self, ix: usize, check: &Check, t: &Theme) -> AnyElement {
        let c = &t.colors;
        h_flex()
            .debug_selector(move || format!("ci-check-{ix}"))
            .min_h(px(JOB_ROW))
            .gap(px(8.0))
            .items_center()
            .child(ci_icon(t, check.state, CI_ROW, self.spinners))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(c.text)
                    .child(check.name.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .mono(t)
                    .tiny(t)
                    .text_color(ci_color(t, check.state))
                    .child(check.detail.clone()),
            )
            .into_any_element()
    }

    fn run_rows(&self, run: &CiRun, actions: CiActions, t: &Theme, cx: &App) -> Vec<AnyElement> {
        let c = &t.colors;
        let open = self.open;
        let ix = ix_of(run);
        let folded = open.ci.folded.contains(&run.id);
        let failed = ci::failed_jobs(run).count();
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
                Button::new(("ci-retry-run", ix), label, ButtonKind::Secondary)
                    .on_click(self.on_session(move |session, cx| {
                        session.request_retry(retry.clone(), cx);
                    }))
                    .into_any_element(),
            )
        } else {
            web_link(
                t,
                ("ci-retry-run-web", ix),
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
            ui::icon_button(cx, ("ci-watch-run", ix), icon, move |_, _, cx| {
                cx.stop_propagation();
                subscriptions.update(cx, |s, cx| s.toggle_run(&key, &run_id, cx));
            })
        });
        let run_id = run.id.clone();
        let row = h_flex()
            .id(("ci-run", ix))
            .debug_selector(|| format!("ci-run-{}", run.id))
            .min_h(px(RUN_ROW))
            .gap(px(8.0))
            .px(px(4.0))
            .mx(px(-4.0))
            .items_center()
            .rounded(t.radius.sm);
        let header = fold::toggle_row(
            row,
            t,
            self.on_session(move |session, cx| session.toggle_run_folded(&run_id, cx)),
        )
        .child(threads::chevron(folded, t))
        .child(ci_icon(t, run.state, CI_ROW, self.spinners))
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
                .tiny(t)
                .text_color(c.text_muted)
                .child(context.join(" · ")),
        )
        .children(run_detail(run).map(|detail| {
            div()
                .flex_none()
                .mono(t)
                .tiny(t)
                .text_color(ci_color(t, run.state))
                .child(detail)
        }))
        .children(retry)
        .children(watch)
        .children(web_link(t, ("ci-run-web", ix), "Web", run.url.as_deref()))
        .into_any_element();
        let mut rows = vec![header];
        if folded {
            return rows;
        }
        for (stage, jobs) in ci::stages(run) {
            if let Some(stage) = stage {
                rows.push(
                    div()
                        .h(px(20.0))
                        .pl(px(INDENT))
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
                rows.push(self.job_block(run, job, actions, t, cx));
            }
        }
        rows
    }

    /// A job's row, and once unfolded its steps and, when it failed, its
    /// log tail.
    fn job_block(
        &self,
        run: &CiRun,
        job: &CiJob,
        actions: CiActions,
        t: &Theme,
        cx: &App,
    ) -> AnyElement {
        let c = &t.colors;
        let open = self.open;
        let failed = job.state == CiState::Fail;
        let foldable = !job.steps.is_empty() || failed;
        let expanded = foldable && open.ci.expanded.contains(&job.id);
        let shown = open.ci.log.as_ref().is_some_and(|log| log.job.id == job.id);
        let loggable = actions.logs && job.started_at.is_some();
        let tag = job.id.to_string();

        let toggle = {
            let (session, tails, key, job) = (
                self.session.clone(),
                self.tails.clone(),
                open.key.clone(),
                job.clone(),
            );
            move |_: &ClickEvent, _: &mut Window, cx: &mut App| {
                cx.stop_propagation();
                let unfolded = session.update(cx, |session, cx| {
                    session.focus(&key, cx);
                    session.toggle_job_steps(&job.id, cx);
                    session
                        .pr(&key)
                        .is_some_and(|open| open.ci.expanded.contains(&job.id))
                });
                if unfolded && job.state == CiState::Fail {
                    tails.update(cx, |tails, cx| tails.ensure(&key, &job, &session, cx));
                }
            }
        };
        let chevron = div()
            .id(SharedString::from(format!("ci-job-fold-{tag}")))
            .debug_selector({
                let tag = tag.clone();
                move || format!("ci-job-fold-{tag}")
            })
            .flex_none()
            .rounded(t.radius.sm)
            .when(foldable, |el| {
                let hover = c.hover;
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(toggle.clone())
                    .child(threads::chevron(!expanded, t))
            })
            .when(!foldable, |el| el.size(px(CI_HEADER)));
        let failing_step = job
            .steps
            .iter()
            .find(|step| step.state == CiState::Fail)
            .map(|step| {
                div()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .truncate()
                    .tiny(t)
                    .text_color(c.danger)
                    .child(format!("failed at {}", step.name))
            });
        let attempt = job
            .attempt
            .filter(|n| *n > 1)
            .map(|n| ui::chip(cx, format!("attempt {n}"), c.info));
        let retry = failed
            .then(|| {
                if actions.retry {
                    let retry = Retry::Job {
                        run: run.id.clone(),
                        job: job.id.clone(),
                    };
                    Some(
                        Button::new(
                            SharedString::from(format!("ci-retry-{tag}")),
                            "Retry job",
                            ButtonKind::Secondary,
                        )
                        .on_click(self.on_session(move |session, cx| {
                            session.request_retry(retry.clone(), cx);
                        }))
                        .into_any_element(),
                    )
                } else {
                    web_link(
                        t,
                        SharedString::from(format!("ci-retry-web-{tag}")),
                        "Retry on the web",
                        job.url.as_deref().or(run.url.as_deref()),
                    )
                }
            })
            .flatten();
        let web_log = (!actions.logs && job.started_at.is_some())
            .then(|| {
                web_link(
                    t,
                    SharedString::from(format!("ci-log-web-{tag}")),
                    "Log on the web",
                    job.url.as_deref(),
                )
            })
            .flatten();
        let hover = c.hover;
        let row = h_flex()
            .id(SharedString::from(format!("ci-job-{tag}")))
            .w_full()
            .debug_selector({
                let tag = tag.clone();
                move || format!("ci-job-{tag}")
            })
            .min_h(px(JOB_ROW))
            .pl(px(INDENT))
            .pr(px(4.0))
            .gap(px(8.0))
            .items_center()
            .rounded(t.radius.sm)
            .when(shown, |el| el.bg(c.selected))
            .when(loggable, |el| {
                let job_id = job.id.clone();
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(self.on_session(move |session, cx| {
                        session.open_log(&job_id, None, cx);
                    }))
            })
            .when(!loggable && foldable, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(toggle.clone())
            })
            .child(chevron)
            .child(ci_icon(t, job.state, CI_ROW, self.spinners))
            .child(
                div()
                    .flex_shrink(1.0)
                    .min_w_0()
                    .truncate()
                    .text_color(c.text)
                    .child(job.name.clone()),
            )
            .children(failing_step)
            .child(div().flex_1())
            .children(attempt)
            .child(
                div()
                    .flex_none()
                    .mono(t)
                    .tiny(t)
                    .text_color(ci_color(t, job.state))
                    .child(job_detail(job)),
            )
            .children(retry)
            .children(web_log)
            .context_menu({
                let menu = JobMenu::new(open.key.clone(), Some(job.id.clone()));
                move |popup, _, cx| menu.popup(popup, cx)
            });
        let mut block = v_flex().w_full().child(row);
        if expanded {
            block = block.children(
                job.steps
                    .iter()
                    .map(|step| self.step_row(job, step, actions, t)),
            );
            if failed {
                block = block.child(self.tail_block(job, actions, t, cx));
            }
        }
        block.into_any_element()
    }

    fn step_row(&self, job: &CiJob, step: &CheckStep, actions: CiActions, t: &Theme) -> AnyElement {
        let c = &t.colors;
        let hover = c.hover;
        let (job_id, number) = (job.id.clone(), step.number);
        h_flex()
            .id(SharedString::from(format!(
                "ci-step-{}-{}",
                job.id, step.number
            )))
            .min_h(px(22.0))
            .pl(px(INDENT * 2.0))
            .gap(px(8.0))
            .items_center()
            .small(t)
            .rounded(t.radius.sm)
            .when(actions.logs, |el| {
                el.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(self.on_session(move |session, cx| {
                        session.open_log(&job_id, Some(number), cx);
                    }))
            })
            .child(ci_icon(t, step.state, 12.0, self.spinners))
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

    /// The log tail under a failed job, and the way to the whole log. It sits
    /// above the card in the hit order, so selecting text in it does not
    /// open the log.
    fn tail_block(&self, job: &CiJob, actions: CiActions, t: &Theme, cx: &App) -> AnyElement {
        let c = &t.colors;
        let key = &self.open.key;
        let tag = job.id.to_string();
        let tails = self.tails.read(cx);
        let status = tails.status(key, &job.id).unwrap_or(TailStatus::Loading);
        let note = |text: String| {
            div()
                .text_color(c.text_muted)
                .child(text)
                .into_any_element()
        };
        let body = match &status {
            TailStatus::Ready => tails.view(key, &job.id).map(|view| {
                let height = view.read(cx).style().line_height * VIEW_LINES;
                let tag = tag.clone();
                div()
                    .debug_selector(move || format!("ci-tail-{tag}"))
                    .h(height)
                    .rounded(t.radius.sm)
                    .overflow_hidden()
                    .child(view)
                    .into_any_element()
            }),
            TailStatus::Loading => Some(note("Reading the log".to_owned())),
            TailStatus::Unavailable => Some(note(
                "This forge has no log API; open the job on the web".to_owned(),
            )),
            TailStatus::Failed(error) => Some(note(format!("Log not read: {error}"))),
        };
        let full = actions.logs.then(|| {
            let job_id = job.id.clone();
            div()
                .id(SharedString::from(format!("ci-full-log-{tag}")))
                .debug_selector({
                    let tag = tag.clone();
                    move || format!("ci-full-log-{tag}")
                })
                .cursor_pointer()
                .text_color(c.accent_fg)
                .hover(|s| s.underline())
                .on_click(self.on_session(move |session, cx| {
                    session.open_log(&job_id, None, cx);
                }))
                .child("Open full log")
                .into_any_element()
        });
        v_flex()
            .occlude()
            .gap(px(4.0))
            .pl(px(INDENT * 2.0))
            .py(px(4.0))
            .children(body)
            .children(full)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tail_is_the_last_lines() {
        let log = b"a\nb\nc\nd\n";
        assert_eq!(tail_of(log, 2), b"c\nd\n");
        assert_eq!(tail_of(log, 4), log);
        assert_eq!(tail_of(log, 9), log);
        assert_eq!(tail_of(b"a\nb", 1), b"b");
        assert_eq!(tail_of(b"", 3), b"");
    }

    #[test]
    fn the_summary_names_the_worst_state_and_counts_each() {
        let states = [CiState::Fail, CiState::Pass, CiState::Pass];
        let (state, headline, counts) = summary(&states, CiState::Fail);
        assert_eq!(state, CiState::Fail);
        assert_eq!(headline, "Some checks were not successful");
        assert_eq!(counts, "1 failing, 2 successful");
        let states = [CiState::Running, CiState::Pass];
        assert_eq!(summary(&states, CiState::Running).0, CiState::Running);
        assert_eq!(
            summary(&[], CiState::None),
            (CiState::None, "No checks", String::new())
        );
    }
}
