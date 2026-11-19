//! Subscriptions to a pull request or a CI run, polled on an adaptive
//! interval, turned into typed change events. No UI here: the app drains
//! [`Change`]s and raises toasts and badges.
//!
//! Intervals: [`FAST`] while the watched CI is active,
//! [`WARM`] right after a change, then doubling on every quiet poll up to
//! [`SLOW`]. A pull request is polled through
//! [`Forge::pull_request_pulse`] (one to three requests, conditional where
//! the forge sends ETags); only when its pulse moved is the full
//! [`Forge::snapshot`] read to find the new comments and reviews. A run is
//! polled with [`Forge::ci_run`] at [`CiDetail::Jobs`].
//!
//! [`Watcher`] takes its [`Clock`], so a test runs a simulated hour against
//! a mock transport in no time; [`crate::ForgeHub::watch`] runs one on the
//! forge thread for the app.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinSet;

use crate::error::Error;
use crate::forge::Forge;
use crate::http::Clock;
use crate::model::{
    CiDetail, CiRun, CiState, Comment, CommentId, JobId, PrNumber, PrState, PullRequestPulse,
    RepoRef, ReviewState, RunId, Sha, Snapshot, ThreadId, User,
};

/// While a watched run or a pull request's CI is queued or running.
pub const FAST: Duration = Duration::from_secs(10);
/// Right after something changed with no CI active.
pub const WARM: Duration = Duration::from_secs(30);
/// The idle ceiling the quiet back-off reaches.
pub const SLOW: Duration = Duration::from_secs(5 * 60);

/// What a subscription follows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum WatchTarget {
    PullRequest { repo: RepoRef, number: PrNumber },
    Run { repo: RepoRef, run: RunId },
}

impl WatchTarget {
    #[must_use]
    pub fn repo(&self) -> &RepoRef {
        match self {
            Self::PullRequest { repo, .. } | Self::Run { repo, .. } => repo,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SubscriptionId(pub u64);

/// One thing a person would be told about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchEvent {
    /// The pull request's check rollup, or a watched run's state.
    CiStateChanged {
        from: CiState,
        to: CiState,
    },
    /// One job of a watched run, by name (a retry may give it a new id).
    JobStateChanged {
        job: JobId,
        name: String,
        from: CiState,
        to: CiState,
    },
    /// A comment that was not there before, the viewer's own included (the
    /// app knows who it is and filters).
    NewComment {
        thread: ThreadId,
        comment: Box<Comment>,
    },
    /// A review with a verdict: from its summary comment, or from a
    /// reviewer's state when the review has no body.
    ReviewSubmitted {
        author: User,
        state: ReviewState,
    },
    /// New commits on the head branch.
    HeadChanged {
        from: Sha,
        to: Sha,
    },
    Merged,
    Closed,
    Reopened,
    /// The first failed poll of a streak; later ones are quiet until a poll
    /// succeeds again.
    PollFailed(Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub subscription: SubscriptionId,
    pub target: WatchTarget,
    pub event: WatchEvent,
}

/// What the last successful poll saw.
#[derive(Clone, Debug)]
enum Known {
    PullRequest {
        pulse: PullRequestPulse,
        comments: HashSet<CommentId>,
        /// Login to the latest review state.
        reviews: HashMap<String, ReviewState>,
    },
    Run {
        state: CiState,
        /// Job name to its state.
        jobs: HashMap<String, CiState>,
    },
}

struct Subscription {
    id: SubscriptionId,
    target: WatchTarget,
    forge: Arc<dyn Forge>,
    /// Clock time of the next poll.
    due: Duration,
    interval: Duration,
    known: Option<Known>,
    failing: bool,
}

/// The polling engine. Drive it with [`Self::run_until`], or step it with
/// [`Self::next_due`] and [`Self::poll_due`].
pub struct Watcher {
    clock: Arc<dyn Clock>,
    subscriptions: Vec<Subscription>,
    next_id: u64,
}

impl Watcher {
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            subscriptions: Vec::new(),
            next_id: 1,
        }
    }

    /// Follows `target` through `forge`; the first poll is due at once and
    /// only records a baseline (no events).
    pub fn subscribe(&mut self, forge: Arc<dyn Forge>, target: WatchTarget) -> SubscriptionId {
        let id = SubscriptionId(self.next_id);
        self.insert(id, forge, target);
        id
    }

    pub(crate) fn insert(
        &mut self,
        id: SubscriptionId,
        forge: Arc<dyn Forge>,
        target: WatchTarget,
    ) {
        self.next_id = self.next_id.max(id.0 + 1);
        self.subscriptions.push(Subscription {
            id,
            target,
            forge,
            due: self.clock.now(),
            interval: WARM,
            known: None,
            failing: false,
        });
    }

    /// Whether it was subscribed.
    pub fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        let before = self.subscriptions.len();
        self.subscriptions.retain(|sub| sub.id != id);
        self.subscriptions.len() != before
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty()
    }

    /// The interval a subscription is on now.
    #[must_use]
    pub fn interval(&self, id: SubscriptionId) -> Option<Duration> {
        self.subscriptions
            .iter()
            .find(|sub| sub.id == id)
            .map(|sub| sub.interval)
    }

    /// Clock time of the earliest poll due.
    #[must_use]
    pub fn next_due(&self) -> Option<Duration> {
        self.subscriptions.iter().map(|sub| sub.due).min()
    }

    /// Polls every subscription that is due, concurrently, and returns what
    /// changed, in subscription order.
    pub async fn poll_due(&mut self) -> Vec<Change> {
        let now = self.clock.now();
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.subscriptions)
            .into_iter()
            .partition(|sub| sub.due <= now);
        self.subscriptions = waiting;
        let mut polls = JoinSet::new();
        for sub in due {
            polls.spawn(poll(sub));
        }
        let mut changes = Vec::new();
        while let Some(joined) = polls.join_next().await {
            // A poll only panics on a bug; that subscription is dropped.
            let Ok((mut sub, outcome)) = joined else {
                continue;
            };
            let now = self.clock.now();
            let (events, wait) = match outcome {
                Ok(Polled { events, interval }) => {
                    sub.failing = false;
                    sub.interval = interval;
                    (events, interval)
                }
                Err(error) => {
                    let wait = match &error {
                        Error::RateLimited {
                            retry_after: Some(after),
                            ..
                        } => (*after).max(sub.interval),
                        _ => sub.interval.saturating_mul(2).min(SLOW),
                    };
                    sub.interval = wait;
                    let first = !std::mem::replace(&mut sub.failing, true);
                    (
                        first
                            .then_some(WatchEvent::PollFailed(error))
                            .into_iter()
                            .collect(),
                        wait,
                    )
                }
            };
            sub.due = now.saturating_add(wait);
            changes.extend(events.into_iter().map(|event| Change {
                subscription: sub.id,
                target: sub.target.clone(),
                event,
            }));
            self.subscriptions.push(sub);
        }
        self.subscriptions.sort_by_key(|sub| sub.id);
        changes.sort_by_key(|change| change.subscription);
        changes
    }

    /// Polls as subscriptions come due until the clock passes `deadline`
    /// (clock time), handing every change to `on_change`.
    pub async fn run_until(&mut self, deadline: Duration, mut on_change: impl FnMut(Change)) {
        while let Some(due) = self.next_due() {
            if due > deadline {
                break;
            }
            let now = self.clock.now();
            if due > now {
                self.clock.sleep(due - now).await;
            }
            for change in self.poll_due().await {
                on_change(change);
            }
        }
    }
}

struct Polled {
    events: Vec<WatchEvent>,
    interval: Duration,
}

async fn poll(mut sub: Subscription) -> (Subscription, Result<Polled, Error>) {
    let result = match sub.target.clone() {
        WatchTarget::PullRequest { repo, number } => {
            poll_pull_request(&mut sub, &repo, number).await
        }
        WatchTarget::Run { repo, run } => poll_run(&mut sub, &repo, &run).await,
    };
    (sub, result)
}

/// The next interval: fast while CI is active, warm after a change, else
/// the quiet back-off.
fn next_interval(previous: Duration, active: bool, changed: bool) -> Duration {
    if active {
        FAST
    } else if changed || previous <= FAST {
        WARM
    } else {
        previous.saturating_mul(2).min(SLOW)
    }
}

/// A new subscription starts warm, not slow: CI often starts right after a
/// push or an open, and the back-off reaches [`SLOW`] within minutes anyway.
fn first_interval(active: bool) -> Duration {
    if active { FAST } else { WARM }
}

async fn poll_pull_request(
    sub: &mut Subscription,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Polled, Error> {
    let pulse = sub.forge.pull_request_pulse(repo, number).await?;
    let active = pulse.ci.is_active();
    let Some(Known::PullRequest {
        pulse: before,
        comments,
        reviews,
    }) = &sub.known
    else {
        let snapshot = sub.forge.snapshot(repo, number).await?;
        sub.known = Some(known_pull_request(pulse, &snapshot));
        return Ok(Polled {
            events: Vec::new(),
            interval: first_interval(active),
        });
    };
    if *before == pulse {
        return Ok(Polled {
            events: Vec::new(),
            interval: next_interval(sub.interval, active, false),
        });
    }
    let mut events = Vec::new();
    if before.ci != pulse.ci {
        events.push(WatchEvent::CiStateChanged {
            from: before.ci,
            to: pulse.ci,
        });
    }
    if before.head != pulse.head {
        events.push(WatchEvent::HeadChanged {
            from: before.head.clone(),
            to: pulse.head.clone(),
        });
    }
    if before.state != pulse.state {
        match pulse.state {
            PrState::Merged => events.push(WatchEvent::Merged),
            PrState::Closed => events.push(WatchEvent::Closed),
            PrState::Open | PrState::Draft if !before.state.is_open() => {
                events.push(WatchEvent::Reopened);
            }
            PrState::Open | PrState::Draft => {}
        }
    }
    let known = if before.updated_at == pulse.updated_at {
        Known::PullRequest {
            pulse,
            comments: comments.clone(),
            reviews: reviews.clone(),
        }
    } else {
        let snapshot = sub.forge.snapshot(repo, number).await?;
        events.extend(discussion_events(comments, reviews, &snapshot));
        known_pull_request(pulse, &snapshot)
    };
    sub.known = Some(known);
    Ok(Polled {
        events,
        interval: next_interval(sub.interval, active, true),
    })
}

fn known_pull_request(pulse: PullRequestPulse, snapshot: &Snapshot) -> Known {
    Known::PullRequest {
        pulse,
        comments: snapshot
            .threads
            .iter()
            .flat_map(|thread| &thread.comments)
            .map(|comment| comment.id.clone())
            .collect(),
        reviews: snapshot
            .pull_request
            .reviewers
            .iter()
            .map(|reviewer| (reviewer.user.login.clone(), reviewer.state))
            .collect(),
    }
}

/// New comments, and verdicts: a new summary comment with one, or a
/// reviewer whose state moved to a verdict without such a comment.
fn discussion_events(
    comments: &HashSet<CommentId>,
    reviews: &HashMap<String, ReviewState>,
    snapshot: &Snapshot,
) -> Vec<WatchEvent> {
    let mut events = Vec::new();
    let mut reported: HashSet<&str> = HashSet::new();
    for thread in &snapshot.threads {
        for comment in thread.comments.iter().filter(|c| !comments.contains(&c.id)) {
            match comment.verdict {
                Some(state) if is_verdict(state) => {
                    reported.insert(comment.author.login.as_str());
                    events.push(WatchEvent::ReviewSubmitted {
                        author: comment.author.clone(),
                        state,
                    });
                    if !comment.body.trim().is_empty() {
                        events.push(WatchEvent::NewComment {
                            thread: thread.id.clone(),
                            comment: Box::new(comment.clone()),
                        });
                    }
                }
                _ => events.push(WatchEvent::NewComment {
                    thread: thread.id.clone(),
                    comment: Box::new(comment.clone()),
                }),
            }
        }
    }
    for reviewer in &snapshot.pull_request.reviewers {
        let login = reviewer.user.login.as_str();
        if is_verdict(reviewer.state)
            && reviews.get(login) != Some(&reviewer.state)
            && !reported.contains(login)
        {
            events.push(WatchEvent::ReviewSubmitted {
                author: reviewer.user.clone(),
                state: reviewer.state,
            });
        }
    }
    events
}

fn is_verdict(state: ReviewState) -> bool {
    matches!(
        state,
        ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Commented
    )
}

async fn poll_run(sub: &mut Subscription, repo: &RepoRef, run: &RunId) -> Result<Polled, Error> {
    let read = sub.forge.ci_run(repo, run, CiDetail::Jobs).await?;
    let active = read.state.is_active() || read.jobs.iter().any(|job| job.state.is_active());
    let known = known_run(&read);
    let Some(Known::Run { state, jobs }) = sub.known.replace(known) else {
        return Ok(Polled {
            events: Vec::new(),
            interval: first_interval(active),
        });
    };
    let mut events = Vec::new();
    if state != read.state {
        events.push(WatchEvent::CiStateChanged {
            from: state,
            to: read.state,
        });
    }
    for job in &read.jobs {
        let from = jobs.get(&job.name).copied().unwrap_or(CiState::None);
        if from != job.state {
            events.push(WatchEvent::JobStateChanged {
                job: job.id.clone(),
                name: job.name.clone(),
                from,
                to: job.state,
            });
        }
    }
    let changed = !events.is_empty();
    Ok(Polled {
        events,
        interval: next_interval(sub.interval, active, changed),
    })
}

fn known_run(run: &CiRun) -> Known {
    Known::Run {
        state: run.state,
        jobs: run
            .jobs
            .iter()
            .map(|job| (job.name.clone(), job.state))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::Capabilities;
    use crate::error::Result;
    use crate::http::ManualClock;
    use crate::model::{
        BranchRef, CheckRun, CiJob, ForgeKind, InboxSection, PullRequest, PullRequestId,
        Resolution, ReviewDecision, Reviewer, Stack, StackSource, Thread, Timestamp,
    };

    fn at(minute: u32) -> Timestamp {
        Utc.with_ymd_and_hms(2026, 11, 17, 12, minute, 0).unwrap()
    }

    fn repo() -> RepoRef {
        RepoRef::new("forge.test", "o", "r")
    }

    fn comment(id: &str, login: &str, verdict: Option<ReviewState>) -> Comment {
        Comment {
            id: CommentId::new(id),
            author: User::new(login),
            body: format!("comment {id}"),
            created_at: at(0),
            edited_at: None,
            web_url: None,
            review: None,
            verdict,
            suggestions: Vec::new(),
        }
    }

    /// The pull request as the fake forge serves it; tests edit it between
    /// polls.
    #[derive(Clone)]
    struct State {
        pulse: PullRequestPulse,
        comments: Vec<Comment>,
        reviewers: Vec<Reviewer>,
        run: CiRun,
        fail: bool,
    }

    struct Fake {
        state: Mutex<State>,
        pulses: AtomicUsize,
        snapshots: AtomicUsize,
        runs: AtomicUsize,
    }

    impl Fake {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                state: Mutex::new(State {
                    pulse: PullRequestPulse {
                        state: PrState::Open,
                        head: Sha::new("a".repeat(40)),
                        updated_at: at(0),
                        ci: CiState::Pass,
                    },
                    comments: vec![comment("1", "mira", None)],
                    reviewers: Vec::new(),
                    run: CiRun {
                        id: RunId::new("7"),
                        name: "ci".to_owned(),
                        event: None,
                        state: CiState::Running,
                        number: None,
                        head_sha: Sha::new("a".repeat(40)),
                        branch: None,
                        started_at: None,
                        completed_at: None,
                        url: None,
                        jobs: vec![job("build", CiState::Running)],
                    },
                    fail: false,
                }),
                pulses: AtomicUsize::new(0),
                snapshots: AtomicUsize::new(0),
                runs: AtomicUsize::new(0),
            })
        }

        fn edit(&self, change: impl FnOnce(&mut State)) {
            change(&mut self.state.lock().unwrap());
        }

        fn fail_if_asked(&self) -> Result<()> {
            if self.state.lock().unwrap().fail {
                return Err(Error::Network {
                    request: "GET /x".to_owned(),
                    reason: "down".to_owned(),
                });
            }
            Ok(())
        }
    }

    fn job(name: &str, state: CiState) -> CiJob {
        CiJob {
            id: JobId::new(name),
            name: name.to_owned(),
            stage: None,
            state,
            attempt: None,
            started_at: None,
            completed_at: None,
            url: None,
            steps: Vec::new(),
        }
    }

    #[async_trait]
    impl Forge for Fake {
        fn kind(&self) -> ForgeKind {
            ForgeKind::GitHub
        }

        fn host(&self) -> &str {
            "forge.test"
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::GITHUB
        }

        async fn whoami(&self) -> Result<User> {
            Ok(User::new("me"))
        }

        async fn inbox(&self, _: &InboxSection) -> Result<Vec<PullRequest>> {
            Ok(Vec::new())
        }

        async fn pull_request(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
            let state = self.state.lock().unwrap().clone();
            let branch = |name: &str| BranchRef {
                repo: Some(repo.clone()),
                name: name.to_owned(),
                sha: state.pulse.head.clone(),
            };
            Ok(PullRequest {
                id: PullRequestId::new("1"),
                repo: repo.clone(),
                number,
                title: "t".to_owned(),
                body: String::new(),
                author: User::new("cora"),
                state: state.pulse.state,
                base: branch("main"),
                head: branch("feature"),
                merge_base: None,
                created_at: at(0),
                updated_at: state.pulse.updated_at,
                review_decision: ReviewDecision::None,
                reviewers: state.reviewers,
                ci: Some(state.pulse.ci),
                labels: Vec::new(),
                assignees: Vec::new(),
                milestone: None,
                diff_stat: None,
                comment_count: None,
                unresolved_threads: None,
                web_url: String::new(),
                head_writable: false,
            })
        }

        async fn threads(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<Thread>> {
            let comments = self.state.lock().unwrap().comments.clone();
            Ok(comments
                .into_iter()
                .map(|comment| Thread {
                    id: ThreadId::new(format!("t{}", comment.id)),
                    anchor: None,
                    resolution: Resolution::NotResolvable,
                    outdated: false,
                    comments: vec![comment],
                })
                .collect())
        }

        async fn checks(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<CheckRun>> {
            Ok(Vec::new())
        }

        async fn stack(&self, _: &RepoRef, _: PrNumber) -> Result<Stack> {
            Ok(Stack {
                source: StackSource::BaseBranch,
                links: Vec::new(),
            })
        }

        async fn snapshot(&self, repo: &RepoRef, number: PrNumber) -> Result<Snapshot> {
            self.fail_if_asked()?;
            self.snapshots.fetch_add(1, Ordering::SeqCst);
            Ok(Snapshot {
                pull_request: self.pull_request(repo, number).await?,
                threads: self.threads(repo, number).await?,
                checks: Vec::new(),
            })
        }

        async fn pull_request_pulse(&self, _: &RepoRef, _: PrNumber) -> Result<PullRequestPulse> {
            self.fail_if_asked()?;
            self.pulses.fetch_add(1, Ordering::SeqCst);
            Ok(self.state.lock().unwrap().pulse.clone())
        }

        async fn ci_run(&self, _: &RepoRef, _: &RunId, _: CiDetail) -> Result<CiRun> {
            self.fail_if_asked()?;
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(self.state.lock().unwrap().run.clone())
        }
    }

    fn watcher() -> (Watcher, Arc<ManualClock>) {
        let clock = Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000)));
        (Watcher::new(clock.clone()), clock)
    }

    fn pull_request_target() -> WatchTarget {
        WatchTarget::PullRequest {
            repo: repo(),
            number: PrNumber(4),
        }
    }

    fn events(changes: Vec<Change>) -> Vec<WatchEvent> {
        changes.into_iter().map(|change| change.event).collect()
    }

    const HOUR: Duration = Duration::from_secs(3600);

    #[tokio::test]
    async fn an_idle_pull_request_backs_off_to_five_minutes() {
        let (mut watcher, _) = watcher();
        let fake = Fake::new();
        let id = watcher.subscribe(fake.clone(), pull_request_target());
        let mut changes = Vec::new();
        watcher.run_until(HOUR, |change| changes.push(change)).await;
        assert!(changes.is_empty(), "{changes:?}");
        // The baseline at 0, then +30, +90, +210 and +450 s while it warms
        // down, then every 300 s up to and including 3450 s.
        assert_eq!(fake.pulses.load(Ordering::SeqCst), 15);
        assert_eq!(fake.snapshots.load(Ordering::SeqCst), 1);
        assert_eq!(watcher.interval(id), Some(SLOW));
    }

    #[tokio::test]
    async fn active_ci_polls_fast_then_backs_off_after_it_ends() {
        let (mut watcher, clock) = watcher();
        let fake = Fake::new();
        fake.edit(|state| state.pulse.ci = CiState::Running);
        let id = watcher.subscribe(fake.clone(), pull_request_target());
        assert!(watcher.poll_due().await.is_empty());
        assert_eq!(watcher.interval(id), Some(FAST));

        clock.advance(FAST);
        assert!(watcher.poll_due().await.is_empty());
        fake.edit(|state| state.pulse.ci = CiState::Fail);
        clock.advance(FAST);
        assert_eq!(
            events(watcher.poll_due().await),
            [WatchEvent::CiStateChanged {
                from: CiState::Running,
                to: CiState::Fail
            }]
        );
        assert_eq!(
            fake.snapshots.load(Ordering::SeqCst),
            1,
            "updated_at did not move"
        );
        let mut intervals = Vec::new();
        for _ in 0..6 {
            intervals.push(watcher.interval(id).unwrap().as_secs());
            clock.advance(watcher.next_due().unwrap() - clock.now());
            assert!(watcher.poll_due().await.is_empty());
        }
        assert_eq!(intervals, [30, 60, 120, 240, 300, 300]);
    }

    #[tokio::test]
    async fn comments_reviews_pushes_and_merges_become_events() {
        let (mut watcher, clock) = watcher();
        let fake = Fake::new();
        watcher.subscribe(fake.clone(), pull_request_target());
        watcher.poll_due().await;

        fake.edit(|state| {
            state.pulse.updated_at = at(5);
            state.pulse.head = Sha::new("b".repeat(40));
            state.comments.push(comment("2", "mira", None));
            state
                .comments
                .push(comment("3", "lee", Some(ReviewState::ChangesRequested)));
            state.reviewers = vec![
                Reviewer::new(User::new("lee"), ReviewState::ChangesRequested),
                Reviewer::new(User::new("ana"), ReviewState::Approved),
            ];
        });
        clock.advance(SLOW);
        let seen = events(watcher.poll_due().await);
        assert_eq!(seen.len(), 5, "{seen:?}");
        assert_eq!(
            seen[0],
            WatchEvent::HeadChanged {
                from: Sha::new("a".repeat(40)),
                to: Sha::new("b".repeat(40))
            }
        );
        assert!(
            matches!(&seen[1], WatchEvent::NewComment { comment, .. } if comment.id.as_str() == "2")
        );
        assert_eq!(
            seen[2],
            WatchEvent::ReviewSubmitted {
                author: User::new("lee"),
                state: ReviewState::ChangesRequested
            }
        );
        assert!(
            matches!(&seen[3], WatchEvent::NewComment { comment, .. } if comment.id.as_str() == "3")
        );
        assert_eq!(
            seen[4],
            WatchEvent::ReviewSubmitted {
                author: User::new("ana"),
                state: ReviewState::Approved
            },
            "a bodiless approval comes from the reviewer list"
        );

        fake.edit(|state| {
            state.pulse.state = PrState::Merged;
            state.pulse.updated_at = at(6);
        });
        clock.advance(SLOW);
        assert_eq!(events(watcher.poll_due().await), [WatchEvent::Merged]);
    }

    #[tokio::test]
    async fn a_run_reports_its_jobs_and_slows_down_once_finished() {
        let (mut watcher, clock) = watcher();
        let fake = Fake::new();
        let id = watcher.subscribe(
            fake.clone(),
            WatchTarget::Run {
                repo: repo(),
                run: RunId::new("7"),
            },
        );
        watcher.poll_due().await;
        assert_eq!(watcher.interval(id), Some(FAST));
        fake.edit(|state| {
            state.run.state = CiState::Pass;
            state.run.jobs = vec![job("build", CiState::Pass), job("test", CiState::Pass)];
        });
        clock.advance(FAST);
        assert_eq!(
            events(watcher.poll_due().await),
            [
                WatchEvent::CiStateChanged {
                    from: CiState::Running,
                    to: CiState::Pass
                },
                WatchEvent::JobStateChanged {
                    job: JobId::new("build"),
                    name: "build".to_owned(),
                    from: CiState::Running,
                    to: CiState::Pass
                },
                WatchEvent::JobStateChanged {
                    job: JobId::new("test"),
                    name: "test".to_owned(),
                    from: CiState::None,
                    to: CiState::Pass
                },
            ]
        );
        assert_eq!(watcher.interval(id), Some(WARM));
        let polls = fake.runs.load(Ordering::SeqCst);
        watcher.run_until(clock.now() + HOUR, |_| {}).await;
        // Polls at +30, +90, +210 and +450 s, then every 300 s to +3450 s.
        assert_eq!(fake.runs.load(Ordering::SeqCst) - polls, 14);
    }

    #[tokio::test]
    async fn failures_back_off_and_report_once() {
        let (mut watcher, clock) = watcher();
        let fake = Fake::new();
        let id = watcher.subscribe(fake.clone(), pull_request_target());
        watcher.poll_due().await;
        fake.edit(|state| state.fail = true);
        let mut failures = 0;
        // Four failures double the warm start up to the slow ceiling.
        for _ in 0..4 {
            clock.advance(watcher.next_due().unwrap() - clock.now());
            failures += events(watcher.poll_due().await)
                .iter()
                .filter(|event| matches!(event, WatchEvent::PollFailed(_)))
                .count();
        }
        assert_eq!(failures, 1);
        assert_eq!(watcher.interval(id), Some(SLOW));
        fake.edit(|state| state.fail = false);
        clock.advance(SLOW);
        assert!(watcher.poll_due().await.is_empty());
        assert!(watcher.unsubscribe(id));
        assert!(watcher.next_due().is_none());
    }
}
