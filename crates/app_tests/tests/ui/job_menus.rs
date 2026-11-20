//! A CI job's right-click menu in the Conversation's pipelines card: the
//! log, re-runs through the forge (several jobs only after the confirmation
//! in the card), watching the job's run, its link; what the forge cannot do
//! is disabled with the reason.

use std::sync::Arc;

use app::ci::{CiActions, failed_jobs};
use app::conversation::job_menu::{JobAction, JobEntry, JobMenu};
use app::fixtures::Fixtures;
use app::model::PrKey;
use app::panels::LOG;
use app::subscriptions::Subscriptions;
use forge::Capabilities;
use forge::model::{CiJob, CiRun, CiState, ForgeKind, JobId, PullRequest};
use gpui::{Modifiers, MouseButton, SharedString, TestAppContext};

use crate::pipelines::{open_card, reveal};

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
    reveal(shell, selector);
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

fn job_row(job: &CiJob) -> String {
    format!("ci-job-{}", job.id.as_str())
}

#[gpui::test]
fn the_job_menu_reruns_one_job_at_the_forge(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-one");
    let (mut shell, fake, key, run) = live(cx, &scratch);
    let job = failed(&run);
    open_card(&mut shell, key.number.0);
    right_click(&mut shell, &job_row(&job));
    assert!(shell.drawn("job-menu-view-log"), "the job's menu");
    assert!(
        !shell.drawn("job-menu-show-in-checks"),
        "no way to a Checks tab"
    );
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
fn rerun_failed_jobs_asks_first_in_the_card(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-failed");
    let (mut shell, fake, key, run) = live(cx, &scratch);
    let job = failed(&run);
    open_card(&mut shell, key.number.0);
    right_click(&mut shell, &job_row(&job));
    shell.click("job-menu-rerun-failed");
    assert!(
        confirm_jobs(&mut shell).is_some_and(|jobs| jobs >= 2),
        "it asks first"
    );
    reveal(&mut shell, "ci-retry-confirm");
    assert!(
        fake.retries().is_empty(),
        "nothing is sent before the confirm"
    );
    shell.click_element(SharedString::from("confirm-retry"));
    shell.settle_until("the re-run at the forge", |_| !fake.retries().is_empty());
    assert_eq!(fake.retries(), [format!("run {}", run.id.as_str())]);
}

#[gpui::test]
fn the_job_menu_watches_the_run(cx: &mut TestAppContext) {
    let scratch = Scratch::new("job-menu-watch");
    let (mut shell, _, key, run) = live(cx, &scratch);
    let job = failed(&run);
    open_card(&mut shell, key.number.0);
    let watched = |shell: &mut ShellWindow| {
        shell.update(|_, cx| {
            Subscriptions::global(cx)
                .expect("watching is on")
                .read(cx)
                .is_run_watched(&key, &run.id)
        })
    };
    assert!(!watched(&mut shell));
    right_click(&mut shell, &job_row(&job));
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
