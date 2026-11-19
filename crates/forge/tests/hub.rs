mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use forge::model::{
    CheckRun, ForgeKind, InboxSection, JobId, MergeMethod, MergeOptions, PrNumber, PullRequest,
    RepoRef, Sha, Stack, SuggestionId, Thread, User,
};
use forge::watch::{WatchEvent, WatchTarget};
use forge::{Capabilities, Error, Forge, ForgeHub, Request, Response, Result};
use tokio::sync::Notify;

/// Answers from memory. `threads` parks until notified, to test cancellation.
struct Fake {
    release: Arc<Notify>,
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[async_trait]
impl Forge for Fake {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        "github.com"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::GITHUB
    }

    async fn whoami(&self) -> Result<User> {
        Ok(User::new("cora"))
    }

    async fn inbox(&self, _section: &InboxSection) -> Result<Vec<PullRequest>> {
        Ok(vec![common::pr(1, "a", "main")])
    }

    async fn pull_request(&self, _repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        Ok(common::pr(number.0, "a", "main"))
    }

    async fn threads(&self, _repo: &RepoRef, _number: PrNumber) -> Result<Vec<Thread>> {
        let _guard = SetOnDrop(self.dropped.clone());
        self.started.store(true, Ordering::SeqCst);
        self.release.notified().await;
        Ok(Vec::new())
    }

    async fn checks(&self, _repo: &RepoRef, _number: PrNumber) -> Result<Vec<CheckRun>> {
        Ok(Vec::new())
    }

    async fn stack(&self, _repo: &RepoRef, number: PrNumber) -> Result<Stack> {
        let pr = common::pr(number.0, "a", "main");
        Ok(forge::stack_by_base_branch(&pr, &[]))
    }
}

struct Harness {
    hub: ForgeHub,
    release: Arc<Notify>,
    started: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

fn hub() -> Harness {
    let release = Arc::new(Notify::new());
    let started = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));
    let fake = Fake {
        release: release.clone(),
        started: started.clone(),
        dropped: dropped.clone(),
    };
    Harness {
        hub: ForgeHub::with_backends(vec![Arc::new(fake)]).unwrap(),
        release,
        started,
        dropped,
    }
}

/// Polls with request round trips through the forge thread, not sleeps.
fn until(hub: &ForgeHub, flag: &AtomicBool) -> bool {
    for _ in 0..1000 {
        if flag.load(Ordering::SeqCst) {
            return true;
        }
        wait(hub.request("github.com", Request::Capabilities)).unwrap();
    }
    false
}

/// The test's own executor, standing in for gpui's: the result arrives
/// from the forge thread.
fn wait<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

#[test]
fn requests_resolve_off_the_calling_thread() {
    let hub = hub().hub;
    assert_eq!(
        hub.forges().collect::<Vec<_>>(),
        [(ForgeKind::GitHub, "github.com")]
    );
    let me = wait(hub.request("github.com", Request::WhoAmI)).unwrap();
    assert_eq!(me, Response::User(User::new("cora")));
    let repo = common::repo();
    let pr = wait(hub.spawn("github.com", move |forge| async move {
        forge.pull_request(&repo, PrNumber(7)).await
    }))
    .unwrap();
    assert_eq!(pr.number, PrNumber(7));
}

#[test]
fn unknown_hosts_and_unsupported_features_are_errors() {
    let hub = hub().hub;
    let err = wait(hub.request("gitlab.com", Request::WhoAmI)).unwrap_err();
    assert_eq!(err, Error::UnknownForge("gitlab.com".to_owned()));
    let err = wait(hub.request(
        "github.com",
        Request::ViewedFiles(common::repo(), PrNumber(1)),
    ))
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
}

#[test]
fn writes_a_backend_lacks_answer_with_its_capability() {
    let hub = hub().hub;
    let (repo, n) = (common::repo(), PrNumber(1));
    let err = wait(hub.request(
        "github.com",
        Request::AddComment(repo.clone(), n, "hi".to_owned()),
    ))
    .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    let err = wait(hub.request(
        "github.com",
        Request::ApplySuggestions(repo, n, vec![SuggestionId::new("1")]),
    ))
    .unwrap_err();
    assert_eq!(
        err,
        Error::ApplyLocally {
            kind: ForgeKind::GitHub
        }
    );
}

#[test]
fn lifecycle_requests_a_backend_lacks_are_unsupported() {
    let hub = hub().hub;
    let (repo, n) = (common::repo(), PrNumber(1));
    let options = Box::new(MergeOptions::new(
        MergeMethod::Squash,
        Sha::new("a".repeat(40)),
    ));
    for request in [
        Request::Mergeability(repo.clone(), n),
        Request::MergeMethods(repo.clone()),
        Request::Merge(repo.clone(), n, options.clone()),
        Request::SetAutoMerge(repo.clone(), n, Some(options)),
        Request::SetAutoMerge(repo.clone(), n, None),
        Request::Close(repo.clone(), n),
        Request::Reopen(repo.clone(), n),
        Request::SetDraft(repo.clone(), n, true),
    ] {
        let err = wait(hub.request("github.com", request.clone())).unwrap_err();
        assert!(
            matches!(err, Error::Unsupported { .. }),
            "{request:?}: {err:?}"
        );
    }
}

#[test]
fn dropping_a_pending_cancels_it() {
    let h = hub();
    let pending = h
        .hub
        .request("github.com", Request::Threads(common::repo(), PrNumber(1)));
    assert!(until(&h.hub, &h.started), "the request never started");
    drop(pending);
    assert!(
        until(&h.hub, &h.dropped),
        "the canceled request kept running"
    );
}

#[test]
fn try_take_does_not_block() {
    let h = hub();
    let mut pending = h
        .hub
        .request("github.com", Request::Threads(common::repo(), PrNumber(1)));
    assert!(pending.try_take().is_none());
    h.release.notify_one();
    assert_eq!(wait(pending).unwrap(), Response::Threads(Vec::new()));
}

#[test]
fn ci_reads_a_backend_lacks_are_unsupported() {
    let hub = hub().hub;
    let request = Request::JobLog(common::repo(), JobId::new("1"), 0);
    let err = wait(hub.request("github.com", request)).unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
}

#[test]
fn the_watcher_reports_a_host_it_has_no_forge_for() {
    let hub = hub().hub;
    let (watch, mut changes) = hub.watch();
    let target = WatchTarget::PullRequest {
        repo: RepoRef::new("gitlab.com", "o", "r"),
        number: PrNumber(1),
    };
    let id = watch.subscribe(target.clone());
    let change = wait(changes.recv()).expect("the watcher is running");
    assert_eq!((change.subscription, change.target), (id, target));
    assert!(
        matches!(change.event, WatchEvent::PollFailed(Error::UnknownForge(_))),
        "{:?}",
        change.event
    );
    drop(watch);
    assert!(
        wait(changes.recv()).is_none(),
        "dropping the handle stops it"
    );
}
