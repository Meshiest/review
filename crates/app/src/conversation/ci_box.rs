//! The Conversation's CI box: counts by state and the failing jobs, and,
//! when the box is clicked, every job (failing first, then running, then
//! the rest) with its steps. A failing job unfolds the tail of its log
//! under it; "Open checks" is the only way here to the Checks tab and
//! "Open full log" the only way to the Log panel.
//!
//! What is open is kept per pull request for the session, in [`CiBoxes`].
//! A log is read only when its job is first opened, off the main thread,
//! and only its last lines are kept.

use std::collections::{HashMap, HashSet};

use forge::model::{CheckStep, CiJob, CiState, JobId, RepoRef};
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global, InteractiveElement as _,
    IntoElement, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _,
    Task, div, prelude::FluentBuilder as _, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::{Icon, h_flex, v_flex};
use log_view::{LineKind, LogFeed, LogStyle, LogView};
use std::time::Duration;

use crate::checks_panel::job_menu::JobMenu;
use crate::ci;
use crate::model::PrKey;
use crate::panels::PanelKind;
use crate::session::{OpenMode, OpenPr, Session};
use crate::ui::spinner::SpinnerSlots;
use crate::ui::{
    self, Button, ButtonKind, CI_ROW, TextExt as _, ci_color, ci_icon, ci_label, fold,
};

/// How many of a log's last lines a tail keeps.
pub const TAIL_LINES: usize = 30;
/// How many lines of the tail show at once.
const VIEW_LINES: f32 = 15.0;
/// The most reads of one log; a forge that never says "complete" stops here.
const MAX_READS: usize = 256;
/// The longest a tail waits for its lines to be parsed.
const SETTLE_WAIT: Duration = Duration::from_secs(10);

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

#[derive(Default)]
struct PrBox {
    expanded: bool,
    open: HashSet<JobId>,
    tails: HashMap<JobId, Tail>,
}

/// The CI boxes of every pull request opened this session.
#[derive(Default)]
pub struct CiBoxes {
    by_pr: HashMap<PrKey, PrBox>,
}

struct Shared(Entity<CiBoxes>);

impl Global for Shared {}

impl CiBoxes {
    /// The one shared state; made on first use.
    pub fn global(cx: &mut App) -> Entity<Self> {
        if let Some(shared) = cx.try_global::<Shared>() {
            return shared.0.clone();
        }
        let boxes = cx.new(|_| Self::default());
        cx.set_global(Shared(boxes.clone()));
        boxes
    }

    #[must_use]
    pub fn is_expanded(&self, key: &PrKey) -> bool {
        self.by_pr.get(key).is_some_and(|b| b.expanded)
    }

    #[must_use]
    pub fn is_open(&self, key: &PrKey, job: &JobId) -> bool {
        self.by_pr.get(key).is_some_and(|b| b.open.contains(job))
    }

    #[must_use]
    pub fn tail_status(&self, key: &PrKey, job: &JobId) -> Option<TailStatus> {
        Some(self.by_pr.get(key)?.tails.get(job)?.status.clone())
    }

    /// How many log tails are read and drawn, for tests.
    #[must_use]
    pub fn tails_ready(&self) -> usize {
        self.by_pr
            .values()
            .flat_map(|b| b.tails.values())
            .filter(|tail| tail.status == TailStatus::Ready)
            .count()
    }

    pub fn toggle(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let state = self.by_pr.entry(key.clone()).or_default();
        state.expanded = !state.expanded;
        cx.notify();
    }

    /// Opens or closes a failing job's log tail; the first open reads the
    /// log.
    pub fn toggle_job(
        &mut self,
        key: &PrKey,
        job: &CiJob,
        session: &Entity<Session>,
        cx: &mut Context<Self>,
    ) {
        let state = self.by_pr.entry(key.clone()).or_default();
        if !state.open.remove(&job.id) {
            state.open.insert(job.id.clone());
            if !state.tails.contains_key(&job.id) {
                let task = Self::read(key.clone(), job.clone(), session, cx);
                self.by_pr.entry(key.clone()).or_default().tails.insert(
                    job.id.clone(),
                    Tail {
                        status: TailStatus::Loading,
                        view: None,
                        _task: task,
                    },
                );
            }
        }
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
            this.update(cx, |this, cx| this.ready(&key, &job.id, &feed, cx))
                .ok();
        })
    }

    fn set_status(&mut self, key: &PrKey, job: &JobId, status: TailStatus, cx: &mut Context<Self>) {
        if let Some(tail) = self.by_pr.get_mut(key).and_then(|b| b.tails.get_mut(job)) {
            tail.status = status;
            cx.notify();
        }
    }

    /// The tail's lines are parsed: a view over them, at the first error.
    fn ready(&mut self, key: &PrKey, job: &JobId, feed: &LogFeed, cx: &mut Context<Self>) {
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
        if let Some(tail) = self.by_pr.get_mut(key).and_then(|b| b.tails.get_mut(job)) {
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

/// One job of the box.
pub(crate) struct JobRow {
    pub state: CiState,
    pub name: SharedString,
    pub job: Option<CiJob>,
    /// The time it took, or what stands in for it.
    pub detail: String,
}

fn rank(state: CiState) -> u8 {
    match state {
        CiState::Fail => 0,
        s if s.is_active() => 1,
        _ => 2,
    }
}

/// Every job of `open`, failing first, then running, then the rest.
pub(crate) fn job_rows(open: &OpenPr) -> Vec<JobRow> {
    let mut rows: Vec<JobRow> = if open.ci.runs.is_empty() {
        open.checks
            .iter()
            .map(|check| JobRow {
                state: check.state,
                name: check.name.clone(),
                job: None,
                detail: check.detail.to_string(),
            })
            .collect()
    } else {
        open.ci
            .runs
            .iter()
            .flat_map(|run| &run.jobs)
            .map(|job| JobRow {
                state: job.state,
                name: job.name.clone().into(),
                detail: ci::duration(job.started_at, job.completed_at)
                    .unwrap_or_else(|| ci_label(job.state).to_owned()),
                job: Some(job.clone()),
            })
            .collect()
    };
    rows.sort_by_key(|row| rank(row.state));
    rows
}

/// The box, as the header of the Conversation draws it.
pub fn render(
    open: &OpenPr,
    boxes: &Entity<CiBoxes>,
    session: &Entity<Session>,
    spinners: &SpinnerSlots,
    cx: &App,
) -> AnyElement {
    let t = ui::theme(cx);
    let c = &t.colors;
    let key = open.key.clone();
    let expanded = boxes.read(cx).is_expanded(&key);
    let rows = job_rows(open);
    let count = |state: CiState| rows.iter().filter(|row| row.state == state).count();
    let counts = [
        CiState::Pass,
        CiState::Fail,
        CiState::Running,
        CiState::Queued,
    ]
    .into_iter()
    .filter(|state| count(*state) > 0)
    .map(|state| {
        h_flex()
            .gap(px(4.0))
            .items_center()
            .child(ci_icon(&t, state, CI_ROW, spinners))
            .child(
                div()
                    .mono(&t)
                    .text_color(ci_color(&t, state))
                    .child(format!("{} {}", count(state), ci_label(state))),
            )
    });
    let caps = session.read(cx).capabilities_for(&key);
    let logs = caps.is_some_and(|caps| caps.job_logs);

    let mut shown = Vec::new();
    let mut failing = 0;
    for row in &rows {
        let is_failing = row.state == CiState::Fail;
        if !expanded && !is_failing {
            continue;
        }
        let ix = if is_failing {
            failing += 1;
            Some(failing - 1)
        } else {
            None
        };
        shown.push(job_block(
            open, row, ix, expanded, logs, boxes, session, spinners, &t, cx,
        ));
    }

    let open_checks = {
        let (session, key) = (session.clone(), key.clone());
        Button::new("open-checks", "Open checks", ButtonKind::Secondary).on_click(
            move |_, _, cx| {
                cx.stop_propagation();
                session.update(cx, |session, cx| {
                    session.show(&key, PanelKind::Checks, OpenMode::Permanent, cx);
                });
            },
        )
    };
    let toggle = {
        let (boxes, key) = (boxes.clone(), key.clone());
        move |_: &gpui::ClickEvent, _: &mut gpui::Window, cx: &mut App| {
            boxes.update(cx, |boxes, cx| boxes.toggle(&key, cx));
        }
    };
    let card = ui::card(cx)
        .id("ci-summary")
        .debug_selector(|| "ci-summary".into())
        .flex()
        .flex_col()
        .gap(px(6.0))
        .p(px(10.0))
        .small(&t);
    fold::toggle_row(card, &t, toggle)
        .child(
            h_flex()
                .gap(px(12.0))
                .items_center()
                .child(
                    Icon::new(if expanded {
                        IconName::ChevronDown
                    } else {
                        IconName::ChevronRight
                    })
                    .size(px(14.0))
                    .text_color(c.text_muted),
                )
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.text_secondary)
                        .child("CI"),
                )
                .when(rows.is_empty(), |el| {
                    el.child(div().text_color(c.text_muted).child(ci_label(open.pr.ci)))
                })
                .children(counts)
                .child(
                    div()
                        .flex_1()
                        .h(px(20.0))
                        .debug_selector(|| "ci-space".into()),
                )
                .child(open_checks),
        )
        .children(shown)
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn job_block(
    open: &OpenPr,
    row: &JobRow,
    failing: Option<usize>,
    expanded: bool,
    logs: bool,
    boxes: &Entity<CiBoxes>,
    session: &Entity<Session>,
    spinners: &SpinnerSlots,
    t: &theme::Theme,
    cx: &App,
) -> AnyElement {
    let c = &t.colors;
    let key = open.key.clone();
    let attempt = row
        .job
        .as_ref()
        .and_then(|job| job.attempt)
        .filter(|n| *n > 1)
        .map(|n| ui::chip(cx, format!("attempt {n}"), c.info));
    let line = h_flex()
        .gap(px(8.0))
        .items_center()
        .min_h(px(22.0))
        .child(ci_icon(t, row.state, CI_ROW, spinners))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(c.text)
                .child(row.name.clone()),
        )
        .children(attempt)
        .child(
            div()
                .flex_none()
                .mono(t)
                .tiny(t)
                .text_color(ci_color(t, row.state))
                .child(row.detail.clone()),
        );
    let mut block = v_flex().gap(px(2.0));
    let menu = JobMenu::new(key.clone(), row.job.as_ref().map(|job| job.id.clone()));
    let popup = move |popup, _: &mut gpui::Window, cx: &mut gpui::Context<_>| menu.popup(popup, cx);
    let job = row.job.clone().filter(|_| failing.is_some());
    let is_open = job
        .as_ref()
        .is_some_and(|job| boxes.read(cx).is_open(&key, &job.id));
    block = match (&job, failing) {
        (Some(job), Some(ix)) => {
            let (boxes, session, job, key) =
                (boxes.clone(), session.clone(), job.clone(), key.clone());
            let line = h_flex()
                .id(("ci-failing", ix))
                .debug_selector(move || format!("ci-failing-{ix}"))
                .px(px(4.0))
                .mx(px(-4.0))
                .rounded(t.radius.sm)
                .child(line.flex_1());
            block.child(
                fold::toggle_row(line, t, move |_, _, cx| {
                    boxes.update(cx, |boxes, cx| boxes.toggle_job(&key, &job, &session, cx));
                })
                .context_menu(popup),
            )
        }
        _ => {
            let selector = format!("ci-job-{}", row.name);
            block.child(
                line.id(SharedString::from(format!("ci-job-{}", row.name)))
                    .debug_selector(move || selector)
                    .context_menu(popup),
            )
        }
    };
    if let Some(steps) = row.job.as_ref().map(|job| &job.steps)
        && !steps.is_empty()
        && expanded
    {
        block = block.children(steps.iter().map(|step| step_row(step, spinners, t)));
    }
    if is_open && let Some(job) = &job {
        block = block.child(tail_block(
            open,
            job,
            failing.unwrap_or(0),
            logs,
            boxes,
            session,
            t,
            cx,
        ));
    }
    block.into_any_element()
}

fn step_row(step: &CheckStep, spinners: &SpinnerSlots, t: &theme::Theme) -> AnyElement {
    let c = &t.colors;
    h_flex()
        .gap(px(8.0))
        .items_center()
        .pl(px(22.0))
        .min_h(px(20.0))
        .child(ci_icon(t, step.state, 12.0, spinners))
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

/// The log tail under a failing job, and the way to the whole log. It sits
/// above the box in the hit order, so selecting text in it does not fold
/// the box.
#[allow(clippy::too_many_arguments)]
fn tail_block(
    open: &OpenPr,
    job: &CiJob,
    ix: usize,
    logs: bool,
    boxes: &Entity<CiBoxes>,
    session: &Entity<Session>,
    t: &theme::Theme,
    cx: &App,
) -> AnyElement {
    let c = &t.colors;
    let key = open.key.clone();
    let boxes = boxes.read(cx);
    let status = boxes
        .tail_status(&key, &job.id)
        .unwrap_or(TailStatus::Loading);
    let body = match &status {
        TailStatus::Ready => boxes
            .by_pr
            .get(&key)
            .and_then(|b| b.tails.get(&job.id))
            .and_then(|tail| tail.view.clone())
            .map(|view| {
                let height = view.read(cx).style().line_height * VIEW_LINES;
                div()
                    .debug_selector(move || format!("ci-tail-{ix}"))
                    .h(height)
                    .rounded(t.radius.sm)
                    .overflow_hidden()
                    .child(view)
                    .into_any_element()
            }),
        TailStatus::Loading => Some(note(t, "Reading the log")),
        TailStatus::Unavailable => Some(note(
            t,
            "This forge has no log API; open the job on the web",
        )),
        TailStatus::Failed(error) => Some(note(t, &format!("Log not read: {error}"))),
    };
    let full = logs.then(|| {
        let (session, key, id) = (session.clone(), key.clone(), job.id.clone());
        div()
            .id(("ci-full-log", ix))
            .debug_selector(move || format!("ci-full-log-{ix}"))
            .cursor_pointer()
            .text_color(c.accent_fg)
            .hover(|s| s.underline())
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                session.update(cx, |session, cx| {
                    session.focus(&key, cx);
                    session.open_log(&id, None, cx);
                });
            })
            .child("Open full log")
            .into_any_element()
    });
    let web = job
        .url
        .clone()
        .filter(|url| !logs && crate::untrusted::is_web_url(url))
        .map(|url| {
            div()
                .id(("ci-web-log", ix))
                .cursor_pointer()
                .text_color(c.accent_fg)
                .hover(|s| s.underline())
                .on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    cx.open_url(&url);
                })
                .child("Log on the web")
                .into_any_element()
        });
    v_flex()
        .occlude()
        .gap(px(4.0))
        .pl(px(22.0))
        .pb(px(4.0))
        .children(body)
        .child(h_flex().gap(px(12.0)).children(full).children(web))
        .into_any_element()
}

fn note(t: &theme::Theme, text: &str) -> AnyElement {
    div()
        .text_color(t.colors.text_muted)
        .child(text.to_owned())
        .into_any_element()
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
}
