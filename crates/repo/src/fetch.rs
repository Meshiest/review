//! Fetching a pull request's head and its base branch into `refs/review/*`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use gix::ObjectId;

use crate::error::{Error, Result};
use crate::git::GitCommand;
use crate::store::{Store, pr_base_ref, pr_head_ref};

/// Where a forge publishes pull request heads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrRefs {
    /// `refs/pull/N/head`: GitHub and Forgejo.
    Pull,
    /// `refs/merge-requests/N/head`: GitLab.
    MergeRequests,
}

impl PrRefs {
    #[must_use]
    pub fn head_source(self, number: u64) -> String {
        match self {
            Self::Pull => format!("refs/pull/{number}/head"),
            Self::MergeRequests => format!("refs/merge-requests/{number}/head"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FetchRequest {
    /// Remote URL (or path) to fetch from. Fetching by URL leaves the
    /// clone's configured remotes and tracking branches alone.
    pub remote_url: String,
    pub refs: PrRefs,
    pub number: u64,
    pub base_branch: String,
    pub timeout: Duration,
}

/// What a fetch reports on its channel.
#[derive(Debug)]
pub enum FetchEvent {
    /// One line of git's progress output ("Receiving objects: 40% ...").
    Progress(String),
    /// The last event: the fetched commits, or why it failed.
    Finished(Result<Fetched>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fetched {
    pub head: ObjectId,
    pub base: ObjectId,
}

/// A running fetch. Poll `events` from the UI loop; it never blocks the
/// caller. Dropping the handle does not stop the fetch; call [`cancel`](Self::cancel).
pub struct FetchHandle {
    pub events: Receiver<FetchEvent>,
    cancel: Arc<AtomicBool>,
}

impl FetchHandle {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Store {
    /// Starts `git fetch` on a background thread: the pull request head into
    /// `refs/review/pr/N` and the base branch into `refs/review/base/N`.
    ///
    /// # Errors
    /// If `request` has an unusable URL or branch name. Fetch failures come
    /// through the handle's channel.
    pub fn fetch_pr(&self, request: &FetchRequest) -> Result<FetchHandle> {
        let args = fetch_args(request)?;
        let (tx, rx) = mpsc::channel();
        // Canceling the thread's work cancels the fetch too.
        let cancel = crate::cancel::current().unwrap_or_default();
        let store = self.clone();
        let command = GitCommand::new(self.git_dir(), request.timeout)
            .args(args)
            .cancel_on(Arc::clone(&cancel))
            .on_stderr_line({
                let tx = tx.clone();
                Box::new(move |line| {
                    let _ = tx.send(FetchEvent::Progress(line.to_owned()));
                })
            });
        let number = request.number;
        thread::Builder::new()
            .name(format!("fetch-pr-{number}"))
            .spawn(move || {
                let result = command.run_checked().and_then(|_| store.fetched(number));
                let _ = tx.send(FetchEvent::Finished(result));
            })?;
        Ok(FetchHandle { events: rx, cancel })
    }

    fn fetched(&self, number: u64) -> Result<Fetched> {
        Ok(Fetched {
            head: self.resolve_commit(&pr_head_ref(number))?,
            base: self.resolve_commit(&pr_base_ref(number))?,
        })
    }
}

fn fetch_args(request: &FetchRequest) -> Result<Vec<String>> {
    let url = request.remote_url.trim();
    if url.is_empty() || url.starts_with('-') || url.contains(char::is_control) {
        return Err(Error::InvalidArgument(format!(
            "unusable remote url `{}`",
            request.remote_url
        )));
    }
    let base_ref = format!("refs/heads/{}", request.base_branch);
    if request.base_branch.starts_with('-')
        || gix::validate::reference::name(base_ref.as_str().into()).is_err()
    {
        return Err(Error::InvalidArgument(format!(
            "unusable branch name `{}`",
            request.base_branch
        )));
    }
    let number = request.number;
    Ok([
        // Private refs only: no FETCH_HEAD, tags, submodule recursion or
        // background maintenance in a clone the user owns.
        "-c",
        "gc.auto=0",
        "-c",
        "maintenance.auto=false",
        "fetch",
        "--progress",
        "--no-tags",
        "--no-write-fetch-head",
        "--no-recurse-submodules",
        "--",
    ]
    .into_iter()
    .map(str::to_owned)
    .chain([
        url.to_owned(),
        format!(
            "+{}:{}",
            request.refs.head_source(number),
            pr_head_ref(number)
        ),
        format!("+{base_ref}:{}", pr_base_ref(number)),
    ])
    .collect())
}
