//! State shared by the sidebar, the shell and every dock panel: the inbox,
//! the pull requests that are open and everything loaded for each, which of
//! them has the focus, and what the user has written on them.
//!
//! Several pull requests can be open at once, each with its own panels. The
//! focused one is the one the sidebar, the Log panel and the keyboard act
//! on; a panel bound to a pull request draws that one whatever has focus.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use diff::Layout;
use forge::Capabilities;
use forge::model::{
    CiJob, CiRun, CiState, ForgeKind, JobId, RepoRef, RunId, Sha, Snapshot, Thread, ThreadId,
    ViewedState,
};
use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{
    App, AppContext as _, Context, Entity, EntityId, EventEmitter, Global, SharedString, Task,
};
use store::DraftAnchor;

use forge::model::Timestamp;

use crate::ci::{self, CiLoad, Retry, RetryConfirm};
use crate::ci_settle::{self, SETTLE_POLL, SettleRead};
use crate::commits::{self, CommitView, PrCommit, PrCommits};
use crate::drafts::DraftSaver;
use crate::fixtures::{self, Fixtures};
use crate::forge_sink::ForgeSink;
use crate::lifecycle::Lifecycle;
use crate::model::{ChangedFile, Check, Inbox, PrKey, PullRequest, ReviewState, Stack, Viewed};
use crate::opener::{self, DiffUpdate, OpenRequest, OpenStatus, OpenUpdate};
use crate::panels::PanelKind;
use crate::services::{ActivityCounts, Services};
use crate::sink::{LogSink, ReviewSink, ThreadChange};
use crate::suggestions::{self, Apply, Change, Confirmation, Offer, SuggestionKey};
use crate::writing::{Writing, WritingEvent};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentKind {
    /// Sent on its own.
    Posted,
    /// Held for the review that gets submitted as a whole.
    Queued,
}

#[derive(Clone, Debug)]
pub struct LocalComment {
    pub body: SharedString,
    pub kind: CommentKind,
}

/// How one forge account is doing, for the status bar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForgeState {
    Connecting,
    Ok,
    /// Not signed in, or the token was rejected.
    AuthError(SharedString),
    RateLimited,
    Error(SharedString),
}

impl ForgeState {
    pub fn of(error: &forge::Error) -> Self {
        match error {
            forge::Error::Auth { reason, .. } => Self::AuthError(reason.clone().into()),
            forge::Error::Cli { reason, .. } => Self::AuthError(reason.clone().into()),
            forge::Error::RateLimited { .. } => Self::RateLimited,
            other => Self::Error(other.to_string().into()),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Ok => "ok",
            Self::AuthError(_) => "auth error",
            Self::RateLimited => "rate limited",
            Self::Error(_) => "offline",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ForgeStatus {
    pub host: SharedString,
    pub kind: ForgeKind,
    pub state: ForgeState,
    /// Who is signed in, once the forge has said.
    pub account: Option<forge::model::User>,
}

/// Where the data comes from.
enum Source {
    Fixtures {
        fixtures: Arc<Fixtures>,
        /// Synthetic diff size for benchmarks; `None` is one small file per path.
        min_rows: Option<usize>,
        /// Diffs and threads built so far, so opening one again is a cached
        /// open, as with a pull request whose objects are local.
        built: HashMap<PrKey, (Arc<diff::Diff>, Arc<[Thread]>)>,
        /// Whether the pull requests have review threads.
        threads: bool,
    },
    /// The forges, once the services have started.
    Live(Option<Services>),
}

/// Where applying suggestions stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyState {
    Running(usize),
    Failed(SharedString),
    /// Pushed; the pull request is being read again.
    Applied(usize),
}

/// Which way `]s` and `[s` move through a stack shown top first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackStep {
    /// Down the list, toward the trunk.
    Down,
    /// Up the list, away from the trunk.
    Up,
}

/// How an open shows its panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenMode {
    /// In the preview tab, which the next preview open reuses.
    Preview,
    /// In a tab that stays until it is closed.
    Permanent,
    /// Permanent, in a new tab group beside the current one.
    Beside,
}

/// The diff of an open pull request; `generation` changes with each new one.
#[derive(Clone, Debug)]
pub struct OpenDiff {
    pub diff: Arc<diff::Diff>,
    pub generation: u64,
    /// The head commit it shows, short.
    pub head: SharedString,
    /// The full id of that commit: the diff's lines are the file at this
    /// head, and no other.
    pub head_sha: String,
    pub complete: bool,
}

/// Everything loaded for one open pull request.
pub struct OpenPr {
    pub key: PrKey,
    pub pr: PullRequest,
    pub files: Vec<ChangedFile>,
    pub checks: Vec<Check>,
    /// Every thread, anchored or not, oldest first.
    pub threads: Arc<[Thread]>,
    pub stack: Stack,
    pub viewed: Viewed,
    pub diff: Option<OpenDiff>,
    pub status: OpenStatus,
    /// When the user opened it, for the open-to-first-paint time.
    pub opened_at: Instant,
    pub first_paint: Option<Duration>,
    /// Path of the file selected in the tree.
    pub selected_file: Option<SharedString>,
    /// The commits from the merge base to the head, newest first; `None`
    /// until they are read.
    pub commits: Option<PrCommits>,
    /// The commit the Diff panel shows alone, in place of all changes.
    pub commit_view: Option<CommitView>,
    /// Marks from the cache, and the toggles made since, by (path, blob).
    marks: Option<store::ViewedSet>,
    toggles: HashMap<(SharedString, SharedString), bool>,
    /// The forge's own viewed state (GitHub), by path.
    forge_viewed: HashMap<String, ViewedState>,
    /// The newest snapshot, patched by writes since.
    snapshot: Option<Arc<Snapshot>>,
    /// Suggestions picked for one commit.
    pub batch: Vec<SuggestionKey>,
    /// The changes the user is being asked to confirm; nothing is applied
    /// until they do.
    pub confirm: Option<Confirmation>,
    pub apply: Option<ApplyState>,
    /// The run, job and step tree behind the checks.
    pub ci: CiPanel,
    /// Merge, close and draft state, and the action in flight.
    pub lifecycle: Lifecycle,
    /// The reviewer, assignee or label write in flight, and the last error.
    pub people: crate::people::PeopleState,
    _task: Option<Task<()>>,
}

/// A line under the CI tree: a re-run on its way, or what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CiNotice {
    pub text: SharedString,
    pub error: bool,
}

/// The job whose log the Log panel shows.
#[derive(Clone, Debug)]
pub struct LogTarget {
    pub repo: RepoRef,
    pub forge: ForgeKind,
    pub run_name: SharedString,
    pub job: CiJob,
    /// Changes with every pick, so the panel starts the log over.
    pub generation: u64,
    /// A step to scroll to, with the generation of that ask.
    pub step: Option<(u32, u64)>,
}

/// An open pull request's CI, as the Conversation's pipelines card and the
/// Log panel show it.
#[derive(Default)]
pub struct CiPanel {
    pub runs: Vec<CiRun>,
    /// The pipelines card lists the runs and their jobs.
    pub listed: bool,
    pub load: CiLoad,
    /// The head the runs were read for.
    head: Option<Sha>,
    pub confirm: Option<RetryConfirm>,
    pub notice: Option<CiNotice>,
    /// Runs folded to their header row.
    pub folded: HashSet<RunId>,
    /// Jobs with their steps shown.
    pub expanded: HashSet<JobId>,
    pub log: Option<LogTarget>,
    task: Option<Task<()>>,
    /// The next read while a run is queued or running; dropped with the
    /// pull request, so a closed one stops polling.
    poll: Option<Task<()>>,
}

impl OpenPr {
    fn new(pr: PullRequest) -> Self {
        Self {
            key: pr.key.clone(),
            pr,
            files: Vec::new(),
            checks: Vec::new(),
            threads: Arc::new([]),
            stack: Stack::default(),
            viewed: Viewed::default(),
            diff: None,
            status: OpenStatus::Loading,
            opened_at: Instant::now(),
            first_paint: None,
            selected_file: None,
            commits: None,
            commit_view: None,
            marks: None,
            toggles: HashMap::new(),
            forge_viewed: HashMap::new(),
            snapshot: None,
            batch: Vec::new(),
            confirm: None,
            apply: None,
            ci: CiPanel::default(),
            lifecycle: Lifecycle::default(),
            people: crate::people::PeopleState::default(),
            _task: None,
        }
    }

    /// The id of the commit the Diff panel shows alone, if one is.
    pub fn shown_commit(&self) -> Option<&SharedString> {
        self.commit_view.as_ref().map(|view| &view.commit.sha)
    }

    /// The diff the Diff panel shows: the commit picked, once built, or
    /// all changes.
    pub fn shown_diff(&self) -> Option<&OpenDiff> {
        match &self.commit_view {
            Some(view) => view.diff.as_ref(),
            None => self.diff.as_ref(),
        }
    }

    /// Local marks at the current blobs, plus what the forge says is viewed
    /// (a forge mark of a file the user has not touched here), then the
    /// toggles made in this session.
    fn refresh_viewed(&mut self) {
        let mut viewed = match &self.marks {
            Some(marks) => Viewed::from_store(marks, &self.files),
            None => self.viewed.clone(),
        };
        for file in &self.files {
            if self.forge_viewed.get(file.path.as_ref()) == Some(&ViewedState::Viewed) {
                viewed.set(file, true);
            }
        }
        for file in &self.files {
            if let Some(&on) = self.toggles.get(&(file.path.clone(), file.blob.clone())) {
                viewed.set(file, on);
            }
        }
        self.viewed = viewed;
    }

    /// Additions and deletions: from the files once they are known.
    pub fn stat(&self) -> (u32, u32) {
        if self.diff.as_ref().is_some_and(|d| d.complete) {
            (
                self.files.iter().map(|f| f.added).sum(),
                self.files.iter().map(|f| f.removed).sum(),
            )
        } else {
            self.pr.stat.unwrap_or_default()
        }
    }

    /// A lifecycle write answered with the pull request as it now reads.
    pub(crate) fn set_forge_pr(&mut self, pull_request: forge::model::PullRequest) {
        let snapshot = match &self.snapshot {
            Some(snapshot) => Snapshot {
                pull_request,
                ..(**snapshot).clone()
            },
            None => Snapshot {
                pull_request,
                threads: self.threads.to_vec(),
                checks: Vec::new(),
            },
        };
        self.snapshot = Some(Arc::new(snapshot));
    }

    /// The commit a merge asked now would land: the head of the diff the
    /// user is looking at, else the forge's newest head.
    #[must_use]
    pub fn merge_head(&self) -> Option<Sha> {
        self.diff
            .as_ref()
            .filter(|diff| !diff.head_sha.is_empty())
            .map(|diff| Sha::new(diff.head_sha.clone()))
            .or_else(|| self.forge_pr().map(|pr| pr.head.sha.clone()))
    }

    /// The forge's record: the newest snapshot, or the one the row was made
    /// from.
    pub fn forge_pr(&self) -> Option<&forge::model::PullRequest> {
        self.snapshot
            .as_ref()
            .map(|s| &s.pull_request)
            .or(self.pr.source.as_deref())
    }
}

/// A dock panel the session knows: which kind, and the pull request it is
/// bound to (`None`: it follows the focused one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelEntry {
    pub id: EntityId,
    pub kind: PanelKind,
    pub pr: Option<PrKey>,
}

pub struct Session {
    pub inbox: Arc<Inbox>,
    /// The stacks among the inbox's pull requests, kept with the inbox.
    stacks: Arc<[Vec<usize>]>,
    source: Source,
    /// Every open pull request, in the order they were opened.
    prs: Vec<OpenPr>,
    /// The one the sidebar, the Log panel and the keyboard act on.
    focused: Option<PrKey>,
    /// The pull request shown in the preview tab, which the next preview
    /// open replaces.
    preview: Option<PrKey>,
    /// Composers, drafts and the pending review, per open pull request.
    writings: HashMap<PrKey, Entity<Writing>>,
    store: Option<store::Store>,
    /// One draft writer for every open pull request.
    saver: Option<DraftSaver>,
    sink: Arc<dyn ReviewSink>,
    /// Text a new conversation composer starts with, for screenshots.
    seed: Option<String>,
    diff_layout: Layout,
    next_generation: u64,
    pub forges: Vec<ForgeStatus>,
    pub activity: ActivityCounts,
    /// A problem worth a line in the status bar (a bad config file).
    pub notice: Option<SharedString>,
    /// The dock panels, so an event can bring the right one forward.
    panels: Vec<PanelEntry>,
    /// Pull requests loaded for an action from their inbox row, without a
    /// panel or the focus; released once the action is done.
    background: HashSet<PrKey>,
    /// Refetches every inbox section; set by whoever fills the inbox.
    refresher: Option<InboxRefresher>,
    /// Stops the live refresher listing a repository and takes it out of
    /// the config file; set by whoever fills the inbox from the forges.
    repo_remover: Option<RepoRemover>,
    pub inbox_refresh: InboxRefresh,
    /// Whether rows with active CI are being read again (see
    /// [`crate::ci_settle`]).
    settling: bool,
}

/// Starts a refetch of every inbox section, resolving when all answered.
pub type InboxRefresher = Rc<dyn Fn(&mut App) -> Task<()>>;

/// Drops a repository's inbox section from the live sources and the config.
pub type RepoRemover = Rc<dyn Fn(&RepoRef, &mut App)>;

/// Where refetching the inbox by hand stands, for the inbox's button.
#[derive(Clone, Debug, Default)]
pub struct InboxRefresh {
    pub running: bool,
    /// When each forge host last finished one.
    pub last: Vec<(SharedString, Timestamp)>,
}

/// Bring a panel forward, building it when the pull request has none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShowPanel {
    /// `None` for the Log, which follows the focused pull request.
    pub pr: Option<PrKey>,
    pub kind: PanelKind,
    /// The preview this open replaces: its panels close and the new one
    /// takes their place.
    pub replace: Option<PrKey>,
    /// In a new tab group beside the current one.
    pub beside: bool,
}

/// Things panels react to that a plain `notify` cannot say.
#[derive(Clone, Debug)]
pub enum SessionEvent {
    /// A file was clicked in the tree, also when it was already selected, so
    /// the diff scrolls back to it.
    FileSelected {
        pr: PrKey,
        path: SharedString,
        /// The file selected before, for Back.
        previous: Option<SharedString>,
    },
    /// Show this thread in the pull request's diff.
    JumpToThread {
        pr: PrKey,
        thread: ThreadId,
        /// Move keyboard focus to the diff; false keeps it where the jump
        /// came from (the sidebar list, which keeps j, k and Enter).
        focus_diff: bool,
    },
    /// Show this draft where it is written: the composer of a draft on
    /// code, a reply or an edit in the pull request's diff, the comment box
    /// of a draft on the pull request in its conversation.
    JumpToDraft {
        pr: PrKey,
        anchor: DraftAnchor,
    },
    /// A commit was picked to show alone in the pull request's diff, or
    /// all changes for `None`.
    CommitShown {
        pr: PrKey,
        commit: Option<SharedString>,
        /// The commit shown before, for Back.
        previous: Option<SharedString>,
    },
    Show(ShowPanel),
    /// Close this dock panel: its tab's close button was clicked.
    ClosePanel(EntityId),
}

impl EventEmitter<SessionEvent> for Session {}

/// The panel a draft at `anchor` is written in.
#[must_use]
pub fn draft_panel(anchor: &DraftAnchor) -> PanelKind {
    match anchor {
        DraftAnchor::Conversation => PanelKind::Conversation,
        DraftAnchor::Line { .. }
        | DraftAnchor::File { .. }
        | DraftAnchor::Reply { .. }
        | DraftAnchor::Edit { .. }
        | DraftAnchor::Review => PanelKind::Diff,
    }
}

struct SessionHandle(Entity<Session>);

impl Global for SessionHandle {}

/// The stacks among the inbox's pull requests by the base-branch rule the
/// Stack section uses, over the forge's records (or the fixtures'): each two
/// or more rows, bottom first, as indices into `inbox.prs`. A row sits in
/// one stack at most; the first one found keeps it.
fn inbox_stacks(inbox: &Inbox, source: &Source) -> Arc<[Vec<usize>]> {
    let records: Vec<forge::model::PullRequest> = inbox
        .prs
        .iter()
        .filter_map(|pr| match source {
            Source::Fixtures { fixtures, .. } => fixtures.forge_pr(&pr.key),
            Source::Live(_) => pr.source.as_deref().cloned(),
        })
        .collect();
    let rows: HashMap<&PrKey, usize> = inbox
        .prs
        .iter()
        .enumerate()
        .map(|(ix, pr)| (&pr.key, ix))
        .collect();
    let mut claimed: HashSet<usize> = HashSet::new();
    let mut stacks: Vec<Vec<usize>> = Vec::new();
    for target in records.iter().filter(|pr| pr.state.is_open()) {
        if rows
            .get(&PrKey::of(target))
            .is_some_and(|ix| claimed.contains(ix))
        {
            continue;
        }
        let links = forge::stack_by_base_branch(target, &records).links;
        // A member another stack holds splits this one where it sits.
        let ixs: Vec<Option<usize>> = links
            .iter()
            .map(|link| {
                let key = PrKey {
                    repo: target.repo.clone(),
                    number: link.number,
                };
                rows.get(&key).copied().filter(|ix| !claimed.contains(ix))
            })
            .collect();
        for run in ixs.split(Option::is_none).filter(|run| run.len() >= 2) {
            let run: Vec<usize> = run.iter().flatten().copied().collect();
            claimed.extend(&run);
            stacks.push(run);
        }
    }
    stacks.into()
}

impl Session {
    fn install(
        inbox: Inbox,
        source: Source,
        store: Option<store::Store>,
        cx: &mut App,
    ) -> Entity<Self> {
        let stacks = inbox_stacks(&inbox, &source);
        let session = cx.new(|_| Self {
            inbox: Arc::new(inbox),
            stacks,
            source,
            prs: Vec::new(),
            focused: None,
            preview: None,
            writings: HashMap::new(),
            saver: store.clone().map(DraftSaver::spawn),
            store,
            sink: Arc::new(LogSink),
            seed: None,
            diff_layout: Layout::Unified,
            next_generation: 0,
            forges: Vec::new(),
            activity: ActivityCounts::default(),
            notice: None,
            panels: Vec::new(),
            background: HashSet::new(),
            refresher: None,
            repo_remover: None,
            inbox_refresh: InboxRefresh::default(),
            settling: false,
        });
        cx.set_global(SessionHandle(session.clone()));
        session
    }

    /// The fixture inbox; opening builds synthetic diffs.
    ///
    /// Drafts are kept in `store` when there is one, in memory otherwise.
    pub fn install_fixtures(
        min_rows: Option<usize>,
        threads: bool,
        store: Option<store::Store>,
        cx: &mut App,
    ) -> Entity<Self> {
        let fixtures = Arc::new(Fixtures::load());
        let inbox = fixtures.inbox.clone();
        let source = Source::Fixtures {
            fixtures,
            min_rows,
            built: HashMap::new(),
            threads,
        };
        let session = Self::install(inbox, source, store, cx);
        let weak = session.downgrade();
        let refresher: InboxRefresher = Rc::new(move |cx: &mut App| {
            // Fixtures have nothing newer: the rows are set again as they are.
            if let Some(session) = weak.upgrade() {
                session.update(cx, |session, cx| {
                    let inbox = Inbox::clone(&session.inbox);
                    session.set_inbox(inbox, cx);
                });
            }
            Task::ready(())
        });
        session.update(cx, |session, _| {
            session.forges = fixtures::forge_statuses();
            session.refresher = Some(refresher);
        });
        session
    }

    /// An empty inbox that [`crate::live`] fills.
    pub fn install_live(cx: &mut App) -> Entity<Self> {
        Self::install(Inbox::default(), Source::Live(None), None, cx)
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<SessionHandle>().0.clone()
    }

    /// The session, or `None` before [`Self::install_fixtures`] or
    /// [`Self::install_live`] ran.
    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<SessionHandle>()
            .map(|handle| handle.0.clone())
    }

    pub fn services(&self) -> Option<&Services> {
        match &self.source {
            Source::Live(services) => services.as_ref(),
            Source::Fixtures { .. } => None,
        }
    }

    /// Whether the data is invented.
    pub fn is_fixtures(&self) -> bool {
        matches!(self.source, Source::Fixtures { .. })
    }

    /// The services are up: opens that were waiting for them start now.
    pub fn set_services(&mut self, services: Services, cx: &mut Context<Self>) {
        let kind = |host: &str| services.config.forge(host).map(|forge| forge.kind);
        let waiting: Vec<PullRequest> = self
            .prs
            .iter()
            .map(|open| {
                let mut pr = open.pr.clone();
                pr.forge = kind(&pr.key.repo.host).unwrap_or(pr.forge);
                pr
            })
            .collect();
        if self.store.as_ref().map(store::Store::path)
            != services.store.as_ref().map(store::Store::path)
        {
            self.store = services.store.clone();
            self.saver = self.store.clone().map(DraftSaver::spawn);
        }
        self.sink = Arc::new(ForgeSink::new(services.clone()));
        let (store, saver, sink) = (self.store.clone(), self.saver.clone(), self.sink.clone());
        for writing in self.writings.values() {
            writing.update(cx, |writing, cx| {
                writing.set_sink(sink.clone());
                writing.attach(store.clone(), saver.clone(), cx);
            });
        }
        self.source = Source::Live(Some(services));
        for pr in waiting {
            self.open_row(pr, cx);
        }
        self.settle_ci(cx);
        cx.notify();
    }

    /// The pull request a command line target names.
    pub fn resolve_target(&self, target: &crate::OpenTarget) -> Option<PrKey> {
        match target {
            crate::OpenTarget::Key(key) => Some(key.clone()),
            crate::OpenTarget::Number(number) => self
                .inbox
                .prs
                .iter()
                .find(|pr| pr.number() == *number)
                .map(|pr| pr.key.clone()),
        }
    }

    /// Loads and focuses a command line target, without asking for panels.
    pub fn open_target(&mut self, target: &crate::OpenTarget, cx: &mut Context<Self>) {
        if let Some(key) = self.resolve_target(target) {
            self.open(&key, cx);
        }
    }

    /// Replaces the inbox; the open pull requests and the selection stay.
    pub fn set_inbox(&mut self, mut inbox: Inbox, cx: &mut Context<Self>) {
        self.keep_patched_ci(&mut inbox);
        for open in &mut self.prs {
            if let Some(row) = inbox.get(&open.key)
                && open.pr.source.is_none()
            {
                open.pr = row.clone();
            }
        }
        self.stacks = inbox_stacks(&inbox, &self.source);
        self.inbox = Arc::new(inbox);
        self.settle_ci(cx);
        cx.notify();
    }

    /// A row built from the same forge record as before (its list was not
    /// read again, or the read failed) keeps the CI state patched since;
    /// a fresh list's state wins.
    fn keep_patched_ci(&self, inbox: &mut Inbox) {
        let old: HashMap<&PrKey, &PullRequest> =
            self.inbox.prs.iter().map(|row| (&row.key, row)).collect();
        for row in &mut inbox.prs {
            if let Some(old) = old.get(&row.key)
                && let (Some(was), Some(now)) = (&old.source, &row.source)
                && Arc::ptr_eq(was, now)
            {
                row.ci = old.ci;
            }
        }
    }

    /// The inbox's stacks of two or more pull requests, each bottom first,
    /// as indices into its rows.
    pub fn inbox_stacks(&self) -> Arc<[Vec<usize>]> {
        self.stacks.clone()
    }

    // ---- open pull requests

    /// The focused pull request's key.
    pub fn open_key(&self) -> Option<&PrKey> {
        self.focused.as_ref()
    }

    /// The focused pull request.
    pub fn open_pr(&self) -> Option<&OpenPr> {
        self.pr(self.focused.as_ref()?)
    }

    pub(crate) fn focused_mut(&mut self) -> Option<&mut OpenPr> {
        let key = self.focused.clone()?;
        self.pr_mut(&key)
    }

    /// An open pull request.
    pub fn pr(&self, key: &PrKey) -> Option<&OpenPr> {
        self.prs.iter().find(|open| &open.key == key)
    }

    pub(crate) fn pr_mut(&mut self, key: &PrKey) -> Option<&mut OpenPr> {
        self.prs.iter_mut().find(|open| &open.key == key)
    }

    /// Every open pull request, in the order they were opened.
    pub fn open_prs(&self) -> impl Iterator<Item = &OpenPr> {
        self.prs.iter()
    }

    pub fn is_open(&self, key: &PrKey) -> bool {
        self.pr(key).is_some()
    }

    /// The key the app holds for `key` (open or in the inbox), compared the
    /// way forges compare repositories: without regard to case.
    pub fn known_key(&self, key: &PrKey) -> Option<PrKey> {
        let same = |other: &PrKey| other.number == key.number && other.repo.same_as(&key.repo);
        self.prs
            .iter()
            .map(|open| &open.key)
            .chain(self.inbox.prs.iter().map(|pr| &pr.key))
            .find(|other| same(other))
            .cloned()
    }

    /// What references in `key`'s text may point at: its repository, and
    /// the repositories and pull requests the app knows on the same host.
    pub fn ref_context(&self, key: &PrKey) -> Option<crate::references::RefContext> {
        let open = self.pr(key)?;
        let host = &key.repo.host;
        let mut ctx = crate::references::RefContext::new(open.pr.forge, key.repo.clone());
        let keys = self
            .prs
            .iter()
            .map(|open| &open.key)
            .chain(self.inbox.prs.iter().map(|pr| &pr.key))
            .filter(|other| other.repo.host.eq_ignore_ascii_case(host));
        for other in keys {
            let path = other.repo.path().to_lowercase();
            ctx.known_prs.insert((path.clone(), other.number.0));
            ctx.known_repos.insert(path);
        }
        if let Some(services) = self.services() {
            ctx.known_repos.extend(
                services
                    .config
                    .repos
                    .iter()
                    .filter_map(crate::config::RepoEntry::repo_ref)
                    .filter(|repo| repo.host.eq_ignore_ascii_case(host))
                    .map(|repo| repo.path().to_lowercase()),
            );
        }
        Some(ctx)
    }

    /// The pull request in the preview tab.
    pub fn preview(&self) -> Option<&PrKey> {
        self.preview.as_ref()
    }

    pub fn is_preview(&self, key: &PrKey) -> bool {
        self.preview.as_ref() == Some(key)
    }

    /// What the user writes on `key`.
    pub fn writing_for(&self, key: &PrKey) -> Option<Entity<Writing>> {
        self.writings.get(key).cloned()
    }

    /// What the user writes on the focused pull request.
    pub fn writing(&self) -> Option<Entity<Writing>> {
        self.writing_for(self.focused.as_ref()?)
    }

    /// The draft writer every pull request shares, to flush on quit.
    pub fn saver(&self) -> Option<DraftSaver> {
        self.saver.clone()
    }

    /// Text new conversation composers start with, for screenshots.
    pub fn seed_conversation(&mut self, text: Option<&str>, cx: &mut Context<Self>) {
        self.seed = text.map(str::to_owned);
        for writing in self.writings.values() {
            writing.update(cx, |writing, _| writing.seed_conversation(text));
        }
    }

    /// Makes `key` the pull request the sidebar and the keyboard act on.
    pub fn focus(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if self.focused.as_ref() != Some(key) && self.is_open(key) {
            self.focused = Some(key.clone());
            cx.notify();
        }
    }

    /// Opens a pull request of the inbox, or one known only by its key (a
    /// stack member, a command line argument), and focuses it. One already
    /// open only takes the focus. No panel is asked for: [`Self::show`] does.
    pub fn open(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if !self.is_open(key) {
            let focused = self.open_pr();
            let row = self.inbox.get(key).cloned().unwrap_or_else(|| {
                let forge = self
                    .services()
                    .and_then(|s| s.config.forge(&key.repo.host))
                    .map_or(ForgeKind::GitHub, |forge| forge.kind);
                let mut row = placeholder(key, forge);
                // A stack member knows its branches, so its diff can start on
                // its own base (its parent's branch) before the forge answers.
                let member =
                    focused.and_then(|open| open.stack.members.iter().find(|m| &m.key == key));
                if let Some(member) = member {
                    row.title = member.title.clone();
                    row.base = member.base.clone();
                    row.head = member.head.clone();
                }
                row
            });
            let stack = focused
                .filter(|open| open.stack.members.iter().any(|m| &m.key == key))
                .map(|open| open.stack.clone());
            self.open_row(row, cx);
            // The stack stays on screen while the neighbor loads.
            if let (Some(stack), Some(open)) = (stack, self.pr_mut(key))
                && open.stack.members.len() < stack.members.len()
            {
                open.stack = stack;
            }
        }
        self.focused = Some(key.clone());
        cx.notify();
    }

    /// Opens `key` if needed, focuses it and brings its `kind` panel forward,
    /// as a click in the inbox does. A preview open of a pull request that
    /// is not open replaces the current preview.
    pub fn show(&mut self, key: &PrKey, kind: PanelKind, mode: OpenMode, cx: &mut Context<Self>) {
        // A pull request loaded only for a row's action opens as if it was not.
        let was_open = self.is_open(key) && !self.background.remove(key);
        let replace = match mode {
            OpenMode::Preview if !was_open => self.preview.take().filter(|old| old != key),
            _ => None,
        };
        self.open(key, cx);
        match mode {
            OpenMode::Preview if !was_open => self.preview = Some(key.clone()),
            OpenMode::Preview => {}
            OpenMode::Permanent | OpenMode::Beside => self.keep(key, cx),
        }
        cx.emit(SessionEvent::Show(ShowPanel {
            pr: Some(key.clone()),
            kind,
            replace,
            beside: mode == OpenMode::Beside,
        }));
        cx.notify();
    }

    /// The preview tab of `key` becomes a permanent one.
    pub fn keep(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if self.preview.as_ref() == Some(key) {
            self.preview = None;
            cx.notify();
        }
    }

    /// Drops everything loaded for `key`: its last panel closed.
    pub fn release(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(ix) = self.prs.iter().position(|open| &open.key == key) else {
            return;
        };
        self.prs.remove(ix);
        self.writings.remove(key);
        self.background.remove(key);
        if self.preview.as_ref() == Some(key) {
            self.preview = None;
        }
        if self.focused.as_ref() == Some(key) {
            // One loaded only for a row's action never takes the focus.
            self.focused = self
                .prs
                .iter()
                .rev()
                .find(|open| !self.background.contains(&open.key))
                .map(|open| open.key.clone());
        }
        cx.notify();
    }

    // ---- panels

    /// A panel joined the dock.
    pub fn register_panel(&mut self, entry: PanelEntry) {
        self.panels.retain(|known| known.id != entry.id);
        self.panels.push(entry);
    }

    /// A panel left the dock; a pull request whose last panel it was is
    /// released.
    pub fn panel_removed(&mut self, id: EntityId, cx: &mut Context<Self>) {
        let Some(ix) = self.panels.iter().position(|entry| entry.id == id) else {
            return;
        };
        let entry = self.panels.remove(ix);
        if let Some(key) = entry.pr
            && !self
                .panels
                .iter()
                .any(|other| other.pr.as_ref() == Some(&key))
        {
            self.release(&key, cx);
        }
    }

    /// Asks the shell to close a dock panel.
    pub fn close_panel(&mut self, id: EntityId, cx: &mut Context<Self>) {
        cx.emit(SessionEvent::ClosePanel(id));
    }

    /// The panels the dock holds.
    pub fn panels(&self) -> &[PanelEntry] {
        &self.panels
    }

    /// The `kind` panel showing `pr`: the one bound to it, or a panel that
    /// follows the focus while `pr` has it. `None` asks for the Log.
    pub fn panel_of(&self, pr: Option<&PrKey>, kind: PanelKind) -> Option<EntityId> {
        let bound = pr.and_then(|pr| {
            self.panels
                .iter()
                .find(|entry| entry.kind == kind && entry.pr.as_ref() == Some(pr))
        });
        let follows = || {
            self.panels.iter().find(|entry| {
                entry.kind == kind && entry.pr.is_none() && (pr.is_none() || pr == self.open_key())
            })
        };
        bound.or_else(follows).map(|entry| entry.id)
    }

    /// Reads the pull request again from the forge and git, after a write
    /// that moved its head.
    pub fn reload(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(open) = self.pr(key) else {
            return;
        };
        if let Some(services) = self.services() {
            services.forget_snapshot(key);
        }
        let (pr, stack, selected) = (
            open.pr.clone(),
            open.stack.clone(),
            open.selected_file.clone(),
        );
        let apply = self.pr_mut(key).and_then(|open| open.apply.take());
        self.open_row(pr, cx);
        if let Some(open) = self.pr_mut(key) {
            open.stack = stack;
            open.apply = apply;
            open.selected_file = selected;
        }
    }

    /// `]s` / `[s`: opens the next pull request of the stack as it is listed
    /// (top first), so `Down` goes toward the trunk.
    pub fn step_stack(&mut self, step: StackStep, cx: &mut Context<Self>) {
        let Some(open) = self.open_pr() else {
            return;
        };
        if let Some(key) = stack_neighbor(&open.stack, &open.key, step) {
            self.show(&key, PanelKind::Diff, OpenMode::Preview, cx);
        }
    }

    /// Builds the open pull request for `row`, in place of an earlier one of
    /// the same key.
    fn open_row(&mut self, row: PullRequest, cx: &mut Context<Self>) {
        let mut open = OpenPr::new(row);
        let generation = self.generation();
        match &mut self.source {
            Source::Fixtures {
                fixtures,
                min_rows,
                built,
                threads: with_threads,
            } => {
                let detail = fixtures.detail(&open.key);
                let (diff, threads) = built
                    .entry(open.key.clone())
                    .or_insert_with(|| {
                        let diff =
                            fixtures::synthetic_diff(open.key.number.0, &detail.files, *min_rows);
                        let threads = if *with_threads {
                            fixtures::threads(&diff)
                        } else {
                            Vec::new()
                        };
                        (Arc::new(diff), threads.into())
                    })
                    .clone();
                open.viewed = fixtures.viewed(&open.key, &detail.files);
                open.commits = Some(PrCommits::all(fixtures::commits(&open.pr)));
                open.snapshot = fixtures.forge_pr(&open.key).map(|pull_request| {
                    Arc::new(Snapshot {
                        pull_request,
                        threads: threads.to_vec(),
                        checks: Vec::new(),
                    })
                });
                open.lifecycle = Lifecycle::fixture(open.forge_pr(), &detail.runs);
                open.files = detail.files;
                open.checks = detail.checks;
                open.ci.runs = detail.runs;
                open.ci.load = CiLoad::Ready;
                open.threads = threads;
                open.stack = fixtures.stack(&open.key);
                open.status = OpenStatus::Ready;
                open.diff = Some(OpenDiff {
                    diff,
                    generation,
                    head: open.pr.sha.clone(),
                    head_sha: open.pr.sha.to_string(),
                    complete: true,
                });
            }
            Source::Live(Some(services)) => {
                let services = services.clone();
                open._task = Some(self.start_open(services.clone(), &open, cx));
                if let Some(pr) = open.pr.source.clone() {
                    open.ci.task = Some(start_ci_read(&services, &open.key, &pr.head.sha, cx));
                    open.ci.head = Some(pr.head.sha.clone());
                    open.ci.load = CiLoad::Loading;
                }
            }
            Source::Live(None) => {}
        }
        open.stack = if open.stack.members.is_empty() {
            self.stack_from_inbox(&open.pr)
        } else {
            open.stack
        };
        // The full head id: a review starts on it.
        let key = open.key.clone();
        let head = open.pr.source.as_ref().map(|pr| pr.head.sha.clone());
        match self.prs.iter_mut().find(|known| known.key == key) {
            Some(known) => *known = open,
            None => self.prs.push(open),
        }
        self.ensure_writing(&key, head, cx);
        self.refresh_lifecycle(&key, cx);
        cx.notify();
    }

    /// The writing state of `key`, made on its first open; later opens tell
    /// it the head commit.
    fn ensure_writing(&mut self, key: &PrKey, head: Option<Sha>, cx: &mut Context<Self>) {
        if let Some(writing) = self.writings.get(key) {
            let key = key.clone();
            writing.update(cx, |writing, cx| writing.switch(Some(key), head, cx));
            return;
        }
        let (store, saver, sink) = (self.store.clone(), self.saver.clone(), self.sink.clone());
        let seed = self.seed.clone();
        let writing = cx.new(|cx| {
            let mut writing = Writing::new(sink);
            writing.seed_conversation(seed.as_deref());
            writing.attach(store, saver, cx);
            writing
        });
        let owner = key.clone();
        cx.subscribe(
            &writing,
            move |session: &mut Self, _, event: &WritingEvent, cx| {
                match event {
                    WritingEvent::Threads(change) => session.patch_threads(&owner, change, cx),
                    // Starting a comment keeps a preview tab.
                    WritingEvent::Opened(_) => session.keep(&owner, cx),
                    WritingEvent::ReviewSubmitted => session.review_submitted(&owner, cx),
                }
            },
        )
        .detach();
        let owner = key.clone();
        cx.observe(&writing, move |session: &mut Self, writing, cx| {
            if session.is_preview(&owner) && writing.read(cx).draft_count() > 0 {
                session.keep(&owner, cx);
            }
        })
        .detach();
        let switched = key.clone();
        writing.update(cx, |writing, cx| writing.switch(Some(switched), head, cx));
        self.writings.insert(key.clone(), writing);
    }

    fn generation(&mut self) -> u64 {
        self.next_generation += 1;
        self.next_generation
    }

    fn start_open(&self, services: Services, open: &OpenPr, cx: &mut Context<Self>) -> Task<()> {
        let base = match &open.pr.source {
            Some(pr) => Some(pr.base.name.clone()),
            None => (!open.pr.base.is_empty()).then(|| open.pr.base.to_string()),
        };
        let request = OpenRequest {
            key: open.key.clone(),
            base_branch: base,
            forge_viewed: self
                .capabilities_for(&open.key)
                .is_some_and(|caps| caps.viewed == forge::ViewedSync::Forge),
        };
        let key = open.key.clone();
        let (tx, mut rx) = mpsc::unbounded();
        let pipeline = cx
            .background_executor()
            .spawn(opener::open_pr(services, request, tx));
        cx.spawn(async move |this, cx| {
            let _pipeline = pipeline;
            while let Some(update) = rx.next().await {
                let applied = this.update(cx, |session, cx| session.apply(&key, update, cx));
                if applied.is_err() {
                    break;
                }
            }
        })
    }

    /// The stack by the base-branch rule over the inbox, until the forge's
    /// own stack arrives.
    fn stack_from_inbox(&self, pr: &PullRequest) -> Stack {
        let Some(target) = &pr.source else {
            return Stack::default();
        };
        let candidates: Vec<forge::model::PullRequest> = self
            .inbox
            .prs
            .iter()
            .filter_map(|row| row.source.as_deref().cloned())
            .collect();
        let stack = forge::stack_by_base_branch(target, &candidates);
        Stack::from_forge(&stack, &pr.key.repo, &self.inbox)
    }

    fn apply(&mut self, key: &PrKey, update: OpenUpdate, cx: &mut Context<Self>) {
        let Some(forge) = self.pr(key).map(|open| open.pr.forge) else {
            return;
        };
        match update {
            OpenUpdate::Snapshot { snapshot, .. } => {
                let row = PullRequest::from_forge(Arc::new(snapshot.pull_request.clone()), forge);
                let stack = self.stack_from_inbox(&row);
                let Some(open) = self.pr_mut(key) else {
                    return;
                };
                open.snapshot = Some(snapshot.clone());
                open.checks = snapshot.checks.iter().map(Check::from_forge).collect();
                if *open.threads != snapshot.threads[..] {
                    open.threads = snapshot.threads.iter().cloned().collect();
                }
                if open.stack.members.len() <= 1 {
                    open.stack = stack;
                }
                open.pr = row.clone();
                open.lifecycle.track(&snapshot.pull_request);
                let head = snapshot.pull_request.head.sha.clone();
                open.lifecycle.head_moved(&head);
                let ci_stale = open.ci.head.as_ref() != Some(&head);
                self.patch_row(row);
                if ci_stale {
                    self.refresh_ci(key, cx);
                }
                if let Some(writing) = self.writing_for(key) {
                    let key = key.clone();
                    writing.update(cx, |writing, cx| writing.switch(Some(key), Some(head), cx));
                }
            }
            OpenUpdate::Viewed(marks) => {
                if let Some(open) = self.pr_mut(key) {
                    open.marks = Some(marks);
                    open.refresh_viewed();
                }
            }
            OpenUpdate::ForgeViewed(files) => {
                if let Some(open) = self.pr_mut(key) {
                    open.forge_viewed = files.into_iter().map(|f| (f.path, f.state)).collect();
                    open.refresh_viewed();
                }
                self.keep_forge_viewed(key);
            }
            OpenUpdate::Stack(stack) => {
                let stack = Stack::from_forge(&stack, &key.repo, &self.inbox);
                if let Some(open) = self.pr_mut(key) {
                    open.stack = stack;
                }
            }
            OpenUpdate::Status(status) => {
                if let Some(open) = self.pr_mut(key) {
                    open.status = status;
                }
            }
            OpenUpdate::Commits(list) => {
                if let Some(open) = self.pr_mut(key) {
                    open.commits = Some(list);
                }
            }
            OpenUpdate::Diff(DiffUpdate {
                head,
                diff,
                files,
                complete,
            }) => {
                let generation = self.generation();
                if let Some(open) = self.pr_mut(key) {
                    open.files = files;
                    open.refresh_viewed();
                }
                self.keep_forge_viewed(key);
                if let Some(open) = self.pr_mut(key) {
                    open.lifecycle.head_moved(&Sha::new(head.to_string()));
                    open.diff = Some(OpenDiff {
                        diff,
                        generation,
                        head: head.to_hex_with_len(7).to_string().into(),
                        head_sha: head.to_string(),
                        complete,
                    });
                }
            }
        }
        self.settle_ci(cx);
        cx.notify();
    }

    /// What the focused pull request's forge can do.
    pub fn capabilities(&self) -> Option<Capabilities> {
        self.capabilities_for(self.focused.as_ref()?)
    }

    /// What `key`'s forge can do: what its backend reported once connected,
    /// or the matrix row until then (fixtures always).
    pub fn capabilities_for(&self, key: &PrKey) -> Option<Capabilities> {
        let open = self.pr(key)?;
        let probed = self
            .services()
            .and_then(|services| services.capabilities(&key.repo.host));
        Some(probed.unwrap_or_else(|| Capabilities::baseline(open.pr.forge)))
    }

    /// Who is signed in on `host`, once the forge has said.
    pub fn account(&self, host: &str) -> Option<&forge::model::User> {
        self.forges
            .iter()
            .find(|forge| forge.host == host)
            .and_then(|forge| forge.account.as_ref())
    }

    /// A write changed `key`'s threads: patches them in memory and in the
    /// cached snapshot, without reading it again.
    fn patch_threads(&mut self, key: &PrKey, change: &ThreadChange, cx: &mut Context<Self>) {
        let services = self.services().cloned();
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        let threads: Arc<[Thread]> = change.apply(&open.threads).into();
        open.threads = threads.clone();
        if let Some(snapshot) = &open.snapshot {
            let patched = Arc::new(Snapshot {
                threads: threads.to_vec(),
                ..(**snapshot).clone()
            });
            open.snapshot = Some(patched.clone());
            if let Some(services) = services {
                services.replace_snapshot(&open.key, patched);
            }
        }
        if let Source::Fixtures { built, .. } = &mut self.source
            && let Some((_, kept)) = built.get_mut(key)
        {
            *kept = threads;
        }
        cx.notify();
    }

    /// Writes the forge's viewed marks into the local store at the files'
    /// current blobs, so they hold when the forge is not asked.
    fn keep_forge_viewed(&self, key: &PrKey) {
        let Some(open) = self.pr(key) else {
            return;
        };
        let Some(store) = self.services().and_then(|s| s.store.clone()) else {
            return;
        };
        let marks: Vec<(String, String)> = open
            .files
            .iter()
            .filter(|file| open.forge_viewed.get(file.path.as_ref()) == Some(&ViewedState::Viewed))
            .filter(|file| {
                !open
                    .marks
                    .as_ref()
                    .is_some_and(|m| m.is_viewed(&file.path, &file.blob))
            })
            .map(|file| (file.path.to_string(), file.blob.to_string()))
            .collect();
        if !marks.is_empty() {
            store.set_viewed_many(&open.key, marks, true).detach();
        }
    }

    // ---- suggestions

    /// What the suggestion of `key` on pull request `pr` offers; `None` when
    /// nothing, or when the thread is not on `pr`.
    pub fn suggestion(&self, pr: &PrKey, key: &SuggestionKey) -> Option<Offer> {
        let open = self.pr(pr)?;
        let forge_pr = open.forge_pr()?;
        let thread = open.threads.iter().find(|t| t.id == key.thread)?;
        let comment = thread.comments.iter().find(|c| c.id == key.comment)?;
        suggestions::offer(thread, comment, forge_pr, &self.capabilities_for(pr)?)
    }

    /// Adds the suggestion to `pr`'s batch, or takes it out.
    pub fn toggle_batch(&mut self, pr: &PrKey, key: SuggestionKey, cx: &mut Context<Self>) {
        let Some(open) = self.pr_mut(pr) else {
            return;
        };
        if let Some(at) = open.batch.iter().position(|k| *k == key) {
            open.batch.remove(at);
        } else {
            open.batch.push(key);
        }
        cx.notify();
    }

    /// Asks to confirm one suggestion on `pr`, or its batch when `keys` is
    /// it. The suggestions are parsed here, once; confirming applies these.
    /// A suggestion whose thread is not on `pr` fails the whole request.
    pub fn request_apply(&mut self, pr: &PrKey, keys: &[SuggestionKey], cx: &mut Context<Self>) {
        if matches!(
            self.pr(pr).and_then(|o| o.apply.as_ref()),
            Some(ApplyState::Running(_))
        ) {
            return;
        }
        let plans: Vec<Apply> = keys
            .iter()
            .filter_map(|key| match self.suggestion(pr, key)? {
                Offer::Apply(apply) => Some(apply),
                Offer::Html | Offer::Refused(_) => None,
            })
            .collect();
        let Some(open) = self.pr_mut(pr) else {
            return;
        };
        if plans.is_empty() || plans.len() != keys.len() {
            open.apply = Some(ApplyState::Failed(
                "Those suggestions cannot be applied any more".into(),
            ));
            open.confirm = None;
        } else {
            let head = open
                .forge_pr()
                .map_or_else(String::new, |pr| pr.head.sha.to_string());
            open.apply = None;
            open.confirm = Some(Confirmation::new(plans, head));
            self.read_current_lines(pr, cx);
        }
        cx.notify();
    }

    /// Where the commit is built from git objects, reads the lines `pr`'s
    /// confirmation replaces at its head from the object store, off the
    /// main thread, and holds the confirmation until they are in.
    fn read_current_lines(&mut self, pr: &PrKey, cx: &mut Context<Self>) {
        let Some(services) = self.services().cloned() else {
            return;
        };
        let Some(open) = self.pr_mut(pr) else {
            return;
        };
        let Some(confirm) = open.confirm.take() else {
            return;
        };
        let confirm = confirm.awaiting_store();
        let edits = confirm.local_edits();
        let head = confirm.head().to_owned();
        open.confirm = Some(confirm);
        if edits.is_empty() {
            return;
        }
        let (key, number) = (open.key.clone(), open.key.number.0);
        cx.spawn(async move |this, cx| {
            let job = {
                let (objects, repo) = (services.objects.clone(), key.repo.clone());
                let (head, edits) = (head.clone(), edits.clone());
                move || match objects.resolve(&repo) {
                    Ok(store) => suggestions::read_current_lines(&store, number, &head, &edits),
                    Err(_) => vec![None; edits.len()],
                }
            };
            let read = services
                .run_blocking(job)
                .await
                .unwrap_or_else(|| vec![None; edits.len()]);
            this.update(cx, |session, cx| {
                let confirm = session.pr_mut(&key).and_then(|open| open.confirm.as_mut());
                if let Some(confirm) = confirm {
                    confirm.set_read(&edits, read);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Drops `pr`'s confirmation without applying anything.
    pub fn cancel_apply(&mut self, pr: &PrKey, cx: &mut Context<Self>) {
        if let Some(open) = self.pr_mut(pr) {
            open.confirm = None;
        }
        cx.notify();
    }

    /// What `key`'s confirmation lists, with the lines now in the files
    /// taken from its diff.
    pub fn confirm_changes_for(&self, key: &PrKey) -> Vec<Change> {
        let Some(open) = self.pr(key) else {
            return Vec::new();
        };
        let Some(confirm) = open.confirm.as_ref() else {
            return Vec::new();
        };
        let diff = open.diff.as_ref();
        let head = diff.map_or("", |d| d.head_sha.as_str());
        confirm.changes(head, |path, first, last| {
            let file = diff?.diff.files.iter().find(|f| f.path == path)?;
            let (first, last) = (first.checked_sub(1)?, last);
            (last <= file.new.line_count() && first < last).then(|| {
                (first..last)
                    .map(|ix| file.new.line(ix).into_owned())
                    .collect()
            })
        })
    }

    /// Applies what `pr`'s confirmation shows: GitLab through its API,
    /// elsewhere as one commit pushed to the head branch.
    pub fn confirm_apply(&mut self, pr: &PrKey, cx: &mut Context<Self>) {
        let Some(open) = self.pr(pr) else {
            return;
        };
        if matches!(open.apply, Some(ApplyState::Running(_))) || open.confirm.is_none() {
            return;
        }
        // The button is disabled in this state; this keeps a stray call from
        // applying lines nobody was shown.
        if !suggestions::all_verified(&self.confirm_changes_for(pr)) {
            if let Some(open) = self.pr_mut(pr) {
                open.apply = Some(ApplyState::Failed(
                    "Current lines not loaded, reload the pull request".into(),
                ));
            }
            cx.notify();
            return;
        }
        let services = self.services().cloned();
        let Some(open) = self.pr_mut(pr) else {
            return;
        };
        let Some(confirm) = open.confirm.take() else {
            return;
        };
        let count = confirm.len();
        let forge_pr = open.forge_pr().cloned();
        if forge_pr
            .as_ref()
            .is_some_and(|forge_pr| forge_pr.head.sha.as_str() != confirm.head())
        {
            open.apply = Some(ApplyState::Failed(
                "The pull request changed since the suggestions were shown; apply them again"
                    .into(),
            ));
            cx.notify();
            return;
        }
        let key = pr.clone();
        cx.notify();
        let (Some(pr), Some(services)) = (forge_pr, services) else {
            // Fixtures: nothing to push to.
            open.apply = Some(ApplyState::Failed(
                "Fixtures have no forge to apply to".into(),
            ));
            return;
        };
        open.apply = Some(ApplyState::Running(count));
        let (ids, edits) = (confirm.forge_ids(), confirm.local_edits());
        cx.spawn(async move |this, cx| {
            let result = if ids.is_empty() {
                suggestions::apply_locally(&services, &pr, edits)
                    .await
                    .map(drop)
            } else {
                let (repo, number) = (pr.repo.clone(), pr.number);
                services
                    .forge(&pr.repo.host, move |forge| async move {
                        forge.apply_suggestions(&repo, number, &ids).await
                    })
                    .await
                    .map_err(|error| error.to_string())
            };
            this.update(cx, |session, cx| {
                let Some(open) = session.pr_mut(&key) else {
                    return;
                };
                match result {
                    Ok(()) => {
                        open.batch.clear();
                        open.apply = Some(ApplyState::Applied(count));
                        session.reload(&key, cx);
                    }
                    Err(error) => {
                        let text = format!("Not applied: {error}");
                        open.apply = Some(ApplyState::Failed(text.into()));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Keeps the inbox row of a pull request in step with its snapshot.
    pub(crate) fn patch_row(&mut self, row: PullRequest) {
        let Some(ix) = self.inbox.index_of(&row.key) else {
            return;
        };
        let old = &self.inbox.prs[ix];
        let moved = old.head != row.head || old.base != row.base || old.state != row.state;
        Arc::make_mut(&mut self.inbox).prs[ix] = row;
        if moved {
            self.stacks = inbox_stacks(&self.inbox, &self.source);
        }
    }

    /// Records how long the first diff paint took, once per open; returns
    /// it with the pull request's label.
    pub fn mark_first_paint(&mut self, generation: u64) -> Option<(Duration, String)> {
        let open = self.prs.iter_mut().find(|open| {
            open.diff
                .as_ref()
                .is_some_and(|d| d.generation == generation)
        })?;
        if open.first_paint.is_some() {
            return None;
        }
        let elapsed = open.opened_at.elapsed();
        open.first_paint = Some(elapsed);
        Some((elapsed, open.pr.ref_label()))
    }

    /// Starts the snapshot and git fetch of a pull request the user may open.
    pub fn prefetch(&self, key: &PrKey) {
        let Some(services) = self.services() else {
            return;
        };
        if self.is_open(key) {
            return;
        }
        let base = self
            .inbox
            .get(key)
            .and_then(|pr| pr.source.as_ref())
            .map(|pr| pr.base.name.clone());
        services.prefetch(key, base.as_deref());
    }

    pub fn viewed(&self) -> Option<&Viewed> {
        self.open_pr().map(|open| &open.viewed)
    }

    /// The selected file of the focused pull request.
    pub fn selected_file(&self) -> Option<&SharedString> {
        self.open_pr()?.selected_file.as_ref()
    }

    /// Asks the diff of the pull request with this thread (the focused one
    /// first) to show it.
    pub fn jump_to_thread(&mut self, thread: ThreadId, focus_diff: bool, cx: &mut Context<Self>) {
        let has = |open: &&OpenPr| open.threads.iter().any(|t| t.id == thread);
        let Some(pr) = self
            .open_pr()
            .filter(has)
            .or_else(|| self.prs.iter().find(has))
            .or_else(|| self.open_pr())
            .map(|open| open.key.clone())
        else {
            return;
        };
        cx.emit(SessionEvent::JumpToThread {
            pr,
            thread,
            focus_diff,
        });
    }

    /// Asks the panel `anchor` is written in, of the pull request `pr`, to
    /// show the draft there.
    pub fn jump_to_draft(&mut self, pr: PrKey, anchor: DraftAnchor, cx: &mut Context<Self>) {
        cx.emit(SessionEvent::JumpToDraft { pr, anchor });
    }

    pub fn select_file(&mut self, path: SharedString, cx: &mut Context<Self>) {
        let Some(open) = self.focused_mut() else {
            return;
        };
        let previous = open.selected_file.replace(path.clone());
        let pr = open.key.clone();
        cx.emit(SessionEvent::FileSelected { pr, path, previous });
        cx.notify();
    }

    /// Shows `path` alone in `key`'s diff, or every file for `None`,
    /// without scrolling to it.
    pub fn set_selected_file(
        &mut self,
        key: &PrKey,
        path: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if let Some(open) = self.pr_mut(key)
            && open.selected_file != path
        {
            open.selected_file = path;
            cx.notify();
        }
    }

    /// Shows `commit` of `key` alone in its Diff panel, or all changes for
    /// `None`, as a click on a commit does: the Diff panel comes forward
    /// and Back returns to where the click was.
    pub fn show_commit(
        &mut self,
        key: &PrKey,
        commit: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        let previous = open.shown_commit().cloned();
        if commit.is_some() {
            // A commit is read whole; a file picked in all changes may not be in it.
            open.selected_file = None;
        }
        self.set_shown_commit(key, commit.clone(), cx);
        cx.emit(SessionEvent::CommitShown {
            pr: key.clone(),
            commit,
            previous,
        });
        cx.notify();
    }

    /// Shows `commit` of `key` alone in its diff, or all changes for
    /// `None`, without a jump: Back and Forward, "Show all changes". A
    /// commit not in the pull request's list is ignored.
    pub fn set_shown_commit(
        &mut self,
        key: &PrKey,
        commit: Option<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.pr(key) else {
            return;
        };
        if open.shown_commit() == commit.as_ref() {
            return;
        }
        let view = match commit {
            None => None,
            Some(sha) => {
                let Some(commit) = open
                    .commits
                    .as_ref()
                    .and_then(|commits| commits.list.iter().find(|c| c.sha == sha))
                    .cloned()
                else {
                    return;
                };
                Some(self.start_commit_view(key, commit, cx))
            }
        };
        if let Some(open) = self.pr_mut(key) {
            open.commit_view = view;
        }
        cx.notify();
    }

    /// The view of one commit, its diff built from the fixtures at once or
    /// read from git off the main thread.
    fn start_commit_view(
        &mut self,
        key: &PrKey,
        commit: PrCommit,
        cx: &mut Context<Self>,
    ) -> CommitView {
        let generation = self.generation();
        let mut view = CommitView::new(commit.clone());
        match &self.source {
            Source::Fixtures { fixtures, .. } => {
                let files = fixtures.detail(key).files;
                view.ready(
                    fixtures::commit_diff(key.number.0, &commit, &files),
                    generation,
                );
            }
            Source::Live(Some(services)) => {
                let (objects, repo) = (services.objects.clone(), key.repo.clone());
                let sha = commit.sha.clone();
                let job = services.run_blocking(move || commits::build(&objects, &repo, &sha));
                let key = key.clone();
                view.task = Some(cx.spawn(async move |this, cx| {
                    let built = job.await;
                    this.update(cx, |session, cx| {
                        let Some(view) = session
                            .pr_mut(&key)
                            .and_then(|open| open.commit_view.as_mut())
                            .filter(|view| view.commit.sha == commit.sha)
                        else {
                            return;
                        };
                        match built {
                            Some(Ok(built)) => view.ready(built.diff, generation),
                            Some(Err(error)) => view.error = Some(error.into()),
                            None => view.error = Some("the diff was not built".into()),
                        }
                        cx.notify();
                    })
                    .ok();
                }));
            }
            Source::Live(None) => {}
        }
        view
    }

    pub fn diff_layout(&self) -> Layout {
        self.diff_layout
    }

    pub fn set_diff_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if self.diff_layout != layout {
            self.diff_layout = layout;
            cx.notify();
        }
    }

    pub fn toggle_diff_layout(&mut self, cx: &mut Context<Self>) {
        let next = match self.diff_layout {
            Layout::Unified => Layout::Split,
            Layout::Split => Layout::Unified,
        };
        self.set_diff_layout(next, cx);
    }

    /// Sets viewed on `files` of the focused pull request and writes it to
    /// the cache (off the main thread; the write is not awaited). Where the
    /// forge keeps viewed state (GitHub) it is told too; the local mark
    /// stands even if that fails.
    fn set_viewed(&mut self, files: &[ChangedFile], viewed: bool, cx: &mut Context<Self>) {
        let store = self.services().and_then(|s| s.store.clone());
        let synced = self
            .capabilities()
            .is_some_and(|caps| caps.viewed == forge::ViewedSync::Forge);
        if synced && let (Some(services), Some(open)) = (self.services(), self.open_pr()) {
            let paths: Vec<String> = files.iter().map(|f| f.path.to_string()).collect();
            let key = open.key.clone();
            let (repo, number) = (open.key.repo.clone(), open.key.number);
            let call = services.forge(&repo.host.clone(), move |forge| async move {
                let mut last = Vec::new();
                for path in &paths {
                    last.push(forge.set_viewed(&repo, number, path, viewed).await?);
                }
                Ok(last)
            });
            cx.spawn(async move |this, cx| {
                let result = call.await;
                this.update(cx, |session, cx| match result {
                    Ok(files) => {
                        if let Some(open) = session.pr_mut(&key) {
                            for file in files {
                                open.forge_viewed.insert(file.path, file.state);
                            }
                        }
                    }
                    Err(error) => {
                        session.notice =
                            Some(format!("viewed state not saved on the forge: {error}").into());
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        let Some(open) = self.focused_mut() else {
            return;
        };
        for file in files {
            open.viewed.set(file, viewed);
            open.toggles
                .insert((file.path.clone(), file.blob.clone()), viewed);
        }
        if let Some(store) = store {
            let marks = files
                .iter()
                .map(|f| (f.path.to_string(), f.blob.to_string()))
                .collect();
            store.set_viewed_many(&open.key, marks, viewed).detach();
        }
        cx.notify();
    }

    pub fn toggle_viewed(&mut self, file: &ChangedFile, cx: &mut Context<Self>) {
        let viewed = self.viewed().is_some_and(|v| v.contains(file));
        self.set_viewed(std::slice::from_ref(file), !viewed, cx);
    }

    pub fn toggle_viewed_all(&mut self, files: &[ChangedFile], cx: &mut Context<Self>) {
        let mut viewed = self.viewed().cloned().unwrap_or_default();
        let on = viewed.toggle_all(files);
        self.set_viewed(files, on, cx);
    }

    /// `v`: toggles viewed on the selected file of the focused pull request.
    pub fn toggle_selected_viewed(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.selected_file().cloned() else {
            return;
        };
        let Some(file) = self
            .open_pr()
            .and_then(|open| open.files.iter().find(|f| f.path == path))
            .cloned()
        else {
            return;
        };
        self.toggle_viewed(&file, cx);
    }
}

impl Session {
    // ---- CI

    /// Reads `key`'s runs again (after a watch event or a re-run). The old
    /// ones stay on screen until the answer lands.
    pub fn refresh_ci(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(services) = self.services().cloned() else {
            return;
        };
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        let Some(head) = open.forge_pr().map(|pr| pr.head.sha.clone()) else {
            return;
        };
        if open.ci.load != CiLoad::Ready {
            open.ci.load = CiLoad::Loading;
        }
        let debounce = open.ci.head.is_some();
        open.ci.head = Some(head.clone());
        let key = open.key.clone();
        open.ci.task = Some(cx.spawn(async move |this, cx| {
            // A burst of events from one poll is one read.
            if debounce {
                cx.background_executor().timer(CI_DEBOUNCE).await;
            }
            let read = this
                .update(cx, |_, cx| start_ci_read(&services, &key, &head, cx))
                .ok();
            if let Some(read) = read {
                read.await;
            }
        }));
        cx.notify();
    }

    fn ci_read(
        &mut self,
        key: &PrKey,
        head: &Sha,
        result: forge::Result<Vec<CiRun>>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        if open.ci.head.as_ref() != Some(head) {
            return;
        }
        let was_active = open.ci.runs.iter().any(|run| run.state.is_active());
        match result {
            Ok(runs) => {
                if !runs.is_empty() {
                    open.checks = ci::checks_of(&runs);
                }
                if let Some(log) = open.ci.log.as_mut()
                    && let Some(job) = runs
                        .iter()
                        .flat_map(|run| &run.jobs)
                        .find(|job| job.id == log.job.id)
                {
                    log.job = job.clone();
                }
                open.ci.runs = runs;
                open.ci.load = CiLoad::Ready;
            }
            Err(forge::Error::Unsupported { .. }) => open.ci.load = CiLoad::Unsupported,
            Err(error) => open.ci.load = CiLoad::Failed(error.to_string().into()),
        }
        // A failed read keeps the runs it had, so polling goes on through it.
        let active = open.ci.runs.iter().any(|run| run.state.is_active());
        open.ci.poll = active.then(|| {
            let key = key.clone();
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CI_ACTIVE_POLL).await;
                this.update(cx, |session, cx| session.refresh_ci(&key, cx))
                    .ok();
            })
        });
        // The last run finished: the row's rollup and the header follow.
        if was_active && !active {
            self.refresh_pr(key, cx);
        }
        cx.notify();
    }

    /// Asks for a re-run; one that would start more than one job waits
    /// for [`Self::confirm_retry`].
    pub fn request_retry(&mut self, retry: Retry, cx: &mut Context<Self>) {
        let Some(open) = self.focused_mut() else {
            return;
        };
        let Some(run) = open.ci.runs.iter().find(|run| &run.id == retry.run()) else {
            return;
        };
        let jobs = ci::retry_jobs(run, &retry);
        match jobs.len() {
            0 => {}
            1 => self.send_retry(retry, jobs, cx),
            _ => {
                open.ci.confirm = Some(RetryConfirm {
                    retry,
                    run_name: run.name.clone().into(),
                    jobs,
                });
                cx.notify();
            }
        }
    }

    pub fn confirm_retry(&mut self, cx: &mut Context<Self>) {
        let Some(confirm) = self.focused_mut().and_then(|open| open.ci.confirm.take()) else {
            return;
        };
        self.send_retry(confirm.retry, confirm.jobs, cx);
    }

    pub fn cancel_retry(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.focused_mut() {
            open.ci.confirm = None;
        }
        cx.notify();
    }

    fn send_retry(&mut self, retry: Retry, jobs: Vec<SharedString>, cx: &mut Context<Self>) {
        let services = self.services().cloned();
        let Some(open) = self.focused_mut() else {
            return;
        };
        let names = jobs.join(", ");
        let Some(services) = services else {
            // Fixtures: nothing to send; the jobs show as queued again.
            for run in open.ci.runs.iter_mut().filter(|run| &run.id == retry.run()) {
                run.state = CiState::Queued;
                for job in run
                    .jobs
                    .iter_mut()
                    .filter(|job| jobs.iter().any(|name| *name == job.name))
                {
                    job.state = CiState::Queued;
                    job.attempt = Some(job.attempt.unwrap_or(1) + 1);
                    job.started_at = None;
                    job.completed_at = None;
                }
            }
            open.ci.notice = Some(CiNotice {
                text: format!("Re-run queued: {names}").into(),
                error: false,
            });
            cx.notify();
            return;
        };
        open.ci.notice = Some(CiNotice {
            text: format!("Starting a re-run of {names}").into(),
            error: false,
        });
        let key = open.key.clone();
        let repo = key.repo.clone();
        let call = services.forge(&key.repo.host, move |forge| async move {
            match retry {
                Retry::Job { job, .. } => forge.retry_job(&repo, &job).await,
                Retry::FailedJobs { run } => forge.retry_failed_jobs(&repo, &run).await,
            }
        });
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |session, cx| {
                let Some(open) = session.pr_mut(&key) else {
                    return;
                };
                open.ci.notice = Some(match &result {
                    Ok(()) => CiNotice {
                        text: format!("Re-run started: {names}").into(),
                        error: false,
                    },
                    Err(error) => CiNotice {
                        text: format!("Not re-run: {error}").into(),
                        error: true,
                    },
                });
                if result.is_ok() {
                    session.refresh_ci(&key, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// Shows a job's log in the Log panel, scrolled to `step` when given.
    pub fn open_log(&mut self, job: &JobId, step: Option<u32>, cx: &mut Context<Self>) {
        let generation = self.generation();
        let Some(open) = self.focused_mut() else {
            return;
        };
        let Some((run, found)) = open
            .ci
            .runs
            .iter()
            .find_map(|run| Some((run, run.jobs.iter().find(|j| &j.id == job)?)))
        else {
            return;
        };
        let same = open
            .ci
            .log
            .as_ref()
            .is_some_and(|log| log.job.id == found.id && log.job.attempt == found.attempt);
        let step = step.map(|number| (number, generation));
        let target = LogTarget {
            repo: open.key.repo.clone(),
            forge: open.pr.forge,
            run_name: run.name.clone().into(),
            job: found.clone(),
            generation,
            step,
        };
        match open.ci.log.as_mut() {
            Some(log) if same => log.step = step.or(log.step),
            _ => open.ci.log = Some(target),
        }
        cx.emit(SessionEvent::Show(ShowPanel {
            pr: None,
            kind: PanelKind::Log,
            replace: None,
            beside: false,
        }));
        cx.notify();
    }

    /// Opens or folds `key`'s list of runs in its pipelines card.
    pub fn toggle_pipelines(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if let Some(open) = self.pr_mut(key) {
            open.ci.listed = !open.ci.listed;
        }
        cx.notify();
    }

    /// Brings `key`'s Conversation forward with its pipelines card open.
    pub fn show_pipelines(&mut self, key: &PrKey, mode: OpenMode, cx: &mut Context<Self>) {
        self.show(key, PanelKind::Conversation, mode, cx);
        if let Some(open) = self.pr_mut(key) {
            open.ci.listed = true;
        }
        cx.notify();
    }

    pub fn toggle_run_folded(&mut self, run: &RunId, cx: &mut Context<Self>) {
        if let Some(open) = self.focused_mut()
            && !open.ci.folded.remove(run)
        {
            open.ci.folded.insert(run.clone());
        }
        cx.notify();
    }

    pub fn toggle_job_steps(&mut self, job: &JobId, cx: &mut Context<Self>) {
        if let Some(open) = self.focused_mut()
            && !open.ci.expanded.remove(job)
        {
            open.ci.expanded.insert(job.clone());
        }
        cx.notify();
    }

    pub fn dismiss_ci_notice(&mut self, cx: &mut Context<Self>) {
        if let Some(open) = self.focused_mut() {
            open.ci.notice = None;
        }
        cx.notify();
    }

    /// A fixture job's invented log; `None` on a live run.
    pub fn fixture_log(&self, job: &CiJob) -> Option<Vec<u8>> {
        match &self.source {
            Source::Fixtures { .. } => Some(fixtures::job_log(job)),
            Source::Live(_) => None,
        }
    }

    /// Brings the focused pull request's `kind` panel forward, building it
    /// when missing.
    pub fn show_panel(&mut self, kind: PanelKind, cx: &mut Context<Self>) {
        let pr = match kind {
            PanelKind::Log => None,
            _ => match self.focused.clone() {
                Some(key) => Some(key),
                None => return,
            },
        };
        cx.emit(SessionEvent::Show(ShowPanel {
            pr,
            kind,
            replace: None,
            beside: false,
        }));
    }

    // ---- watch events

    /// The watch engine saw the rollup move.
    pub fn patch_row_ci(&mut self, key: &PrKey, state: CiState, cx: &mut Context<Self>) {
        if let Some(ix) = self.inbox.index_of(key)
            && self.inbox.prs[ix].ci != state
        {
            Arc::make_mut(&mut self.inbox).prs[ix].ci = state;
        }
        if let Some(open) = self.pr_mut(key) {
            open.pr.ci = state;
        }
        self.settle_ci(cx);
        cx.notify();
    }

    /// The CI state `key` shows: its inbox row's, else the open pull
    /// request's. A stack member draws this, so it never disagrees with
    /// the row.
    #[must_use]
    pub fn shown_ci(&self, key: &PrKey) -> Option<CiState> {
        self.inbox
            .get(key)
            .map(|row| row.ci)
            .or_else(|| self.pr(key).map(|open| open.pr.ci))
    }

    /// Starts reading rows with active CI again every [`SETTLE_POLL`],
    /// unless that is running or nothing is active.
    fn settle_ci(&mut self, cx: &mut Context<Self>) {
        if self.settling || self.services().is_none() || self.settle_reads().is_empty() {
            return;
        }
        self.settling = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(SETTLE_POLL).await;
                let next = this.update(cx, |session, _| session.next_settle_reads());
                let Ok(Some((services, reads))) = next else {
                    return;
                };
                let calls = reads.iter().map(|read| {
                    let (repo, numbers) = (read.repo.clone(), read.numbers.clone());
                    services.forge(&read.repo.host, move |forge| async move {
                        forge.ci_states(&repo, &numbers).await
                    })
                });
                let answers = futures::future::join_all(calls).await;
                let applied = this.update(cx, |session, cx| {
                    session.settle_answers(&reads, answers, cx);
                });
                if applied.is_err() {
                    return;
                }
            }
        })
        .detach();
    }

    /// The rows to read now; `None`, and the loop ends, when none is active.
    fn next_settle_reads(&mut self) -> Option<(Services, Vec<SettleRead>)> {
        let reads = self.settle_reads();
        let services = self.services().cloned().filter(|_| !reads.is_empty());
        self.settling = services.is_some();
        Some((services?, reads))
    }

    fn settle_reads(&self) -> Vec<SettleRead> {
        let rows = self.inbox.prs.iter().map(|row| (&row.key, row.ci));
        let open = self.prs.iter().map(|open| (&open.key, open.pr.ci));
        ci_settle::reads(rows.chain(open))
    }

    /// A failed read changes nothing; the next round asks again.
    fn settle_answers(
        &mut self,
        reads: &[SettleRead],
        answers: Vec<forge::Result<Vec<CiState>>>,
        cx: &mut Context<Self>,
    ) {
        for (read, answer) in reads.iter().zip(answers) {
            let Ok(states) = answer else {
                continue;
            };
            for (key, state) in ci_settle::answered(read, &states) {
                let shown = self.inbox.get(&key).map(|row| row.ci);
                let open = self.pr(&key).map(|open| open.pr.ci);
                if shown.is_some_and(|ci| ci != state) || open.is_some_and(|ci| ci != state) {
                    self.patch_row_ci(&key, state, cx);
                }
            }
        }
    }

    /// Comments, reviews or the state changed: reads `key`'s snapshot
    /// again, without the diff.
    pub fn refresh_pr(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if !self.is_open(key) {
            return;
        }
        let Some(services) = self.services().cloned() else {
            return;
        };
        services.forget_snapshot(key);
        let shared = services.snapshot(key);
        self.refresh_lifecycle(key, cx);
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let Ok(snapshot) = shared.await else {
                return;
            };
            this.update(cx, |session, cx| {
                let update = OpenUpdate::Snapshot {
                    snapshot,
                    fresh: true,
                };
                session.apply(&key, update, cx);
            })
            .ok();
        })
        .detach();
    }

    /// New commits on an open pull request: opens it again at its new head.
    pub fn head_moved(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if self.is_open(key) {
            self.reload(key, cx);
        }
    }
}

/// Inbox rows: actions on a pull request without opening it in a tab, and
/// refetching the inbox.
impl Session {
    /// The local cache, where drafts, viewed files and review marks live.
    pub fn store(&self) -> Option<&store::Store> {
        self.store.as_ref()
    }

    /// Loads `key` for an action from its inbox row: no panel, and the focus
    /// stays where it is. [`Self::release_background`] drops it again; a
    /// [`Self::show`] makes it an ordinary open one.
    pub fn load_quietly(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if self.is_open(key) {
            return;
        }
        let focused = self.focused.clone();
        self.open(key, cx);
        self.focused = focused;
        self.background.insert(key.clone());
        cx.notify();
    }

    /// Whether `key` is loaded only for a row's action.
    pub fn is_background(&self, key: &PrKey) -> bool {
        self.background.contains(key)
    }

    /// The pull requests loaded only for a row's action.
    pub fn background_keys(&self) -> Vec<PrKey> {
        self.background.iter().cloned().collect()
    }

    /// Drops `key` if it was loaded only for a row's action and nothing
    /// shows it now.
    pub fn release_background(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let shown = self
            .panels
            .iter()
            .any(|entry| entry.pr.as_ref() == Some(key))
            || self.focused.as_ref() == Some(key);
        if self.background.contains(key) && !shown {
            self.release(key, cx);
        }
    }

    /// What `key`'s forge can do, open or not.
    pub fn capabilities_of(&self, key: &PrKey, forge: ForgeKind) -> Capabilities {
        self.services()
            .and_then(|services| services.capabilities(&key.repo.host))
            .unwrap_or_else(|| Capabilities::baseline(forge))
    }

    /// The forge's record of `key`: the open pull request's, the inbox
    /// row's, or a fixture's.
    pub fn forge_record(&self, key: &PrKey) -> Option<forge::model::PullRequest> {
        if let Some(pr) = self.pr(key).and_then(OpenPr::forge_pr) {
            return Some(pr.clone());
        }
        if let Some(pr) = self.inbox.get(key).and_then(|row| row.source.as_deref()) {
            return Some(pr.clone());
        }
        match &self.source {
            Source::Fixtures { fixtures, .. } => fixtures.forge_pr(key),
            Source::Live(_) => None,
        }
    }

    /// What is known of merging `key` without loading it: the open pull
    /// request's state, or what fixtures say. `None` until a live one loads.
    pub fn merge_preview(&self, key: &PrKey) -> Option<Lifecycle> {
        if let Some(open) = self.pr(key) {
            return Some(open.lifecycle.clone());
        }
        match &self.source {
            Source::Fixtures { fixtures, .. } => Some(Lifecycle::fixture(
                fixtures.forge_pr(key).as_ref(),
                &fixtures.detail(key).runs,
            )),
            Source::Live(_) => None,
        }
    }

    /// Takes the function that refetches every inbox section.
    pub fn set_inbox_refresher(&mut self, refresher: InboxRefresher) {
        self.refresher = Some(refresher);
    }

    /// Takes the function that stops listing a repository for good.
    pub fn set_repo_remover(&mut self, remover: RepoRemover) {
        self.repo_remover = Some(remover);
    }

    /// The function that stops listing a repository for good, once a live
    /// start has set it.
    #[must_use]
    pub fn repo_remover(&self) -> Option<RepoRemover> {
        self.repo_remover.clone()
    }

    /// Takes `repo`'s sections out of the inbox now.
    pub fn drop_repo_sections(&mut self, repo: &RepoRef, cx: &mut Context<Self>) {
        let mut inbox = Inbox::clone(&self.inbox);
        inbox
            .sections
            .retain(|section| section.repo.as_ref() != Some(repo));
        self.set_inbox(inbox, cx);
    }

    /// Whether the inbox can be refetched by hand now.
    pub fn can_refresh_inbox(&self) -> bool {
        self.refresher.is_some()
    }

    /// After a review went out: the pull request is read again now, and the
    /// inbox after [`INBOX_AFTER_REVIEW`], which gives the forges' search
    /// indexes time to see the new review state.
    fn review_submitted(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        self.refresh_pr(key, cx);
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(INBOX_AFTER_REVIEW).await;
            this.update(cx, |session, cx| session.refresh_inbox(cx))
                .ok();
        })
        .detach();
    }

    /// Refetches every inbox section now; the rows are patched in place as
    /// the answers land. One at a time.
    pub fn refresh_inbox(&mut self, cx: &mut Context<Self>) {
        let Some(refresher) = self.refresher.clone() else {
            return;
        };
        if self.inbox_refresh.running {
            return;
        }
        self.inbox_refresh.running = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let round = cx.update(|cx| refresher(cx));
            round.await;
            this.update(cx, |session, cx| session.inbox_refreshed(cx))
                .ok();
        })
        .detach();
    }

    fn inbox_refreshed(&mut self, cx: &mut Context<Self>) {
        let now = chrono::Utc::now();
        let refresh = &mut self.inbox_refresh;
        refresh.running = false;
        for forge in &self.forges {
            match refresh
                .last
                .iter_mut()
                .find(|(host, _)| *host == forge.host)
            {
                Some((_, at)) => *at = now,
                None => refresh.last.push((forge.host.clone(), now)),
            }
        }
        cx.notify();
    }
}

/// Wait before a CI read that an event asked for.
const CI_DEBOUNCE: Duration = Duration::from_millis(250);
/// How often an open pull request's CI is read again while a run is queued
/// or running, watched or not.
pub const CI_ACTIVE_POLL: Duration = Duration::from_secs(30);
/// How long after a review the inbox is refetched.
pub const INBOX_AFTER_REVIEW: Duration = Duration::from_secs(5);

/// Reads `head`'s runs and hands them to [`Session::ci_read`].
fn start_ci_read(
    services: &Services,
    key: &PrKey,
    head: &Sha,
    cx: &mut Context<Session>,
) -> Task<()> {
    let (repo, sha) = (key.repo.clone(), head.clone());
    let call = services.forge(&key.repo.host, move |forge| async move {
        forge.ci_runs(&repo, &sha).await
    });
    let (key, head) = (key.clone(), head.clone());
    cx.spawn(async move |this, cx| {
        let result = call.await;
        this.update(cx, |session, cx| session.ci_read(&key, &head, result, cx))
            .ok();
    })
}

/// The pull request `step` moves to from `current`, in the stack's listed
/// order (top first; `members` are bottom first).
fn stack_neighbor(stack: &Stack, current: &PrKey, step: StackStep) -> Option<PrKey> {
    let at = stack.members.iter().position(|m| &m.key == current)?;
    let target = match step {
        StackStep::Down => at.checked_sub(1)?,
        StackStep::Up => at + 1,
    };
    stack.members.get(target).map(|m| m.key.clone())
}

/// A row for a pull request that is not in the inbox; the snapshot fills it.
fn placeholder(key: &PrKey, forge: ForgeKind) -> PullRequest {
    PullRequest {
        key: key.clone(),
        forge,
        title: SharedString::default(),
        author: SharedString::default(),
        author_avatar: None,
        reviewers: Vec::new(),
        reviewer_avatars: Vec::new(),
        head: SharedString::default(),
        base: SharedString::default(),
        sha: SharedString::default(),
        ci: forge::model::CiState::None,
        review: ReviewState::Pending,
        state: forge::model::PrState::Open,
        updated_at: chrono::Utc::now(),
        description: SharedString::default(),
        stat: None,
        comments: None,
        unresolved: None,
        source: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CiState, PrNumber, RepoRef, StackMember};

    fn key(n: u64) -> PrKey {
        PrKey::new(RepoRef::new("github.com", "a", "b"), PrNumber(n))
    }

    fn file(path: &str, blob: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_owned().into(),
            blob: blob.to_owned().into(),
            status: crate::model::FileStatus::Modified,
            added: 1,
            removed: 0,
        }
    }

    #[test]
    fn forge_viewed_marks_join_local_ones_and_toggles_win() {
        let mut open = OpenPr::new(placeholder(&key(1), ForgeKind::GitHub));
        open.files = vec![file("a", "1"), file("b", "2"), file("c", "3")];
        open.forge_viewed = [
            ("a".to_owned(), ViewedState::Viewed),
            ("b".to_owned(), ViewedState::Dismissed),
            ("c".to_owned(), ViewedState::Viewed),
        ]
        .into_iter()
        .collect();
        open.toggles.insert(("c".into(), "3".into()), false);
        open.refresh_viewed();
        let viewed: Vec<bool> = open.files.iter().map(|f| open.viewed.contains(f)).collect();
        // Viewed on the forge; changed since (dismissed); unmarked here.
        assert_eq!(viewed, [true, false, false]);
    }

    #[test]
    fn stack_steps_follow_the_list_top_first() {
        let member = |n: u64| StackMember {
            key: key(n),
            title: SharedString::default(),
            author: SharedString::default(),
            author_avatar: None,
            ci: CiState::None,
            head: format!("b{n}").into(),
            base: format!("b{}", n - 1).into(),
        };
        // Bottom first: 1 sits on the trunk, 3 is the top.
        let stack = Stack {
            members: vec![member(1), member(2), member(3)],
            base: "main".into(),
        };
        assert_eq!(
            stack_neighbor(&stack, &key(3), StackStep::Down),
            Some(key(2))
        );
        assert_eq!(
            stack_neighbor(&stack, &key(2), StackStep::Down),
            Some(key(1))
        );
        assert_eq!(stack_neighbor(&stack, &key(1), StackStep::Down), None);
        assert_eq!(stack_neighbor(&stack, &key(1), StackStep::Up), Some(key(2)));
        assert_eq!(stack_neighbor(&stack, &key(3), StackStep::Up), None);
        assert_eq!(stack_neighbor(&stack, &key(9), StackStep::Up), None);
    }
}
