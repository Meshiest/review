//! The forge runtime: one tokio thread that owns every backend. The UI hands
//! it work with [`ForgeHub::spawn`] or [`ForgeHub::request`] and gets a
//! [`Pending`] back at once; it never blocks on it.
//!
//! In gpui, await the [`Pending`] inside `cx.spawn` and update the entity
//! when it resolves; the oneshot it wraps does not need a tokio context to
//! be awaited. Dropping a [`Pending`] cancels the work (a hover prefetch
//! that lost interest); [`Pending::detach`] lets it finish anyway.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use tokio::sync::{OnceCell, Semaphore, mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::auth::CliCredentials;
use crate::avatar::{self, Avatar};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::forgejo::Forgejo;
use crate::github::GitHub;
use crate::gitlab::GitLab;
use crate::http::{Clock, TokioClock};
use crate::http::{HttpClient, HttpOptions, ReqwestTransport, Transport};
use crate::model::{
    CheckRun, CiDetail, CiJob, CiRun, Comment, CommentId, ForgeKind, InboxSection, JobId, Label,
    LogChunk, MergeOptions, MergeSettings, Mergeability, NewComment, PendingComment, PendingReview,
    PrNumber, PullRequest, RepoRef, Review, ReviewId, RunId, Sha, Snapshot, Stack, SuggestionId,
    Thread, ThreadId, User, Verdict, ViewedFile,
};
use crate::watch::{Change, SubscriptionId, WatchEvent, WatchTarget, Watcher};

/// One configured forge account.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeConfig {
    pub kind: ForgeKind,
    /// The web host: `github.com`, `gitlab.com`, `git.example.org`.
    pub host: String,
}

type ConnectFuture = Pin<Box<dyn Future<Output = Result<Arc<dyn Forge>>> + Send>>;
type Connect = Box<dyn Fn() -> ConnectFuture + Send + Sync>;

struct Backend {
    kind: ForgeKind,
    cell: OnceCell<Arc<dyn Forge>>,
    connect: Option<Connect>,
}

impl Backend {
    /// A failed connect is not kept, so the next request tries again (after
    /// `gh auth login`, say).
    async fn get(&self, host: &str) -> Result<Arc<dyn Forge>> {
        let forge = self
            .cell
            .get_or_try_init(|| async {
                match &self.connect {
                    Some(connect) => connect().await,
                    None => Err(Error::UnknownForge(host.to_owned())),
                }
            })
            .await?;
        Ok(forge.clone())
    }
}

/// Avatar fetches that run at once; the rest wait their turn.
const AVATAR_SLOTS: usize = 4;

/// What avatar fetches share: the Gravatar switch and the concurrency limit.
struct AvatarPolicy {
    gravatar: AtomicBool,
    slots: Semaphore,
}

/// Owns the forge thread and a backend per configured forge, keyed by host.
/// Dropping it stops the thread and cancels whatever is still running.
pub struct ForgeHub {
    handle: tokio::runtime::Handle,
    backends: BTreeMap<String, Arc<Backend>>,
    transport: Arc<OnceCell<Arc<dyn Transport>>>,
    avatars: Arc<AvatarPolicy>,
    _stop: oneshot::Sender<()>,
}

impl ForgeHub {
    /// Starts the forge thread and, on it, reads each forge's token from its
    /// CLI and builds the backend. Returns at once; requests made before a
    /// backend is ready wait for it on the forge thread.
    ///
    /// # Errors
    /// [`Error::Shutdown`] when the runtime or its thread cannot be created.
    pub fn start(configs: Vec<ForgeConfig>) -> Result<Self> {
        let shared_transport: Arc<OnceCell<Arc<dyn Transport>>> = Arc::new(OnceCell::new());
        let transport = shared_transport.clone();
        let backends = configs
            .into_iter()
            .map(|config| {
                let transport = transport.clone();
                let kind = config.kind;
                let host = config.host.clone();
                let connect: Connect =
                    Box::new(move || Box::pin(connect(config.clone(), transport.clone())));
                let backend = Backend {
                    kind,
                    cell: OnceCell::new(),
                    connect: Some(connect),
                };
                (host, Arc::new(backend))
            })
            .collect();
        let mut hub = Self::with(backends)?;
        hub.transport = shared_transport;
        for (host, backend) in &hub.backends {
            let (host, backend) = (host.clone(), backend.clone());
            hub.handle.spawn(async move {
                // Warm-up only; a failure resurfaces on the first request.
                let _ = backend.get(&host).await;
            });
        }
        Ok(hub)
    }

    /// A hub over ready-made backends, for tests and fakes.
    ///
    /// # Errors
    /// As [`Self::start`].
    pub fn with_backends(backends: Vec<Arc<dyn Forge>>) -> Result<Self> {
        let backends = backends
            .into_iter()
            .map(|forge| {
                let backend = Backend {
                    kind: forge.kind(),
                    cell: OnceCell::new_with(Some(forge.clone())),
                    connect: None,
                };
                (forge.host().to_owned(), Arc::new(backend))
            })
            .collect();
        Self::with(backends)
    }

    fn with(backends: BTreeMap<String, Arc<Backend>>) -> Result<Self> {
        // Creating a runtime makes no disk, network or process calls, so it
        // is fine on the caller's (UI) thread; everything else runs on it.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Error::Shutdown)?;
        let handle = runtime.handle().clone();
        let (stop, stopped) = oneshot::channel::<()>();
        std::thread::Builder::new()
            .name("forge".to_owned())
            .spawn(move || {
                runtime.block_on(async {
                    let _ = stopped.await;
                });
            })
            .map_err(|_| Error::Shutdown)?;
        Ok(Self {
            handle,
            backends,
            transport: Arc::new(OnceCell::new()),
            avatars: Arc::new(AvatarPolicy {
                gravatar: AtomicBool::new(false),
                slots: Semaphore::new(AVATAR_SLOTS),
            }),
            _stop: stop,
        })
    }

    /// Lets avatar fetches use `transport` instead of the network, for tests.
    #[must_use]
    pub fn with_avatar_transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Arc::new(OnceCell::new_with(Some(transport)));
        self
    }

    /// Whether Gravatar images may be fetched (they come from a third
    /// party, so they are off until the user turns them on).
    pub fn set_allow_gravatar(&self, allow: bool) {
        self.avatars.gravatar.store(allow, Ordering::Relaxed);
    }

    /// Fetches and decodes the avatar at `url` for an account on the forge
    /// at `host`, on the forge thread. Only that forge's own host, its
    /// avatar CDN and (when allowed) Gravatar are fetched; any other URL
    /// fails at once without a request. No credential is sent.
    pub fn avatar(&self, host: &str, url: &str) -> Pending<Avatar> {
        let (reply, rx) = oneshot::channel();
        let kind = self.backends.get(host).map(|backend| backend.kind);
        let (host, url) = (host.to_owned(), url.to_owned());
        let (transport, policy) = (self.transport.clone(), self.avatars.clone());
        let task = self.handle.spawn(async move {
            let result = match kind {
                Some(kind) => fetch_avatar(&transport, &policy, kind, &host, &url).await,
                None => Err(Error::UnknownForge(host)),
            };
            let _ = reply.send(result);
        });
        Pending {
            rx,
            abort: Some(task.abort_handle()),
        }
    }

    /// The configured forges, by host.
    pub fn forges(&self) -> impl Iterator<Item = (ForgeKind, &str)> {
        self.backends
            .iter()
            .map(|(host, backend)| (backend.kind, host.as_str()))
    }

    /// Runs `op` against the backend for `host` on the forge thread.
    ///
    /// ```no_run
    /// # use forge::{ForgeHub, model::{PrNumber, RepoRef}};
    /// # fn demo(hub: &ForgeHub, repo: RepoRef, number: PrNumber) {
    /// let host = repo.host.clone();
    /// let pending = hub.spawn(&host, move |forge| async move {
    ///     forge.threads(&repo, number).await
    /// });
    /// # pending.detach(); }
    /// ```
    pub fn spawn<T, F, Fut>(&self, host: &str, op: F) -> Pending<T>
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn Forge>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let (reply, rx) = oneshot::channel();
        let backend = self.backends.get(host).cloned();
        let host = host.to_owned();
        let task = self.handle.spawn(async move {
            let result = match backend {
                Some(backend) => match backend.get(&host).await {
                    Ok(forge) => op(forge).await,
                    Err(err) => Err(err),
                },
                None => Err(Error::UnknownForge(host)),
            };
            // The receiver may be gone (canceled); nothing to do then.
            let _ = reply.send(result);
        });
        Pending {
            rx,
            abort: Some(task.abort_handle()),
        }
    }

    /// [`Self::spawn`] with a [`Request`] value, for callers that queue
    /// requests as data.
    pub fn request(&self, host: &str, request: Request) -> Pending<Response> {
        self.spawn(host, move |forge| async move {
            request.run(forge.as_ref()).await
        })
    }

    /// Starts a [`Watcher`] on the forge thread. Subscribe through the
    /// handle; changes arrive on the receiver (drain it from the UI; it is
    /// runtime-agnostic). Dropping the handle stops the watcher.
    pub fn watch(&self) -> (WatchHandle, mpsc::UnboundedReceiver<Change>) {
        let (commands, inbox) = mpsc::unbounded_channel();
        let (events, changes) = mpsc::unbounded_channel();
        let backends = self.backends.clone();
        self.handle.spawn(watch_loop(
            backends,
            inbox,
            events,
            Arc::new(TokioClock::new()),
        ));
        let handle = WatchHandle {
            commands,
            next_id: Arc::new(AtomicU64::new(1)),
        };
        (handle, changes)
    }
}

enum WatchCommand {
    Subscribe(SubscriptionId, WatchTarget),
    Unsubscribe(SubscriptionId),
}

/// Subscribes to and unsubscribes from the hub's watcher. Cheap to clone;
/// the watcher stops when the last clone is dropped.
#[derive(Clone)]
pub struct WatchHandle {
    commands: mpsc::UnboundedSender<WatchCommand>,
    next_id: Arc<AtomicU64>,
}

impl WatchHandle {
    /// Follows `target` on the forge of its repository's host. The id comes
    /// back at once; the first poll only records a baseline. An unknown
    /// host or a forge that cannot connect arrives as
    /// [`WatchEvent::PollFailed`].
    pub fn subscribe(&self, target: WatchTarget) -> SubscriptionId {
        let id = SubscriptionId(self.next_id.fetch_add(1, Ordering::Relaxed));
        // A send only fails once the forge thread is gone; nothing to watch then.
        let _ = self.commands.send(WatchCommand::Subscribe(id, target));
        id
    }

    pub fn unsubscribe(&self, id: SubscriptionId) {
        let _ = self.commands.send(WatchCommand::Unsubscribe(id));
    }
}

async fn watch_loop(
    backends: BTreeMap<String, Arc<Backend>>,
    mut commands: mpsc::UnboundedReceiver<WatchCommand>,
    events: mpsc::UnboundedSender<Change>,
    clock: Arc<dyn Clock>,
) {
    let mut watcher = Watcher::new(clock.clone());
    loop {
        let wait = watcher
            .next_due()
            .map(|due| due.saturating_sub(clock.now()));
        let sleep = async {
            match wait {
                Some(wait) => clock.sleep(wait).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            command = commands.recv() => match command {
                None => return,
                Some(WatchCommand::Unsubscribe(id)) => {
                    watcher.unsubscribe(id);
                }
                Some(WatchCommand::Subscribe(id, target)) => {
                    let host = target.repo().host.clone();
                    let forge = match backends.get(&host) {
                        Some(backend) => backend.get(&host).await,
                        None => Err(Error::UnknownForge(host)),
                    };
                    match forge {
                        Ok(forge) => watcher.insert(id, forge, target),
                        Err(error) => {
                            let change = Change {
                                subscription: id,
                                target,
                                event: WatchEvent::PollFailed(error),
                            };
                            if events.send(change).is_err() {
                                return;
                            }
                        }
                    }
                }
            },
            () = sleep => {
                for change in watcher.poll_due().await {
                    if events.send(change).is_err() {
                        return;
                    }
                }
            }
        }
    }
}

/// A result that will arrive from the forge thread.
#[must_use = "dropping a Pending cancels its request"]
pub struct Pending<T> {
    rx: oneshot::Receiver<Result<T>>,
    abort: Option<AbortHandle>,
}

impl<T> Pending<T> {
    /// The result if it has arrived, without waiting.
    pub fn try_take(&mut self) -> Option<Result<T>> {
        match self.rx.try_recv() {
            Ok(result) => Some(result),
            Err(oneshot::error::TryRecvError::Empty) => None,
            Err(oneshot::error::TryRecvError::Closed) => Some(Err(Error::Shutdown)),
        }
    }

    /// Lets the request run to completion even though nobody waits for it
    /// (to warm a cache, say).
    pub fn detach(mut self) {
        self.abort = None;
    }
}

impl<T> Future for Pending<T> {
    type Output = Result<T>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.rx)
            .poll(cx)
            .map(|received| received.unwrap_or(Err(Error::Shutdown)))
    }
}

impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        if let Some(abort) = self.abort.take() {
            abort.abort();
        }
    }
}

/// The [`Forge`] operations as data, reads then writes. A write variant
/// carries the target repository and pull request, like its method.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    WhoAmI,
    Capabilities,
    Inbox(InboxSection),
    PullRequest(RepoRef, PrNumber),
    Threads(RepoRef, PrNumber),
    Checks(RepoRef, PrNumber),
    Stack(RepoRef, PrNumber),
    ViewedFiles(RepoRef, PrNumber),
    Snapshot(RepoRef, PrNumber),
    PendingReview(RepoRef, PrNumber),
    Mergeability(RepoRef, PrNumber),
    MergeMethods(RepoRef),

    StartReview(RepoRef, PrNumber, Sha),
    AddReviewComment(RepoRef, PrNumber, ReviewId, NewComment),
    DeleteReviewComment(RepoRef, PrNumber, CommentId),
    EditReviewComment(RepoRef, PrNumber, Box<PendingComment>, String),
    SubmitReview(RepoRef, PrNumber, ReviewId, Verdict, String),
    DiscardReview(RepoRef, PrNumber, ReviewId),
    Reply(RepoRef, PrNumber, ThreadId, String),
    SetResolved(RepoRef, PrNumber, ThreadId, bool),
    AddComment(RepoRef, PrNumber, String),
    SetViewed(RepoRef, PrNumber, String, bool),
    ApplySuggestions(RepoRef, PrNumber, Vec<SuggestionId>),
    RetryJob(RepoRef, JobId),
    RetryFailedJobs(RepoRef, RunId),
    Merge(RepoRef, PrNumber, Box<MergeOptions>),
    /// `None` turns auto-merge off.
    SetAutoMerge(RepoRef, PrNumber, Option<Box<MergeOptions>>),
    Close(RepoRef, PrNumber),
    Reopen(RepoRef, PrNumber),
    SetDraft(RepoRef, PrNumber, bool),

    /// The repository, and the prefix to search for.
    Collaborators(RepoRef, String),
    Labels(RepoRef),
    /// Users, then teams.
    RequestReviewers(RepoRef, PrNumber, Vec<User>, Vec<User>),
    RemoveReviewers(RepoRef, PrNumber, Vec<User>, Vec<User>),
    SetAssignees(RepoRef, PrNumber, Vec<User>),
    /// By name.
    SetLabels(RepoRef, PrNumber, Vec<String>),

    CiRuns(RepoRef, Sha),
    CiRun(RepoRef, RunId, CiDetail),
    CiJob(RepoRef, JobId),
    /// The log from a byte offset.
    JobLog(RepoRef, JobId, u64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    User(User),
    Capabilities(Capabilities),
    PullRequests(Vec<PullRequest>),
    PullRequest(Box<PullRequest>),
    Threads(Vec<Thread>),
    Checks(Vec<CheckRun>),
    Stack(Stack),
    ViewedFiles(Vec<ViewedFile>),
    Snapshot(Box<Snapshot>),
    /// `StartReview` (no comments yet) and `PendingReview`.
    PendingReview(Option<Box<PendingReview>>),
    PendingComment(Box<PendingComment>),
    Review(Box<Review>),
    /// `Reply` and `AddComment`.
    Comment(Box<Comment>),
    /// `SetResolved`: the thread as it now reads.
    Thread(Box<Thread>),
    ViewedFile(ViewedFile),
    Mergeability(Mergeability),
    MergeMethods(MergeSettings),
    /// A write with nothing to return: deletes, discards, applies, re-runs.
    Done,
    CiRuns(Vec<CiRun>),
    CiRun(Box<CiRun>),
    CiJob(Box<CiJob>),
    LogChunk(LogChunk),
    /// `Collaborators`.
    Users(Vec<User>),
    /// `Labels`.
    Labels(Vec<Label>),
}

impl Request {
    async fn run(self, forge: &dyn Forge) -> Result<Response> {
        Ok(match self {
            Self::WhoAmI => Response::User(forge.whoami().await?),
            Self::Capabilities => Response::Capabilities(forge.capabilities()),
            Self::Inbox(section) => Response::PullRequests(forge.inbox(&section).await?),
            Self::PullRequest(repo, n) => {
                Response::PullRequest(Box::new(forge.pull_request(&repo, n).await?))
            }
            Self::Threads(repo, n) => Response::Threads(forge.threads(&repo, n).await?),
            Self::Checks(repo, n) => Response::Checks(forge.checks(&repo, n).await?),
            Self::Stack(repo, n) => Response::Stack(forge.stack(&repo, n).await?),
            Self::ViewedFiles(repo, n) => {
                Response::ViewedFiles(forge.viewed_files(&repo, n).await?)
            }
            Self::Snapshot(repo, n) => {
                Response::Snapshot(Box::new(forge.snapshot(&repo, n).await?))
            }
            Self::PendingReview(repo, n) => {
                Response::PendingReview(forge.pending_review(&repo, n).await?.map(Box::new))
            }
            Self::StartReview(repo, n, commit) => Response::PendingReview(Some(Box::new(
                forge.start_review(&repo, n, &commit).await?,
            ))),
            Self::AddReviewComment(repo, n, review, comment) => Response::PendingComment(Box::new(
                forge
                    .add_review_comment(&repo, n, &review, &comment)
                    .await?,
            )),
            Self::DeleteReviewComment(repo, n, comment) => {
                forge.delete_review_comment(&repo, n, &comment).await?;
                Response::Done
            }
            Self::EditReviewComment(repo, n, comment, body) => Response::PendingComment(Box::new(
                forge.edit_review_comment(&repo, n, &comment, &body).await?,
            )),
            Self::SubmitReview(repo, n, review, verdict, body) => Response::Review(Box::new(
                forge
                    .submit_review(&repo, n, &review, verdict, &body)
                    .await?,
            )),
            Self::DiscardReview(repo, n, review) => {
                forge.discard_review(&repo, n, &review).await?;
                Response::Done
            }
            Self::Reply(repo, n, thread, body) => {
                Response::Comment(Box::new(forge.reply(&repo, n, &thread, &body).await?))
            }
            Self::SetResolved(repo, n, thread, resolved) => Response::Thread(Box::new(
                forge.set_resolved(&repo, n, &thread, resolved).await?,
            )),
            Self::AddComment(repo, n, body) => {
                Response::Comment(Box::new(forge.add_comment(&repo, n, &body).await?))
            }
            Self::SetViewed(repo, n, path, viewed) => {
                Response::ViewedFile(forge.set_viewed(&repo, n, &path, viewed).await?)
            }
            Self::ApplySuggestions(repo, n, ids) => {
                forge.apply_suggestions(&repo, n, &ids).await?;
                Response::Done
            }
            Self::RetryJob(repo, job) => {
                forge.retry_job(&repo, &job).await?;
                Response::Done
            }
            Self::RetryFailedJobs(repo, run) => {
                forge.retry_failed_jobs(&repo, &run).await?;
                Response::Done
            }
            Self::Mergeability(repo, n) => {
                Response::Mergeability(forge.mergeability(&repo, n).await?)
            }
            Self::MergeMethods(repo) => Response::MergeMethods(forge.merge_methods(&repo).await?),
            Self::Merge(repo, n, options) => {
                Response::PullRequest(Box::new(forge.merge(&repo, n, &options).await?))
            }
            Self::SetAutoMerge(repo, n, options) => Response::PullRequest(Box::new(
                forge.set_auto_merge(&repo, n, options.as_deref()).await?,
            )),
            Self::Close(repo, n) => Response::PullRequest(Box::new(forge.close(&repo, n).await?)),
            Self::Reopen(repo, n) => Response::PullRequest(Box::new(forge.reopen(&repo, n).await?)),
            Self::SetDraft(repo, n, draft) => {
                Response::PullRequest(Box::new(forge.set_draft(&repo, n, draft).await?))
            }
            Self::Collaborators(repo, query) => {
                Response::Users(forge.collaborators(&repo, &query).await?)
            }
            Self::Labels(repo) => Response::Labels(forge.labels(&repo).await?),
            Self::RequestReviewers(repo, n, users, teams) => Response::PullRequest(Box::new(
                forge.request_reviewers(&repo, n, &users, &teams).await?,
            )),
            Self::RemoveReviewers(repo, n, users, teams) => Response::PullRequest(Box::new(
                forge.remove_reviewers(&repo, n, &users, &teams).await?,
            )),
            Self::SetAssignees(repo, n, users) => {
                Response::PullRequest(Box::new(forge.set_assignees(&repo, n, &users).await?))
            }
            Self::SetLabels(repo, n, labels) => {
                Response::PullRequest(Box::new(forge.set_labels(&repo, n, &labels).await?))
            }
            Self::CiRuns(repo, sha) => Response::CiRuns(forge.ci_runs(&repo, &sha).await?),
            Self::CiRun(repo, run, detail) => {
                Response::CiRun(Box::new(forge.ci_run(&repo, &run, detail).await?))
            }
            Self::CiJob(repo, job) => Response::CiJob(Box::new(forge.ci_job(&repo, &job).await?)),
            Self::JobLog(repo, job, from) => {
                Response::LogChunk(forge.job_log(&repo, &job, from).await?)
            }
        })
    }
}

/// The one transport every backend and avatar fetch shares, built on first use.
async fn shared_transport(cell: &OnceCell<Arc<dyn Transport>>) -> Result<Arc<dyn Transport>> {
    // Built on the forge thread: loading the TLS roots reads the OS store.
    let transport = cell
        .get_or_try_init(|| async {
            ReqwestTransport::new().map(|t| Arc::new(t) as Arc<dyn Transport>)
        })
        .await?;
    Ok(transport.clone())
}

async fn fetch_avatar(
    transport: &OnceCell<Arc<dyn Transport>>,
    policy: &AvatarPolicy,
    kind: ForgeKind,
    host: &str,
    url: &str,
) -> Result<Avatar> {
    let gravatar = policy.gravatar.load(Ordering::Relaxed);
    // A refused URL fails here, before the transport is even built.
    avatar::check(kind, host, url, gravatar)?;
    let _slot = policy.slots.acquire().await.map_err(|_| Error::Shutdown)?;
    let transport = shared_transport(transport).await?;
    avatar::fetch(transport.as_ref(), kind, host, url, gravatar).await
}

async fn connect(
    config: ForgeConfig,
    transport: Arc<OnceCell<Arc<dyn Transport>>>,
) -> Result<Arc<dyn Forge>> {
    let transport = shared_transport(&transport).await?;
    let ForgeConfig { kind, host } = config;
    let credentials = Arc::new(CliCredentials {
        kind,
        host: host.clone(),
    });
    let http = HttpClient::new(
        kind,
        host,
        transport,
        credentials,
        HttpOptions::for_kind(kind),
    );
    http.authorize().await?;
    Ok(match kind {
        ForgeKind::GitHub => Arc::new(GitHub::connect(http).await?),
        ForgeKind::GitLab => Arc::new(GitLab::connect(http).await?),
        ForgeKind::Forgejo => Arc::new(Forgejo::connect(http).await?),
    })
}
