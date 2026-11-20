//! Subscriptions without gpui: what is watched (the user's choices, kept in
//! the store's subscriptions table, and by default the pull requests that
//! are theirs), the forge subscriptions that takes, and what each change
//! the watch engine reports means for the app: an inbox row to patch, CI
//! to read again, a badge, an activity entry and maybe a toast.
//!
//! [`crate::subscriptions`] runs this against the forge hub's watcher and
//! applies the [`Effect`]s; `tests/poll_cost.rs` runs it against a watcher
//! on a fake clock.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::Utc;
use forge::model::{CiState, PrNumber, ReviewState, RunId, ThreadId, Timestamp};
use forge::watch::{Change, SubscriptionId, WatchEvent, WatchTarget};
use serde::{Deserialize, Serialize};
use store::{PrKey, SubscriptionTarget};

use crate::settings::NotificationSettings;

/// One toast per pull request in this window; what comes later in it is
/// folded into one toast when the window ends.
pub const COALESCE: Duration = Duration::from_secs(10);
/// Activity entries kept.
pub const ACTIVITY_LIMIT: usize = 50;

/// What the store keeps per target in its opaque `state` column.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// False mutes a pull request that would be watched by default.
    pub watch: bool,
    /// For a run: the pull request it belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
}

impl Saved {
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("two plain fields")
    }

    /// A row without a readable state is a plain "watch".
    #[must_use]
    pub fn decode(state: Option<&str>) -> Self {
        state
            .and_then(|text| serde_json::from_str(text).ok())
            .unwrap_or(Self {
                watch: true,
                pr: None,
            })
    }
}

/// A store change a toggle asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Persist {
    Put(SubscriptionTarget, String),
    Delete(SubscriptionTarget),
}

/// Subscribes and unsubscribes forge watches: the hub's
/// [`forge::WatchHandle`] in the app, a watcher on a fake clock in tests.
pub trait Engine {
    fn subscribe(&mut self, target: WatchTarget) -> SubscriptionId;
    fn unsubscribe(&mut self, id: SubscriptionId);
}

impl Engine for forge::WatchHandle {
    fn subscribe(&mut self, target: WatchTarget) -> SubscriptionId {
        forge::WatchHandle::subscribe(self, target)
    }

    fn unsubscribe(&mut self, id: SubscriptionId) {
        forge::WatchHandle::unsubscribe(self, id);
    }
}

#[must_use]
pub fn watch_target(target: &SubscriptionTarget) -> WatchTarget {
    match target {
        SubscriptionTarget::PullRequest { repo, number } => WatchTarget::PullRequest {
            repo: repo.clone(),
            number: PrNumber(*number),
        },
        SubscriptionTarget::Run { repo, run } => WatchTarget::Run {
            repo: repo.clone(),
            run: run.clone(),
        },
    }
}

#[must_use]
pub fn run_target(key: &PrKey, run: &RunId) -> SubscriptionTarget {
    SubscriptionTarget::Run {
        repo: key.repo.clone(),
        run: run.clone(),
    }
}

/// A pull request as watching needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrInfo {
    pub key: PrKey,
    /// `owner/name#12` (`!12` on GitLab).
    pub label: String,
    pub title: String,
    /// The viewer opened it.
    pub authored: bool,
    /// The viewer opened it or is a reviewer.
    pub mine: bool,
}

/// The open pull request: watched for the panels even when not for toasts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenInterest {
    pub info: PrInfo,
    /// Its runs still going, followed so the job rows move.
    pub active_runs: Vec<RunId>,
}

/// One subscription the app wants.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Interest {
    pub target: SubscriptionTarget,
    /// The pull request a change is about.
    pub pr: PrKey,
    /// Changes may raise a toast; false for what is followed only because
    /// it is open.
    pub notify: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    CiFailed,
    CiRecovered,
    CiFinished,
    Review,
    Comment,
    MergedClosed,
    Reopened,
}

impl NoticeKind {
    fn allowed(self, settings: &NotificationSettings) -> bool {
        match self {
            Self::CiFailed => settings.ci_failed,
            Self::CiRecovered => settings.ci_recovered,
            Self::CiFinished => settings.ci_finished,
            Self::Review => settings.reviews,
            Self::Comment => settings.comments,
            Self::MergedClosed => settings.merged_closed,
            Self::Reopened => false,
        }
    }

    /// A CI change worth telling, or `None`. `last` is the last finished
    /// state seen before `from`, so a failure, a re-run and a pass read as
    /// recovered. From no CI at all to a pass is a run that started and
    /// ended between two polls.
    #[must_use]
    pub fn of_ci(from: CiState, to: CiState, last: Option<CiState>) -> Option<Self> {
        let went = from.is_active() || from == CiState::None;
        match to {
            CiState::Fail if from != CiState::Fail => Some(Self::CiFailed),
            CiState::Pass if from == CiState::Fail => Some(Self::CiRecovered),
            CiState::Pass if went && last == Some(CiState::Fail) => Some(Self::CiRecovered),
            CiState::Pass if went => Some(Self::CiFinished),
            _ => None,
        }
    }
}

/// Where a toast or an activity entry opens the pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// The Conversation with its pipelines card open.
    Pipelines,
    Conversation(Option<ThreadId>),
    Overview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub key: PrKey,
    pub kind: NoticeKind,
    pub place: Place,
    pub title: String,
    pub body: String,
    /// Changes folded into it; more than one says so in the body.
    pub count: usize,
}

impl Toast {
    /// Replaces an earlier toast of the same pull request in the OS.
    #[must_use]
    pub fn tag(&self) -> String {
        format!(
            "review:{}/{}#{}",
            self.key.repo.host,
            self.key.repo.path(),
            self.key.number
        )
    }

    #[must_use]
    pub fn text(&self) -> String {
        match self.count {
            0 | 1 => self.body.clone(),
            n => format!("{} (and {} more)", self.body, n - 1),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityEntry {
    pub at: Timestamp,
    pub key: PrKey,
    pub label: String,
    pub text: String,
    pub kind: NoticeKind,
    pub place: Place,
}

/// What the app does about a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// The pull request's check rollup is now `ci`.
    RowCi {
        key: PrKey,
        ci: CiState,
    },
    /// Its CI runs changed: read them again if it is open.
    RefreshCi(PrKey),
    /// Comments, reviews or its state changed: read it again if it is open.
    RefreshPr(PrKey),
    /// New commits: the open diff is stale.
    HeadMoved(PrKey),
    Toast(Toast),
    /// The first failed poll of a streak.
    PollFailed {
        key: PrKey,
        error: String,
    },
}

#[derive(Default)]
struct Coalescer {
    shown: HashMap<PrKey, Instant>,
    held: HashMap<PrKey, Toast>,
}

impl Coalescer {
    fn offer(&mut self, toast: Toast, now: Instant) -> Option<Toast> {
        if let Some(&at) = self.shown.get(&toast.key)
            && now < at + COALESCE
        {
            let count = self.held.get(&toast.key).map_or(0, |held| held.count);
            self.held.insert(
                toast.key.clone(),
                Toast {
                    count: count + toast.count,
                    ..toast
                },
            );
            return None;
        }
        self.shown.insert(toast.key.clone(), now);
        Some(toast)
    }

    fn flush(&mut self, now: Instant) -> Vec<Toast> {
        let due: Vec<PrKey> = self
            .held
            .keys()
            .filter(|key| self.shown.get(*key).is_none_or(|&at| at + COALESCE <= now))
            .cloned()
            .collect();
        due.into_iter()
            .filter_map(|key| {
                let toast = self.held.remove(&key)?;
                self.shown.insert(key, now);
                Some(toast)
            })
            .collect()
    }

    fn next_flush(&self) -> Option<Instant> {
        self.held
            .keys()
            .filter_map(|key| self.shown.get(key))
            .map(|&at| at + COALESCE)
            .min()
    }
}

struct Live {
    id: SubscriptionId,
    pr: PrKey,
    notify: bool,
}

/// The subscription state of the app.
#[derive(Default)]
pub struct Watching {
    saved: HashMap<SubscriptionTarget, Saved>,
    settings: NotificationSettings,
    live: HashMap<SubscriptionTarget, Live>,
    by_id: HashMap<SubscriptionId, SubscriptionTarget>,
    /// The last finished CI state reported for each pull request, by its
    /// own subscription or one of its runs'.
    last_ci: HashMap<PrKey, CiState>,
    info: HashMap<PrKey, PrInfo>,
    /// The viewer's login per forge host.
    me: HashMap<String, String>,
    open: Option<PrKey>,
    coalescer: Coalescer,
    activity: VecDeque<ActivityEntry>,
    unread: usize,
    badges: HashSet<PrKey>,
}

impl Watching {
    #[must_use]
    pub fn new(settings: NotificationSettings) -> Self {
        Self {
            settings,
            ..Self::default()
        }
    }

    /// The store's rows, read at start.
    pub fn load(&mut self, rows: impl IntoIterator<Item = store::Subscription>) {
        for row in rows {
            self.saved
                .insert(row.target, Saved::decode(row.state.as_deref()));
        }
    }

    pub fn set_settings(&mut self, settings: NotificationSettings) {
        self.settings = settings;
    }

    #[must_use]
    pub fn settings(&self) -> &NotificationSettings {
        &self.settings
    }

    pub fn set_me(&mut self, host: &str, login: &str) {
        self.me.insert(host.to_owned(), login.to_owned());
    }

    #[must_use]
    pub fn me(&self, host: &str) -> Option<&str> {
        self.me.get(host).map(String::as_str)
    }

    /// Whether `target` is watched: the user's choice, else the default for
    /// pull requests that are theirs.
    #[must_use]
    pub fn is_watched(&self, target: &SubscriptionTarget, mine: bool) -> bool {
        self.saved
            .get(target)
            .map_or(mine && self.default_watch(target), |saved| saved.watch)
    }

    fn default_watch(&self, target: &SubscriptionTarget) -> bool {
        self.settings.watch_mine && matches!(target, SubscriptionTarget::PullRequest { .. })
    }

    /// Flips watching `target` and says what to write to the store. A run
    /// keeps the pull request it belongs to.
    pub fn toggle(&mut self, target: &SubscriptionTarget, mine: bool, pr: Option<u64>) -> Persist {
        let watch = !self.is_watched(target, mine);
        if watch == (mine && self.default_watch(target)) {
            self.saved.remove(target);
            return Persist::Delete(target.clone());
        }
        let saved = Saved { watch, pr };
        self.saved.insert(target.clone(), saved);
        Persist::Put(target.clone(), saved.encode())
    }

    /// The subscriptions wanted for these inbox rows and the open pull
    /// request. Remembers the rows for the text of later notices.
    pub fn plan(
        &mut self,
        rows: impl IntoIterator<Item = PrInfo>,
        open: Option<OpenInterest>,
    ) -> Vec<Interest> {
        let mut wanted: Vec<Interest> = Vec::new();
        let mut seen: HashSet<SubscriptionTarget> = HashSet::new();
        let mut push = |wanted: &mut Vec<Interest>, interest: Interest| {
            if seen.insert(interest.target.clone()) {
                wanted.push(interest);
            }
        };
        let mut saved: Vec<(&SubscriptionTarget, &Saved)> =
            self.saved.iter().filter(|(_, s)| s.watch).collect();
        saved.sort_by_key(|(target, _)| sort_key(target));
        for (target, saved) in saved {
            let pr = match target {
                SubscriptionTarget::PullRequest { repo, number } => {
                    PrKey::new(repo.clone(), PrNumber(*number))
                }
                SubscriptionTarget::Run { repo, .. } => {
                    let Some(number) = saved.pr else { continue };
                    PrKey::new(repo.clone(), PrNumber(number))
                }
            };
            push(
                &mut wanted,
                Interest {
                    target: target.clone(),
                    pr,
                    notify: true,
                },
            );
        }
        self.info.clear();
        for row in rows {
            let target = SubscriptionTarget::pull_request(&row.key);
            if self.is_watched(&target, row.mine) {
                push(
                    &mut wanted,
                    Interest {
                        target,
                        pr: row.key.clone(),
                        notify: true,
                    },
                );
            }
            self.info.insert(row.key.clone(), row);
        }
        self.open = open.as_ref().map(|open| open.info.key.clone());
        if let Some(open) = open {
            let key = open.info.key.clone();
            push(
                &mut wanted,
                Interest {
                    target: SubscriptionTarget::pull_request(&key),
                    pr: key.clone(),
                    notify: false,
                },
            );
            for run in &open.active_runs {
                push(
                    &mut wanted,
                    Interest {
                        target: run_target(&key, run),
                        pr: key.clone(),
                        notify: false,
                    },
                );
            }
            self.info.entry(key).or_insert(open.info);
        }
        wanted
    }

    /// Subscribes to what is wanted and not followed yet, and drops what is
    /// followed and no longer wanted.
    pub fn sync(&mut self, wanted: Vec<Interest>, engine: &mut dyn Engine) {
        let keep: HashSet<&SubscriptionTarget> = wanted.iter().map(|i| &i.target).collect();
        let gone: Vec<SubscriptionTarget> = self
            .live
            .keys()
            .filter(|target| !keep.contains(target))
            .cloned()
            .collect();
        for target in gone {
            if let Some(live) = self.live.remove(&target) {
                engine.unsubscribe(live.id);
                self.by_id.remove(&live.id);
            }
        }
        for interest in wanted {
            if let Some(live) = self.live.get_mut(&interest.target) {
                live.notify = interest.notify;
                live.pr = interest.pr;
                continue;
            }
            let id = engine.subscribe(watch_target(&interest.target));
            self.by_id.insert(id, interest.target.clone());
            self.live.insert(
                interest.target,
                Live {
                    id,
                    pr: interest.pr,
                    notify: interest.notify,
                },
            );
        }
    }

    /// Drops every subscription without telling the engine, for a new one.
    pub fn forget_subscriptions(&mut self) {
        self.live.clear();
        self.by_id.clear();
    }

    #[must_use]
    pub fn subscription_id(&self, target: &SubscriptionTarget) -> Option<SubscriptionId> {
        self.live.get(target).map(|live| live.id)
    }

    /// Forge subscriptions running now.
    #[must_use]
    pub fn subscription_count(&self) -> usize {
        self.live.len()
    }

    /// What `change` means for the app, toasts included (already
    /// coalesced; call [`Self::flush`] at [`Self::next_flush`] for the rest).
    pub fn handle(&mut self, change: Change, now: Instant) -> Vec<Effect> {
        let Some(target) = self.by_id.get(&change.subscription).cloned() else {
            return Vec::new();
        };
        let Some(live) = self.live.get(&target) else {
            return Vec::new();
        };
        let (key, notify) = (live.pr.clone(), live.notify);
        let mut effects = Vec::new();
        let mut told: Option<(NoticeKind, Place, String)> = None;
        match change.event {
            WatchEvent::CiStateChanged { from, to } => {
                let last = self.last_ci.get(&key).copied();
                let last = if is_finished(from) { Some(from) } else { last };
                if is_finished(to) {
                    self.last_ci.insert(key.clone(), to);
                } else if let Some(last) = last {
                    self.last_ci.insert(key.clone(), last);
                }
                if matches!(target, SubscriptionTarget::PullRequest { .. }) {
                    effects.push(Effect::RowCi {
                        key: key.clone(),
                        ci: to,
                    });
                }
                effects.push(Effect::RefreshCi(key.clone()));
                if let Some(kind) = NoticeKind::of_ci(from, to, last) {
                    let what = match &target {
                        SubscriptionTarget::Run { .. } => "run",
                        SubscriptionTarget::PullRequest { .. } => "checks",
                    };
                    let text = match kind {
                        NoticeKind::CiFailed => format!("{what} failed"),
                        NoticeKind::CiRecovered => format!("{what} pass again"),
                        _ => format!("{what} passed"),
                    };
                    told = Some((kind, Place::Pipelines, text));
                }
            }
            WatchEvent::JobStateChanged { .. } => effects.push(Effect::RefreshCi(key.clone())),
            WatchEvent::HeadChanged { .. } => {
                effects.push(Effect::HeadMoved(key.clone()));
                effects.push(Effect::RefreshCi(key.clone()));
            }
            WatchEvent::NewComment { thread, comment } => {
                effects.push(Effect::RefreshPr(key.clone()));
                if self.is_theirs(&key, &comment.author.login) {
                    let excerpt: String = comment.body.chars().take(140).collect();
                    told = Some((
                        NoticeKind::Comment,
                        Place::Conversation(Some(thread)),
                        format!("{}: {}", comment.author.login, excerpt.trim()),
                    ));
                }
            }
            WatchEvent::ReviewSubmitted { author, state } => {
                effects.push(Effect::RefreshPr(key.clone()));
                if self.is_theirs(&key, &author.login) {
                    told = Some((
                        NoticeKind::Review,
                        Place::Conversation(None),
                        format!("{} {}", author.login, verdict_text(state)),
                    ));
                }
            }
            WatchEvent::Merged => {
                effects.push(Effect::RefreshPr(key.clone()));
                told = Some((NoticeKind::MergedClosed, Place::Overview, "merged".into()));
            }
            WatchEvent::Closed => {
                effects.push(Effect::RefreshPr(key.clone()));
                told = Some((NoticeKind::MergedClosed, Place::Overview, "closed".into()));
            }
            WatchEvent::Reopened => {
                effects.push(Effect::RefreshPr(key.clone()));
                told = Some((NoticeKind::Reopened, Place::Overview, "reopened".into()));
            }
            WatchEvent::PollFailed(error) => {
                effects.push(Effect::PollFailed {
                    key,
                    error: error.to_string(),
                });
                return effects;
            }
        }
        if let Some((kind, place, text)) = told
            && let Some(toast) = self.tell(&key, kind, place, text, notify, now)
        {
            effects.push(Effect::Toast(toast));
        }
        effects
    }

    /// A comment or review on the viewer's own pull request by someone else.
    fn is_theirs(&self, key: &PrKey, author: &str) -> bool {
        let authored = self.info.get(key).is_some_and(|info| info.authored);
        authored && self.me(&key.repo.host) != Some(author)
    }

    fn tell(
        &mut self,
        key: &PrKey,
        kind: NoticeKind,
        place: Place,
        text: String,
        notify: bool,
        now: Instant,
    ) -> Option<Toast> {
        let info = self.info.get(key);
        let label = info.map_or_else(
            || format!("{}#{}", key.repo.path(), key.number),
            |info| info.label.clone(),
        );
        let title = info.map(|info| info.title.clone()).unwrap_or_default();
        self.activity.push_front(ActivityEntry {
            at: Utc::now(),
            key: key.clone(),
            label: label.clone(),
            text: text.clone(),
            kind,
            place: place.clone(),
        });
        self.activity.truncate(ACTIVITY_LIMIT);
        self.unread += 1;
        if self.open.as_ref() != Some(key) {
            self.badges.insert(key.clone());
        }
        if !(notify && self.settings.enabled && kind.allowed(&self.settings)) {
            return None;
        }
        let toast = Toast {
            key: key.clone(),
            kind,
            place,
            title: format!("{label}: {text}"),
            body: title,
            count: 1,
        };
        self.coalescer.offer(toast, now)
    }

    /// Toasts held back by [`COALESCE`] whose window has ended.
    pub fn flush(&mut self, now: Instant) -> Vec<Toast> {
        self.coalescer.flush(now)
    }

    #[must_use]
    pub fn next_flush(&self) -> Option<Instant> {
        self.coalescer.next_flush()
    }

    /// Newest first.
    pub fn activity(&self) -> impl Iterator<Item = &ActivityEntry> {
        self.activity.iter()
    }

    #[must_use]
    pub fn unread(&self) -> usize {
        self.unread
    }

    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    #[must_use]
    pub fn has_badge(&self, key: &PrKey) -> bool {
        self.badges.contains(key)
    }

    pub fn clear_badge(&mut self, key: &PrKey) -> bool {
        self.badges.remove(key)
    }
}

fn is_finished(state: CiState) -> bool {
    matches!(state, CiState::Pass | CiState::Fail | CiState::Canceled)
}

fn sort_key(target: &SubscriptionTarget) -> (u8, String) {
    match target {
        SubscriptionTarget::PullRequest { repo, number } => (0, format!("{repo:?}#{number}")),
        SubscriptionTarget::Run { repo, run } => (1, format!("{repo:?}/{run}")),
    }
}

fn verdict_text(state: ReviewState) -> &'static str {
    match state {
        ReviewState::Approved => "approved",
        ReviewState::ChangesRequested => "requested changes",
        ReviewState::Commented => "reviewed",
        ReviewState::Requested | ReviewState::Pending | ReviewState::Dismissed => {
            "updated a review"
        }
    }
}

#[cfg(test)]
mod tests {
    use forge::model::{Comment, CommentId, RepoRef, User};

    use super::*;

    fn key(n: u64) -> PrKey {
        PrKey::new(RepoRef::new("github.com", "o", "r"), PrNumber(n))
    }

    fn info(n: u64, authored: bool) -> PrInfo {
        PrInfo {
            key: key(n),
            label: format!("o/r#{n}"),
            title: format!("Change {n}"),
            authored,
            mine: authored,
        }
    }

    #[derive(Default)]
    struct Recorder {
        next: u64,
        subscribed: Vec<WatchTarget>,
        dropped: Vec<SubscriptionId>,
    }

    impl Engine for Recorder {
        fn subscribe(&mut self, target: WatchTarget) -> SubscriptionId {
            self.next += 1;
            self.subscribed.push(target);
            SubscriptionId(self.next)
        }

        fn unsubscribe(&mut self, id: SubscriptionId) {
            self.dropped.push(id);
        }
    }

    fn change(id: u64, event: WatchEvent) -> Change {
        Change {
            subscription: SubscriptionId(id),
            target: watch_target(&SubscriptionTarget::pull_request(&key(1))),
            event,
        }
    }

    fn toasts(effects: &[Effect]) -> Vec<&Toast> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Toast(toast) => Some(toast),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn own_pull_requests_are_watched_unless_muted() {
        let mut watching = Watching::new(NotificationSettings::default());
        let wanted = watching.plan([info(1, true), info(2, false)], None);
        assert_eq!(wanted.len(), 1);
        assert_eq!(wanted[0].pr, key(1));
        let target = SubscriptionTarget::pull_request(&key(1));
        assert_eq!(
            watching.toggle(&target, true, None),
            Persist::Put(target.clone(), r#"{"watch":false}"#.to_owned())
        );
        assert!(watching.plan([info(1, true)], None).is_empty());
        assert_eq!(
            watching.toggle(&target, true, None),
            Persist::Delete(target.clone())
        );
        let other = SubscriptionTarget::pull_request(&key(2));
        assert!(matches!(
            watching.toggle(&other, false, None),
            Persist::Put(..)
        ));
        assert_eq!(
            watching.plan([info(1, true), info(2, false)], None).len(),
            2
        );
    }

    #[test]
    fn saved_rows_load_and_runs_keep_their_pull_request() {
        let mut watching = Watching::new(NotificationSettings {
            watch_mine: false,
            ..NotificationSettings::default()
        });
        let run = run_target(&key(3), &RunId::new("9"));
        watching.load([store::Subscription {
            target: run.clone(),
            created_at: Utc::now(),
            state: Some(
                Saved {
                    watch: true,
                    pr: Some(3),
                }
                .encode(),
            ),
        }]);
        let wanted = watching.plan([info(1, true)], None);
        assert_eq!(wanted.len(), 1);
        assert_eq!(
            (wanted[0].target.clone(), wanted[0].pr.clone()),
            (run, key(3))
        );
    }

    #[test]
    fn sync_subscribes_once_and_drops_what_is_gone() {
        let mut watching = Watching::new(NotificationSettings::default());
        let mut engine = Recorder::default();
        let open = OpenInterest {
            info: info(5, false),
            active_runs: vec![RunId::new("77")],
        };
        let wanted = watching.plan([info(1, true)], Some(open));
        watching.sync(wanted.clone(), &mut engine);
        watching.sync(wanted, &mut engine);
        assert_eq!(engine.subscribed.len(), 3);
        let wanted = watching.plan([info(1, true)], None);
        watching.sync(wanted, &mut engine);
        assert_eq!(engine.dropped.len(), 2);
        assert_eq!(watching.subscription_count(), 1);
    }

    #[test]
    fn ci_changes_toast_and_coalesce_per_pull_request() {
        let mut watching = Watching::new(NotificationSettings::default());
        let mut engine = Recorder::default();
        let wanted = watching.plan([info(1, true)], None);
        watching.sync(wanted, &mut engine);
        let start = Instant::now();
        let failed = watching.handle(
            change(
                1,
                WatchEvent::CiStateChanged {
                    from: CiState::Running,
                    to: CiState::Fail,
                },
            ),
            start,
        );
        assert!(failed.contains(&Effect::RowCi {
            key: key(1),
            ci: CiState::Fail
        }));
        let shown = toasts(&failed);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].kind, NoticeKind::CiFailed);
        assert_eq!(shown[0].title, "o/r#1: checks failed");
        assert!(watching.has_badge(&key(1)));

        let rerun = |from, to| WatchEvent::CiStateChanged { from, to };
        let later = start + Duration::from_secs(3);
        assert!(
            toasts(&watching.handle(change(1, rerun(CiState::Fail, CiState::Running)), later))
                .is_empty()
        );
        let passed = watching.handle(change(1, rerun(CiState::Running, CiState::Pass)), later);
        assert!(toasts(&passed).is_empty(), "held: shown 3 s ago");
        assert_eq!(watching.next_flush(), Some(start + COALESCE));
        assert!(watching.flush(start + Duration::from_secs(9)).is_empty());
        let held = watching.flush(start + COALESCE);
        assert_eq!(held.len(), 1);
        assert_eq!(
            held[0].kind,
            NoticeKind::CiRecovered,
            "a failure then a pass"
        );
        assert_eq!(watching.activity().count(), 2);
    }

    #[test]
    fn comments_toast_only_on_your_pull_request_and_not_your_own() {
        let mut watching = Watching::new(NotificationSettings::default());
        watching.set_me("github.com", "cora");
        let mut engine = Recorder::default();
        let wanted = watching.plan([info(1, true)], None);
        watching.sync(wanted, &mut engine);
        let comment = |login: &str| WatchEvent::NewComment {
            thread: ThreadId::new("t"),
            comment: Box::new(Comment {
                id: CommentId::new(login),
                author: User::new(login),
                body: "Looks good".to_owned(),
                created_at: Utc::now(),
                edited_at: None,
                web_url: None,
                review: None,
                verdict: None,
                suggestions: Vec::new(),
            }),
        };
        let now = Instant::now();
        assert!(toasts(&watching.handle(change(1, comment("cora")), now)).is_empty());
        let theirs = watching.handle(change(1, comment("mira")), now);
        assert_eq!(
            toasts(&theirs)[0].place,
            Place::Conversation(Some(ThreadId::new("t")))
        );

        watching.set_settings(NotificationSettings {
            comments: false,
            ..NotificationSettings::default()
        });
        let later = now + COALESCE * 2;
        assert!(toasts(&watching.handle(change(1, comment("lee")), later)).is_empty());
        assert_eq!(watching.activity().count(), 2, "still listed");
    }

    #[test]
    fn ci_notices_follow_the_transition() {
        use CiState::{Canceled, Fail, Pass, Queued, Running};
        assert_eq!(
            NoticeKind::of_ci(Running, Fail, None),
            Some(NoticeKind::CiFailed)
        );
        assert_eq!(
            NoticeKind::of_ci(Fail, Pass, None),
            Some(NoticeKind::CiRecovered)
        );
        assert_eq!(
            NoticeKind::of_ci(Queued, Pass, Some(Fail)),
            Some(NoticeKind::CiRecovered)
        );
        assert_eq!(
            NoticeKind::of_ci(Running, Pass, Some(Pass)),
            Some(NoticeKind::CiFinished)
        );
        assert_eq!(NoticeKind::of_ci(Pass, Running, None), None);
        assert_eq!(
            NoticeKind::of_ci(CiState::None, Pass, Some(Fail)),
            Some(NoticeKind::CiRecovered)
        );
        assert_eq!(NoticeKind::of_ci(Running, Canceled, None), None);
    }
}
