//! The pipelines card at the end of the Conversation: the only place the
//! runs and jobs are listed (no CI box at the top, no Checks tab, no checks
//! section in the sidebar). Its header row folds it from a click anywhere
//! on it or from Enter and Space; open, it lists each run with its jobs. A
//! job opens its log in the Log tab, and a re-run that would start several
//! jobs asks first. On fixtures, and on a fake forge behind the real forge
//! hub, where the confirmed re-run reaches the forge.

use std::sync::Arc;

use app::ci::{CiActions, failed_jobs};
use app::conversation::pipelines::{LogTails, ix_of};
use app::fixtures::Fixtures;
use app::panels::{CONVERSATION, LOG, PanelKind};
use app::session::OpenMode;
use app::sidebar_panels::Section;
use forge::Capabilities;
use forge::model::{CiJob, CiRun, CiState, ForgeKind, PullRequest};
use gpui::{SharedString, TestAppContext, point, px};

use crate::harness::{
    FAKE_LOG, FakeForge, Options, Scratch, ShellWindow, fake_services, flat, go_live, open_shell,
};

/// A fixture pull request on a forge that serves logs and re-runs, with a
/// run where more than one job failed: its forge pull request and that run.
fn with_failed_jobs() -> (PullRequest, ForgeKind, CiRun) {
    let fixtures = Fixtures::load();
    fixtures
        .inbox
        .prs
        .iter()
        .filter(|row| {
            let actions = CiActions::of(&Capabilities::baseline(row.forge));
            actions.logs && actions.retry
        })
        .find_map(|row| {
            let run = fixtures
                .detail(&row.key)
                .runs
                .into_iter()
                .find(|run| failed_jobs(run).count() >= 2)?;
            Some((fixtures.forge_pr(&row.key)?, row.forge, run))
        })
        .expect("a fixture pull request with two failed jobs in one run")
}

/// Scrolls the Conversation until the element with `selector` is inside it.
pub(crate) fn reveal(shell: &mut ShellWindow, selector: &str) {
    let selector = String::leak(selector.to_owned());
    for _ in 0..80 {
        let body = shell.bounds_of("conversation-body");
        let element = shell.bounds_of(selector);
        if element.top() >= body.top() && element.bottom() <= body.bottom() {
            return;
        }
        let dy = if element.top() < body.top() {
            -120.0
        } else {
            120.0
        };
        shell.scroll_at(body.center(), dy);
    }
    panic!("{selector} never came into view");
}

/// Whether the focused pull request's card lists its runs.
pub(crate) fn listed(shell: &mut ShellWindow) -> bool {
    let session = shell.session();
    shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.ci.listed)
    })
}

/// Shows `number`'s Conversation and opens its pipelines card with a click
/// on the card's header.
pub(crate) fn open_card(shell: &mut ShellWindow, number: u64) {
    shell.show(number, PanelKind::Conversation, OpenMode::Permanent);
    reveal(shell, "merge-box-checks");
    if !listed(shell) {
        shell.click("merge-box-checks");
    }
    assert!(listed(shell), "the card is open");
}

fn shown_log_job(shell: &mut ShellWindow) -> Option<String> {
    let session = shell.session();
    shell.update(|_, cx| {
        let open = session.read(cx).open_pr()?;
        Some(open.ci.log.as_ref()?.job.id.as_str().to_owned())
    })
}

fn log_text(shell: &mut ShellWindow) -> Option<String> {
    let panel = shell.log_panel();
    shell.update(|_, cx| {
        let feed = panel.read(cx).feed()?;
        feed.wait_settled(std::time::Duration::from_secs(10));
        Some(feed.read(|buffer| {
            buffer
                .lines(0..buffer.total())
                .map(|line| line.text.to_owned())
                .collect::<Vec<_>>()
                .join("\n")
        }))
    })
}

fn confirm_jobs(shell: &mut ShellWindow) -> Option<usize> {
    let session = shell.session();
    shell.update(|_, cx| {
        let open = session.read(cx).open_pr()?;
        Some(open.ci.confirm.as_ref()?.jobs.len())
    })
}

fn job_states(shell: &mut ShellWindow, run: &CiRun) -> Vec<CiState> {
    let session = shell.session();
    shell.update(|_, cx| {
        let open = session.read(cx).open_pr().expect("a pull request is open");
        let shown = open
            .ci
            .runs
            .iter()
            .find(|shown| shown.id == run.id)
            .expect("the run is shown");
        shown.jobs.iter().map(|job| job.state).collect()
    })
}

/// Clicks a job's row, which opens its log.
fn pick_job(shell: &mut ShellWindow, job: &CiJob) {
    let row = format!("ci-job-{}", job.id.as_str());
    reveal(shell, &row);
    shell.click_named(&row);
}

/// Clicks the run's "Re-run failed jobs".
fn rerun_failed(shell: &mut ShellWindow, run: &CiRun) {
    reveal(shell, &format!("ci-run-{}", run.id.as_str()));
    shell.click_element((SharedString::from("ci-retry-run"), ix_of(run)));
}

fn press(shell: &mut ShellWindow, id: &'static str) {
    reveal(shell, "ci-retry-confirm");
    shell.click_element(SharedString::from(id));
}

#[gpui::test]
fn the_top_box_the_checks_tab_and_section_are_gone(cx: &mut TestAppContext) {
    let mut shell = open_shell(cx, Options::default());
    shell.open_pr(1288);
    shell.show(1288, PanelKind::Conversation, OpenMode::Permanent);
    assert!(shell.drawn("pr-actions"), "the header is drawn");
    assert!(!shell.drawn("ci-summary"), "no CI box at the top");
    assert!(
        shell.element(SharedString::from("open-checks")).is_none(),
        "no Open checks button"
    );
    assert!(
        shell.drawn("merge-box-checks"),
        "the card at the end has it"
    );
    assert!(
        !Section::ALL
            .iter()
            .any(|section| section.name() == "Rollup"),
        "no checks section"
    );
    assert!(!shell.drawn("section-Rollup"));
    assert!(
        shell.drawn("section-Threads"),
        "the other sections are there"
    );
    let groups = shell.groups();
    assert!(
        groups.iter().flatten().all(|name| name != "Checks"),
        "no Checks tab: {groups:?}"
    );
    assert!(
        groups.iter().flatten().any(|name| name == CONVERSATION),
        "{groups:?}"
    );

    // The inbox row's CI icon leads to the card, not to a tab of its own.
    shell.click("inbox-ci-412");
    let conversation = shell
        .panel_of(412, PanelKind::Conversation)
        .expect("#412's Conversation");
    assert!(shell.is_shown(conversation));
    assert!(listed(&mut shell), "with its pipelines card open");
}

#[gpui::test]
fn the_header_row_folds_the_card_from_anywhere_on_it(cx: &mut TestAppContext) {
    let (pr, _, run) = with_failed_jobs();
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            open_pr: Some(pr.number.0),
            ..Options::default()
        },
    );
    shell.show(pr.number.0, PanelKind::Conversation, OpenMode::Permanent);
    reveal(&mut shell, "merge-box-checks");
    assert!(!listed(&mut shell), "it starts folded");
    assert!(shell.drawn("merge-box-checks-chevron-folded"));
    assert!(!shell.drawn("merge-box-jobs"));
    assert!(
        shell
            .element(SharedString::from("merge-box-checks"))
            .is_none(),
        "no Show button: the row is the toggle"
    );
    let folded = shell.bounds_of("merge-box-checks-chevron-folded");
    let header = shell.bounds_of("merge-box-checks");

    // Its middle, past the text, and its left edge.
    let middle = point(header.center().x, header.center().y);
    shell.click_at(middle);
    assert!(listed(&mut shell), "a click in its middle opened it");
    assert!(shell.drawn("merge-box-checks-chevron-open"));
    assert!(!shell.drawn("merge-box-checks-chevron-folded"));
    let open = shell.bounds_of("merge-box-checks-chevron-open");
    assert_eq!(open.top(), folded.top(), "the chevron did not move");
    assert_eq!(open.left(), folded.left());
    assert!(shell.drawn("merge-box-jobs"), "the runs are listed");
    let job = run.jobs.first().expect("a job");
    assert!(
        shell.drawn(&format!("ci-run-{}", run.id.as_str())),
        "each run"
    );
    assert!(
        shell.drawn(&format!("ci-job-{}", job.id.as_str())),
        "with its jobs"
    );

    let header = shell.bounds_of("merge-box-checks");
    shell.click_at(point(header.left() + px(2.0), header.center().y));
    assert!(!listed(&mut shell), "a click at its edge folded it");
    assert!(!shell.drawn("merge-box-jobs"));

    // The click left the focus on the row: Enter and Space fold it too.
    shell.keys("enter");
    assert!(listed(&mut shell), "Enter opened it");
    shell.keys("space");
    assert!(!listed(&mut shell), "and Space folded it");

    // A control in the row keeps its own action.
    shell.keys("enter");
    reveal(&mut shell, "merge-box-checks");
    shell.click("watch-pr");
    assert!(listed(&mut shell), "the bell did not fold the card");
    rerun_failed(&mut shell, &run);
    assert!(confirm_jobs(&mut shell).is_some(), "the re-run asked");
    assert!(listed(&mut shell), "and did not fold the card");
}

#[gpui::test]
fn a_job_opens_its_log_and_a_wide_rerun_asks_first(cx: &mut TestAppContext) {
    let (pr, _, run) = with_failed_jobs();
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            open_pr: Some(pr.number.0),
            ..Options::default()
        },
    );
    open_card(&mut shell, pr.number.0);

    let job = failed_jobs(&run).next().expect("a failed job").clone();
    pick_job(&mut shell, &job);
    assert!(
        shell.shown_tabs().iter().any(|tab| tab == LOG),
        "the Log tab came forward"
    );
    assert_eq!(shown_log_job(&mut shell).as_deref(), Some(job.id.as_str()));
    let text = log_text(&mut shell).expect("the Log panel shows a log");
    let first_step = job.steps.first().map_or("", |step| step.name.as_str());
    assert!(
        !text.is_empty() && text.contains(first_step),
        "the job's own log: {text}"
    );

    // Another job reuses the one Log tab.
    let other = failed_jobs(&run)
        .nth(1)
        .expect("a second failed job")
        .clone();
    shell.show(pr.number.0, PanelKind::Conversation, OpenMode::Permanent);
    pick_job(&mut shell, &other);
    assert_eq!(
        shown_log_job(&mut shell).as_deref(),
        Some(other.id.as_str())
    );
    let logs = shell
        .groups()
        .iter()
        .flatten()
        .filter(|tab| *tab == LOG)
        .count();
    assert_eq!(logs, 1, "one Log tab");

    shell.show(pr.number.0, PanelKind::Conversation, OpenMode::Permanent);
    let failed_before = job_states(&mut shell, &run);
    rerun_failed(&mut shell, &run);
    let failed = failed_jobs(&run).count();
    assert_eq!(confirm_jobs(&mut shell), Some(failed), "it asks first");
    assert!(shell.drawn("ci-retry-confirm"), "in the card");
    assert_eq!(
        job_states(&mut shell, &run),
        failed_before,
        "nothing re-ran yet"
    );

    press(&mut shell, "cancel-retry");
    assert_eq!(confirm_jobs(&mut shell), None);
    assert_eq!(job_states(&mut shell, &run), failed_before);

    rerun_failed(&mut shell, &run);
    press(&mut shell, "confirm-retry");
    assert_eq!(confirm_jobs(&mut shell), None);
    let states = job_states(&mut shell, &run);
    assert!(
        !states.contains(&CiState::Fail),
        "the failed jobs are queued again: {states:?}"
    );
    assert!(
        shell.drawn("ci-notice"),
        "the card says the re-run is queued"
    );
}

#[gpui::test]
fn a_failed_job_unfolds_its_steps_and_log_tail(cx: &mut TestAppContext) {
    let (pr, _, run) = with_failed_jobs();
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(pr.number.0),
            ..Options::default()
        },
    );
    open_card(&mut shell, pr.number.0);
    let job = failed_jobs(&run).next().expect("a failed job").clone();
    let tag = job.id.as_str().to_owned();
    let tabs = shell.shown_tabs();
    reveal(&mut shell, &format!("ci-job-{tag}"));
    shell.click_named(&format!("ci-job-fold-{tag}"));
    let tails = shell.update(|_, cx| LogTails::global(cx));
    shell.settle_until("the log tail", |cx| tails.read(cx).ready() > 0);
    assert!(
        shell.drawn(&format!("ci-tail-{tag}")),
        "the tail is under the failed job"
    );
    assert_eq!(shell.shown_tabs(), tabs, "no Log tab for a tail");
    assert!(listed(&mut shell), "the chevron did not fold the card");

    reveal(&mut shell, &format!("ci-full-log-{tag}"));
    shell.click_named(&format!("ci-full-log-{tag}"));
    assert_eq!(shown_log_job(&mut shell).as_deref(), Some(tag.as_str()));
    assert!(shell.shown_tabs().iter().any(|tab| tab == LOG));

    shell.show(pr.number.0, PanelKind::Conversation, OpenMode::Permanent);
    reveal(&mut shell, &format!("ci-job-fold-{tag}"));
    shell.click_named(&format!("ci-job-fold-{tag}"));
    assert!(
        !shell.drawn(&format!("ci-tail-{tag}")),
        "a second click folded it"
    );
}

#[gpui::test]
fn a_confirmed_rerun_reaches_the_forge(cx: &mut TestAppContext) {
    let (pr, kind, run) = with_failed_jobs();
    let scratch = Scratch::new("pipelines-live");
    let fake = FakeForge::new(kind, &pr.repo.host, vec![run.clone()]);
    let services = fake_services(
        &scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(
        cx,
        Options {
            theme: flat(),
            ..Options::default()
        },
    );
    go_live(&mut shell, services, vec![pr.clone()], kind);
    shell.keys("j enter");
    let session = shell.session();
    shell.settle_until("the runs from the fake forge", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.ci.runs.iter().any(|shown| shown.id == run.id))
    });
    open_card(&mut shell, pr.number.0);

    let job = failed_jobs(&run).next().expect("a failed job").clone();
    pick_job(&mut shell, &job);
    assert_eq!(shown_log_job(&mut shell).as_deref(), Some(job.id.as_str()));
    let panel = shell.log_panel();
    shell.settle_until("the log from the fake forge", |cx| {
        panel
            .read(cx)
            .feed()
            .is_some_and(|feed| feed.read(|buffer| buffer.total() > 0))
    });
    let text = log_text(&mut shell).expect("the Log panel shows a log");
    assert!(text.contains("1 test failed"), "the forge's log: {text}");
    assert_eq!(FAKE_LOG.lines().count(), 3);

    shell.show(pr.number.0, PanelKind::Conversation, OpenMode::Permanent);
    rerun_failed(&mut shell, &run);
    assert!(confirm_jobs(&mut shell).is_some_and(|jobs| jobs >= 2));
    assert!(
        fake.retries().is_empty(),
        "nothing is sent before the confirm"
    );
    press(&mut shell, "confirm-retry");
    shell.settle_until("the re-run at the forge", |_| !fake.retries().is_empty());
    assert_eq!(fake.retries(), [format!("run {}", run.id.as_str())]);
}

/// Lets the CI poll fire, then its read's debounce.
fn poll(shell: &mut ShellWindow) {
    shell.advance(app::session::CI_ACTIVE_POLL);
    shell.advance(std::time::Duration::from_secs(1));
}

/// `run` with every job, and the run, in `state`.
fn all_in(run: &CiRun, state: CiState) -> CiRun {
    let mut run = run.clone();
    run.state = state;
    for job in &mut run.jobs {
        job.state = state;
    }
    run
}

#[gpui::test]
fn running_ci_is_read_again_until_it_finishes(cx: &mut TestAppContext) {
    let (pr, kind, run) = with_failed_jobs();
    let scratch = Scratch::new("pipelines-poll");
    let fake = FakeForge::new(kind, &pr.repo.host, vec![all_in(&run, CiState::Running)]);
    let services = fake_services(
        &scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr.clone()], kind);
    shell.keys("j enter");
    let session = shell.session();
    let shown = |state: CiState| {
        let session = session.clone();
        move |cx: &mut gpui::App| {
            session.read(cx).open_pr().is_some_and(|open| {
                !open.ci.runs.is_empty() && open.ci.runs.iter().all(|run| run.state == state)
            })
        }
    };
    shell.settle_until("the running run", shown(CiState::Running));
    open_card(&mut shell, pr.number.0);
    let job = run.jobs.first().expect("a job");
    let row = format!("ci-job-{}", job.id.as_str());
    assert!(shell.drawn(&row), "the card lists the running job");

    let reads = fake.ci_reads();
    poll(&mut shell);
    shell.settle_until("a second read while it runs", |_| fake.ci_reads() > reads);

    fake.set_runs(vec![all_in(&run, CiState::Pass)]);
    poll(&mut shell);
    shell.settle_until("the finished run", shown(CiState::Pass));
    assert!(shell.drawn(&row), "the card follows the new states");
    let reads = fake.ci_reads();
    shell.advance(app::session::CI_ACTIVE_POLL * 3);
    assert_eq!(fake.ci_reads(), reads, "a finished run is not read again");
}
