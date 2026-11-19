//! The Checks panel: picking a job shows its log in the Log panel, and a
//! re-run that would start several jobs asks first. On fixtures, and on a
//! fake forge behind the real forge hub, where the confirmed re-run reaches
//! the forge.

use std::sync::Arc;

use app::checks_panel::ix_of;
use app::ci::{CiActions, failed_jobs};
use app::fixtures::Fixtures;
use app::panels::{CHECKS, LOG};
use forge::Capabilities;
use forge::model::{CiJob, CiRun, CiState, ForgeKind, PullRequest};
use gpui::{SharedString, TestAppContext};

use crate::harness::{
    FAKE_LOG, FakeForge, Options, Scratch, ShellWindow, fake_services, go_live, open_shell,
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

use crate::harness::flat;

fn show_checks(shell: &mut ShellWindow) {
    let session = shell.session();
    shell.update(|_, cx| {
        session.update(cx, |session, cx| {
            session.show_panel(app::panels::PanelKind::Checks, cx)
        })
    });
    shell.settle();
    assert!(shell.shown_tabs().iter().any(|tab| tab == CHECKS));
}

fn job_selector(job: &CiJob) -> &'static str {
    String::leak(format!("job-{}", job.id.as_str()))
}

/// Clicks the job's row, and for a job whose row unfolds its steps, the
/// "View log" button that appears.
fn pick_job(shell: &mut ShellWindow, job: &CiJob) {
    shell.click(job_selector(job));
    if !job.steps.is_empty() {
        let session = shell.session();
        let unfolded = shell.update(|_, cx| {
            session
                .read(cx)
                .open_pr()
                .is_some_and(|open| open.ci.expanded.contains(&job.id))
        });
        assert!(unfolded, "a click on a job with steps unfolds them");
        shell.click_element(SharedString::from(format!("log-{}", job.id.as_str())));
    }
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
    show_checks(&mut shell);

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

    show_checks(&mut shell);
    let failed_before = job_states(&mut shell, &run);
    let retry = (SharedString::from("retry-run"), ix_of(&run));
    shell.click_element(retry.clone());
    let failed = failed_jobs(&run).count();
    assert_eq!(confirm_jobs(&mut shell), Some(failed), "it asks first");
    assert_eq!(
        job_states(&mut shell, &run),
        failed_before,
        "nothing re-ran yet"
    );

    shell.click_element(SharedString::from("cancel-retry"));
    assert_eq!(confirm_jobs(&mut shell), None);
    assert_eq!(job_states(&mut shell, &run), failed_before);

    shell.click_element(retry);
    shell.click_element(SharedString::from("confirm-retry"));
    assert_eq!(confirm_jobs(&mut shell), None);
    let states = job_states(&mut shell, &run);
    assert!(
        !states.contains(&CiState::Fail),
        "the failed jobs are queued again: {states:?}"
    );
}

#[gpui::test]
fn a_confirmed_rerun_reaches_the_forge(cx: &mut TestAppContext) {
    let (pr, kind, run) = with_failed_jobs();
    let scratch = Scratch::new("checks-live");
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
    show_checks(&mut shell);

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

    show_checks(&mut shell);
    shell.click_element((SharedString::from("retry-run"), ix_of(&run)));
    assert!(confirm_jobs(&mut shell).is_some_and(|jobs| jobs >= 2));
    assert!(
        fake.retries().is_empty(),
        "nothing is sent before the confirm"
    );
    shell.click_element(SharedString::from("confirm-retry"));
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
    let scratch = Scratch::new("checks-poll");
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

    let reads = fake.ci_reads();
    poll(&mut shell);
    shell.settle_until("a second read while it runs", |_| fake.ci_reads() > reads);

    fake.set_runs(vec![all_in(&run, CiState::Pass)]);
    poll(&mut shell);
    shell.settle_until("the finished run", shown(CiState::Pass));
    let reads = fake.ci_reads();
    shell.advance(app::session::CI_ACTIVE_POLL * 3);
    assert_eq!(fake.ci_reads(), reads, "a finished run is not read again");
}
