//! The right-click menu of a CI job in the Conversation's pipelines card.
//! Re-runs and watching go through the session's and the subscriptions' own
//! paths, so a re-run of several jobs asks first as the card's button does.

use forge::model::{CiJob, CiRun, ForgeKind, JobId};
use gpui::App;
use gpui_kit::component::menu::PopupMenu;

use crate::ci::{self, Retry};
use crate::model::PrKey;
use crate::session::Session;
use crate::subscriptions::Subscriptions;
use crate::threads::comment_menu::copy;
use crate::threads::thread_menu::entry_item;
use crate::untrusted::is_web_url;

/// What a job menu entry does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobAction {
    ViewLog,
    RerunJob,
    RerunFailed,
    /// `true` starts watching the job's run, `false` stops.
    Watch(bool),
    CopyLink,
    OpenOnForge,
}

impl JobAction {
    /// The entry's `debug_selector`, after `job-menu-`.
    fn slug(self) -> &'static str {
        match self {
            Self::ViewLog => "view-log",
            Self::RerunJob => "rerun-job",
            Self::RerunFailed => "rerun-failed",
            Self::Watch(true) => "watch",
            Self::Watch(false) => "unwatch",
            Self::CopyLink => "copy-link",
            Self::OpenOnForge => "open-on-forge",
        }
    }
}

/// One entry of the menu; a blocked one is shown disabled with why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobEntry {
    pub action: JobAction,
    pub label: &'static str,
    pub blocked: Option<String>,
}

impl JobEntry {
    fn new(action: JobAction, label: &'static str, blocked: Option<String>) -> Self {
        Self {
            action,
            label,
            blocked,
        }
    }
}

/// The job a menu is for, read again from the session when it opens. A
/// check of the rollup with no job behind it has `None`.
#[derive(Clone, Debug)]
pub struct JobMenu {
    pub key: PrKey,
    pub job: Option<JobId>,
}

/// The job and its run, as the session has them now.
struct Found {
    forge: ForgeKind,
    run: CiRun,
    job: CiJob,
}

impl JobMenu {
    #[must_use]
    pub fn new(key: PrKey, job: Option<JobId>) -> Self {
        Self { key, job }
    }

    fn find(&self, cx: &App) -> Option<Found> {
        let id = self.job.as_ref()?;
        let open = Session::global(cx).read(cx).pr(&self.key)?;
        open.ci.runs.iter().find_map(|run| {
            let job = run.jobs.iter().find(|job| &job.id == id)?;
            Some(Found {
                forge: open.pr.forge,
                run: run.clone(),
                job: job.clone(),
            })
        })
    }

    /// The job's page on the forge, or its run's.
    #[must_use]
    pub fn link(&self, cx: &App) -> Option<String> {
        let found = self.find(cx)?;
        [found.job.url, found.run.url]
            .into_iter()
            .flatten()
            .find(|url| is_web_url(url))
    }

    /// The entries in groups: its log, re-runs and watching, its link.
    #[must_use]
    pub fn entries(&self, cx: &App) -> Vec<Vec<JobEntry>> {
        use JobAction as A;

        let Some(found) = self.find(cx) else {
            let why = || Some("No job behind this check: the forge lists none".to_owned());
            return vec![
                vec![JobEntry::new(A::ViewLog, "View log", why())],
                vec![
                    JobEntry::new(A::RerunJob, "Re-run job", why()),
                    JobEntry::new(A::RerunFailed, "Re-run failed jobs", why()),
                    JobEntry::new(A::Watch(true), "Watch run", why()),
                ],
                vec![
                    JobEntry::new(A::CopyLink, "Copy link", why()),
                    JobEntry::new(A::OpenOnForge, "Open on forge", why()),
                ],
            ];
        };
        let caps = ci::CiActions::of(
            &Session::global(cx)
                .read(cx)
                .capabilities_of(&self.key, found.forge),
        );
        let forge = found.forge.label();
        let job = &found.job;
        let log = if !caps.logs {
            Some(format!(
                "{forge} has no job logs here: open it on the forge"
            ))
        } else if job.started_at.is_none() {
            Some("It has not started yet".to_owned())
        } else {
            None
        };
        let no_retry = || {
            (!caps.retry)
                .then(|| format!("{forge} cannot re-run jobs from here: open it on the forge"))
        };
        let rerun = no_retry().or_else(|| {
            job.state
                .is_active()
                .then(|| "It has not finished yet".to_owned())
        });
        let failed = no_retry().or_else(|| {
            (ci::failed_jobs(&found.run).count() == 0)
                .then(|| "No job of its run failed".to_owned())
        });
        let subscriptions = Subscriptions::global(cx);
        let watched = subscriptions
            .as_ref()
            .is_some_and(|s| s.read(cx).is_run_watched(&self.key, &found.run.id));
        let watch_blocked = subscriptions
            .is_none()
            .then(|| "Watching is off in this session".to_owned());
        let no_link = self
            .link(cx)
            .is_none()
            .then(|| "The forge gave no address for it".to_owned());
        vec![
            vec![JobEntry::new(A::ViewLog, "View log", log)],
            vec![
                JobEntry::new(A::RerunJob, "Re-run job", rerun),
                JobEntry::new(A::RerunFailed, "Re-run failed jobs", failed),
                if watched {
                    JobEntry::new(A::Watch(false), "Unwatch run", watch_blocked)
                } else {
                    JobEntry::new(A::Watch(true), "Watch run", watch_blocked)
                },
            ],
            vec![
                JobEntry::new(A::CopyLink, "Copy link", no_link.clone()),
                JobEntry::new(A::OpenOnForge, "Open on forge", no_link),
            ],
        ]
    }

    /// Does what `action` says, on this menu's pull request.
    pub fn run(&self, action: JobAction, cx: &mut App) {
        let session = Session::global(cx);
        let Some(found) = self.find(cx) else {
            return;
        };
        let (key, run, job) = (&self.key, found.run.id, found.job.id);
        match action {
            JobAction::ViewLog => session.update(cx, |session, cx| {
                session.focus(key, cx);
                session.open_log(&job, None, cx);
            }),
            JobAction::RerunJob => session.update(cx, |session, cx| {
                session.focus(key, cx);
                session.request_retry(Retry::Job { run, job }, cx);
            }),
            JobAction::RerunFailed => session.update(cx, |session, cx| {
                session.focus(key, cx);
                session.request_retry(Retry::FailedJobs { run }, cx);
            }),
            JobAction::Watch(_) => {
                if let Some(subscriptions) = Subscriptions::global(cx) {
                    subscriptions.update(cx, |s, cx| s.toggle_run(key, &run, cx));
                }
            }
            JobAction::CopyLink => {
                if let Some(url) = self.link(cx) {
                    copy(&url, cx);
                }
            }
            JobAction::OpenOnForge => {
                if let Some(url) = self.link(cx) {
                    cx.open_url(&url);
                }
            }
        }
    }

    /// The menu, built when it opens.
    pub fn popup(&self, menu: PopupMenu, cx: &App) -> PopupMenu {
        self.entries(cx)
            .into_iter()
            .enumerate()
            .fold(menu, |menu, (ix, group)| {
                let menu = if ix > 0 { menu.separator() } else { menu };
                group.into_iter().fold(menu, |menu, entry| {
                    let this = self.clone();
                    let action = entry.action;
                    menu.item(entry_item(
                        format!("job-menu-{}", action.slug()),
                        entry.label,
                        entry.blocked,
                        move |cx| this.run(action, cx),
                    ))
                })
            })
    }
}
