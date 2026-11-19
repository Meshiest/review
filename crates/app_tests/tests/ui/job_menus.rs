//! A CI job's right-click menu, the same in the sidebar's Checks section,
//! the Checks panel, the Conversation's CI box and its merge box: the log,
//! the Checks tab, re-runs through the forge (several jobs only after the
//! confirmation), watching the job's run, its link; what the forge cannot
//! do is disabled with the reason. A click on the rollup line opens the
//! Checks tab.

use std::sync::Arc;

use app::checks_panel::job_menu::{JobAction, JobEntry, JobMenu};
use app::ci::{CiActions, failed_jobs};
use app::fixtures::Fixtures;
use app::model::PrKey;
use app::panels::{CHECKS, LOG, PanelKind};
use app::session::OpenMode;
use app::subscriptions::Subscriptions;
use forge::Capabilities;
use forge::model::{CiJob, CiRun, CiState, ForgeKind, JobId, PullRequest};
use gpui::{Modifiers, MouseButton, SharedString, TestAppContext};

use crate::harness::{
    FakeForge, Options, Scratch, ShellWindow, fake_services, go_live, open_shell,
};

/// A fixture pull request on a forge that re-runs jobs, with a run where
/// more than one job failed.
fn with_failed_jobs() -> (PullRequest, ForgeKind, CiRun) {
    let fixtures = Fixtures::load();
    fixtures
        .inbox
        .prs
        .iter()
        .filter(|row| CiActions::of(&Capabilities::baseline(row.forge)).retry)
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

fn right_click(shell: &mut ShellWindow, selector: &str) {
    let at = shell.bounds_of(String::leak(selector.to_owned())).center();
    let none = Modifiers::none();
    shell.cx.simulate_mouse_move(at, None, none);
    shell.cx.simulate_mouse_down(at, MouseButton::Right, none);
    shell.cx.simulate_mouse_up(at, MouseButton::Right, none);
    shell.settle();
}

fn entries(shell: &mut ShellWindow, menu: &JobMenu) -> Vec<JobEntry> {
    shell.update(|_, cx| menu.entries(cx).into_iter().flatten().collect())
}

fn entry(shell: &mut ShellWindow, menu: &JobMenu, pick: fn(JobAction) -> bool) -> JobEntry {
    entries(shell, menu)
        .into_iter()
        .find(|entry| pick(entry.action))
        .expect("the menu has the entry")
}

fn act(shell: &mut ShellWindow, menu: &JobMenu, action: JobAction) {
    shell.update(|_, cx| menu.run(action, cx));
    shell.settle();
}

fn confirm_jobs(shell: &mut ShellWindow) -> Option<usize> {
    let session = shell.session();
    shell.update(|_, cx| Some(session.read(cx).open_pr()?.ci.confirm.as_ref()?.jobs.len()))
}

/// The pull request open on a [`FakeForge`] that serves `run`.
fn live(cx: &mut TestAppContext, scratch: &Scratch) -> (ShellWindow, Arc<FakeForge>, PrKey, CiRun) {
    let (pr, kind, run) = with_failed_jobs();
    let fake = FakeForge::new(kind, &pr.repo.host, vec![run.clone()]);
    let services = fake_services(
        scratch,
        vec![fake.clone() as Arc<dyn forge::Forge>],
        std::slice::from_ref(&pr.repo),
    );
    let mut shell = open_shell(cx, Options::default());
    go_live(&mut shell, services, vec![pr], kind);
    shell.keys("j enter");
    let session = shell.session();
    shell.settle_until("the runs from the fake forge", |cx| {
        session
            .read(cx)
            .open_pr()
            .is_some_and(|open| open.ci.runs.iter().any(|shown| shown.id == run.id))
    });
    let key = shell.update(|_, cx| session.read(cx).open_key().cloned().expect("open"));
    (shell, fake, key, run)
}

fn failed(run: &CiRun) -> CiJob {
    failed_jobs(run).next().expect("a failed job").clone()
}

#[gpui::test]
fn the_sidebar_job_menu_reruns_one_job_at_the_forge(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-sidebar");
    let (mut shell, fake, _, run) = live(cx, &scratch);
    let job = failed(&run);
    right_click(&mut shell, "sidebar-check-0");
    assert!(shell.drawn("job-menu-view-log"), "the job's menu");
    shell.click("job-menu-rerun-job");
    shell.settle_until("the re-run at the forge", |_| !fake.retries().is_empty());
    assert_eq!(fake.retries(), [format!("job {}", job.id.as_str())]);
    assert_eq!(
        confirm_jobs(&mut shell),
        None,
        "one job needs no confirmation"
    );
}

#[gpui::test]
fn rerun_failed_jobs_asks_first_from_the_checks_panel(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-checks");
    let (mut shell, fake, key, run) = live(cx, &scratch);
    let job = failed(&run);
    shell.show(key.number.0, PanelKind::Checks, OpenMode::Permanent);
    right_click(&mut shell, &format!("job-{}", job.id.as_str()));
    shell.click("job-menu-rerun-failed");
    assert!(
        confirm_jobs(&mut shell).is_some_and(|jobs| jobs >= 2),
        "it asks first"
    );
    assert!(shell.shown_tabs().iter().any(|tab| tab == CHECKS));
    assert!(
        fake.retries().is_empty(),
        "nothing is sent before the confirm"
    );
    shell.click_element(SharedString::from("confirm-retry"));
    shell.settle_until("the re-run at the forge", |_| !fake.retries().is_empty());
    assert_eq!(fake.retries(), [format!("run {}", run.id.as_str())]);
}

#[gpui::test]
fn the_ci_box_job_menu_watches_the_run(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-ci-box");
    let (mut shell, _, key, run) = live(cx, &scratch);
    let job = failed(&run);
    shell.show(key.number.0, PanelKind::Conversation, OpenMode::Permanent);
    let watched = |shell: &mut ShellWindow| {
        shell.update(|_, cx| {
            Subscriptions::global(cx)
                .expect("watching is on")
                .read(cx)
                .is_run_watched(&key, &run.id)
        })
    };
    assert!(!watched(&mut shell));
    right_click(&mut shell, "ci-failing-0");
    shell.click("job-menu-watch");
    assert!(watched(&mut shell), "the job's run is watched");
    let menu = JobMenu::new(key.clone(), Some(job.id.clone()));
    let watch = entry(&mut shell, &menu, |a| matches!(a, JobAction::Watch(_)));
    assert_eq!(
        (watch.action, watch.label),
        (JobAction::Watch(false), "Unwatch run")
    );
    act(&mut shell, &menu, JobAction::Watch(false));
    assert!(!watched(&mut shell));

    // The merge box's job list has the same menu.
    shell.click_element(SharedString::from("merge-box-checks"));
    right_click(&mut shell, "merge-box-job-0");
    assert!(shell.drawn("job-menu-show-in-checks"));
    shell.click("job-menu-show-in-checks");
    assert!(shell.shown_tabs().iter().any(|tab| tab == CHECKS));
}

#[gpui::test]
fn the_menu_views_logs_and_copies_links_on_fixtures(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    let (number, job, url) = fixtures
        .inbox
        .prs
        .iter()
        .filter(|row| CiActions::of(&Capabilities::baseline(row.forge)).logs)
        .find_map(|row| {
            fixtures.detail(&row.key).runs.into_iter().find_map(|run| {
                let job = run
                    .jobs
                    .iter()
                    .find(|job| job.started_at.is_some() && !job.state.is_active())?
                    .clone();
                let url = job.url.clone().or(run.url.clone())?;
                Some((row.key.number.0, job, url))
            })
        })
        .expect("a fixture job with a log and a web page");
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            ..Options::default()
        },
    );
    let key = shell.key(number);
    let menu = JobMenu::new(key, Some(job.id.clone()));
    act(&mut shell, &menu, JobAction::ViewLog);
    assert!(shell.shown_tabs().iter().any(|tab| tab == LOG));
    let session = shell.session();
    let shown = shell.update(|_, cx| {
        session
            .read(cx)
            .open_pr()
            .and_then(|open| Some(open.ci.log.as_ref()?.job.id.clone()))
    });
    assert_eq!(shown, Some(job.id.clone()));

    act(&mut shell, &menu, JobAction::CopyLink);
    assert_eq!(
        shell.cx.read_from_clipboard().and_then(|item| item.text()),
        Some(url.clone())
    );
    act(&mut shell, &menu, JobAction::OpenOnForge);
    assert_eq!(shell.cx.opened_url(), Some(url));

    act(&mut shell, &menu, JobAction::ShowInChecks);
    assert!(shell.shown_tabs().iter().any(|tab| tab == CHECKS));
}

#[gpui::test]
fn what_the_forge_cannot_do_is_disabled_with_the_reason(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    // Forgejo at the oldest version re-runs nothing and reads no logs.
    let (number, job) = fixtures
        .inbox
        .prs
        .iter()
        .filter(|row| row.forge == ForgeKind::Forgejo)
        .find_map(|row| {
            let job = fixtures
                .detail(&row.key)
                .runs
                .into_iter()
                .flat_map(|run| run.jobs)
                .next()?;
            Some((row.key.number.0, job))
        })
        .expect("a Forgejo fixture with a job");
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            ..Options::default()
        },
    );
    let key = shell.key(number);
    let menu = JobMenu::new(key.clone(), Some(job.id.clone()));
    let rerun = entry(&mut shell, &menu, |a| a == JobAction::RerunJob);
    assert_eq!(
        rerun.blocked.as_deref(),
        Some("Forgejo cannot re-run jobs from here: open it on the forge")
    );
    let failed = entry(&mut shell, &menu, |a| a == JobAction::RerunFailed);
    assert_eq!(failed.blocked, rerun.blocked);
    let log = entry(&mut shell, &menu, |a| a == JobAction::ViewLog);
    assert_eq!(
        log.blocked.as_deref(),
        Some("Forgejo has no job logs here: open it on the forge")
    );
    let checks = entry(&mut shell, &menu, |a| a == JobAction::ShowInChecks);
    assert_eq!(checks.blocked, None);

    // A running job on a forge that re-runs cannot be re-run yet.
    let running = fixtures.inbox.prs.iter().find_map(|row| {
        if !CiActions::of(&Capabilities::baseline(row.forge)).retry {
            return None;
        }
        let job = fixtures
            .detail(&row.key)
            .runs
            .into_iter()
            .flat_map(|run| run.jobs)
            .find(|job| job.state == CiState::Running)?;
        Some((row.key.number.0, job.id))
    });
    if let Some((number, job)) = running {
        shell.open_pr(number);
        let menu = JobMenu::new(shell.key(number), Some(job));
        let rerun = entry(&mut shell, &menu, |a| a == JobAction::RerunJob);
        assert_eq!(rerun.blocked.as_deref(), Some("It has not finished yet"));
    }

    // A check of the rollup with no job behind it.
    let menu = JobMenu::new(key, None::<JobId>);
    let rerun = entry(&mut shell, &menu, |a| a == JobAction::RerunJob);
    assert!(
        rerun
            .blocked
            .is_some_and(|why| why.starts_with("No job behind"))
    );
}

#[gpui::test]
fn the_rollup_line_opens_the_checks_tab(cx: &mut TestAppContext) {
    let fixtures = Fixtures::load();
    // A pull request with passing jobs: they roll up into one line.
    let number = fixtures
        .inbox
        .prs
        .iter()
        .find(|row| {
            let detail = fixtures.detail(&row.key);
            detail
                .runs
                .iter()
                .flat_map(|run| &run.jobs)
                .any(|job| job.state == CiState::Pass)
        })
        .expect("a fixture with a passing job")
        .key
        .number
        .0;
    let mut shell = open_shell(
        cx,
        Options {
            open_pr: Some(number),
            ..Options::default()
        },
    );
    assert!(!shell.shown_tabs().iter().any(|tab| tab == CHECKS));
    shell.click("sidebar-checks-summary");
    assert!(shell.shown_tabs().iter().any(|tab| tab == CHECKS));
}
