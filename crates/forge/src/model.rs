//! The forge-neutral model. Every backend maps its API onto these types, and
//! the store crate caches them as JSON, so everything here is serde.

use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Times are always UTC; the UI formats them relative to now.
pub type Timestamp = DateTime<Utc>;

macro_rules! opaque_id {
    ($($(#[$meta:meta])* $name:ident;)+) => {$(
        $(#[$meta])*
        ///
        /// Opaque: only the backend that produced it knows its shape. Compare
        /// and store it, never parse it.
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    )+};
}

opaque_id! {
    /// A pull request's own id: a GitHub node id (`PR_kw...`), a GitLab
    /// global id (`gid://gitlab/MergeRequest/1`), a Forgejo numeric id.
    PullRequestId;
    /// A thread: a GitHub `PullRequestReviewThread` node id, a GitLab
    /// discussion id, or for Forgejo (which has no thread object) the id of
    /// the thread's first comment.
    ThreadId;
    /// A comment: a GitHub node id, a GitLab note id, a Forgejo comment id.
    CommentId;
    /// A submitted or pending review.
    ReviewId;
    /// An account.
    UserId;
    /// One check row: a GitHub check run or status context, a GitLab job, a
    /// Forgejo commit status.
    CheckRunId;
    /// What a job log or retry is addressed by: the GitHub Actions job id
    /// (`CheckRun.databaseId`), the GitLab job id, the Forgejo `ActionRunJob.id`.
    JobId;
    /// A workflow run or pipeline, for "re-run failed" and web links.
    RunId;
    /// A suggestion the forge itself can apply: a GitLab `Note.suggestions`
    /// id ([L4]). Other forges have none; the app applies theirs locally.
    SuggestionId;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForgeKind {
    GitHub,
    GitLab,
    Forgejo,
}

impl ForgeKind {
    pub const ALL: [Self; 3] = [Self::GitHub, Self::GitLab, Self::Forgejo];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::GitHub => "GitHub",
            Self::GitLab => "GitLab",
            Self::Forgejo => "Forgejo",
        }
    }

    /// Lowercase name, used for fixture directories and config.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
            Self::Forgejo => "forgejo",
        }
    }

    /// GitLab calls them merge requests and writes `!12`.
    #[must_use]
    pub fn sigil(self) -> char {
        match self {
            Self::GitLab => '!',
            Self::GitHub | Self::Forgejo => '#',
        }
    }
}

impl fmt::Display for ForgeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// A repository on a forge. `owner` is a user or organization, or on GitLab a
/// group path that may be nested (`group/subgroup`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RepoRef {
    /// The forge's web host, e.g. `github.com`; it also names the backend.
    pub host: String,
    pub owner: String,
    pub name: String,
}

impl RepoRef {
    #[must_use]
    pub fn new(host: impl Into<String>, owner: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            owner: owner.into(),
            name: name.into(),
        }
    }

    /// `owner/name`, GitLab's `full_path`.
    #[must_use]
    pub fn path(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// The same repository: forges treat owner and name case-insensitively,
    /// and answer with their own casing.
    #[must_use]
    pub fn same_as(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }
}

impl fmt::Display for RepoRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.host, self.owner, self.name)
    }
}

/// The number people use: GitHub and Forgejo `number`, GitLab `iid` (not the
/// global `id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PrNumber(pub u64);

impl fmt::Display for PrNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A full commit id, lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Sha(String);

impl Sha {
    #[must_use]
    pub fn new(sha: impl Into<String>) -> Self {
        Self(sha.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The first seven characters, for display.
    #[must_use]
    pub fn short(&self) -> &str {
        self.0.get(..7).unwrap_or(&self.0)
    }
}

impl fmt::Display for Sha {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountKind {
    #[default]
    User,
    Bot,
    /// A team or group named as a reviewer (GitHub team review requests).
    Team,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct User {
    /// `None` when the forge only gave a login (e.g. a deleted account).
    pub id: Option<UserId>,
    /// Username, or `org/team` for a team.
    pub login: String,
    /// Display name when the forge reports one.
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    #[serde(default)]
    pub kind: AccountKind,
}

impl User {
    #[must_use]
    pub fn new(login: impl Into<String>) -> Self {
        Self {
            id: None,
            login: login.into(),
            name: None,
            avatar_url: None,
            kind: AccountKind::User,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrState {
    /// Open and marked draft (GitHub `isDraft`, GitLab `draft`, Forgejo a
    /// `WIP:`/`Draft:` title prefix).
    Draft,
    Open,
    Merged,
    /// Closed without merging.
    Closed,
}

impl PrState {
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Self::Draft | Self::Open)
    }
}

/// One side of a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BranchRef {
    /// The repository the branch lives in; differs from the pull request's
    /// repo for a fork. `None` when the fork was deleted.
    pub repo: Option<RepoRef>,
    pub name: String,
    /// The branch tip the forge last saw.
    pub sha: Sha,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DiffStat {
    pub additions: u32,
    pub deletions: u32,
    pub changed_files: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Label {
    pub name: String,
    /// The forge's own hex color, without `#`. Data, not a theme color.
    pub color: Option<String>,
    /// What the label is for, when the forge has one.
    #[serde(default)]
    pub description: Option<String>,
}

/// The milestone a pull request is filed under.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Milestone {
    pub title: String,
}

/// The state of one person's review, as the forge itself words it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewState {
    /// Asked to review, has not yet (GitHub `reviewRequests`, GitLab
    /// `UNREVIEWED`, Forgejo `REQUEST_REVIEW`). Also the state of someone
    /// asked again after an earlier review: see [`Reviewer::requested_again`].
    Requested,
    /// The viewer's own unsubmitted review (GitHub `PENDING`, GitLab
    /// `REVIEW_STARTED`, Forgejo `PENDING`).
    Pending,
    /// GitHub `COMMENTED`, GitLab `REVIEWED`, Forgejo `COMMENT`.
    Commented,
    Approved,
    ChangesRequested,
    /// Dismissed, or on GitLab approved then unapproved (`UNAPPROVED`).
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Reviewer {
    /// A person, a bot, or a team (`user.kind` is [`AccountKind::Team`]).
    pub user: User,
    pub state: ReviewState,
    /// Asked to review again after an earlier review or approval: the forge
    /// shows a re-request (GitHub a pending request for someone who already
    /// reviewed; GitLab `UNREVIEWED` after an approval or review; Forgejo a
    /// review request newer than their review). `state` is then
    /// [`ReviewState::Requested`]: the earlier verdict no longer counts.
    #[serde(default)]
    pub requested_again: bool,
    /// An approval given before the current head (a push came after it). Only
    /// set when `state` is [`ReviewState::Approved`] and the forge can tell;
    /// false when it cannot.
    #[serde(default)]
    pub stale: bool,
}

impl Reviewer {
    #[must_use]
    pub fn new(user: User, state: ReviewState) -> Self {
        Self {
            user,
            state,
            requested_again: false,
            stale: false,
        }
    }

    /// A team was asked rather than a person.
    #[must_use]
    pub fn is_team(&self) -> bool {
        self.user.kind == AccountKind::Team
    }

    /// Approved the current head and has not been asked again.
    #[must_use]
    pub fn is_current_approval(&self) -> bool {
        self.state == ReviewState::Approved && !self.stale && !self.requested_again
    }

    /// What to call this person's review: separates a first request from a
    /// re-request, and an approval of the head from one before the last push.
    #[must_use]
    pub fn status(&self) -> ReviewStatus {
        match self.state {
            ReviewState::Requested if self.requested_again => ReviewStatus::ReReviewRequested,
            ReviewState::Requested => ReviewStatus::ReviewRequested,
            ReviewState::Pending => ReviewStatus::Pending,
            ReviewState::Commented => ReviewStatus::Commented,
            ReviewState::Approved if self.stale => ReviewStatus::StaleApproval,
            ReviewState::Approved => ReviewStatus::Approved,
            ReviewState::ChangesRequested => ReviewStatus::ChangesRequested,
            ReviewState::Dismissed => ReviewStatus::Dismissed,
        }
    }
}

/// One person's review as the app words it; see [`Reviewer::status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    /// "review requested"
    ReviewRequested,
    /// "re-review requested": asked again after reviewing or approving.
    ReReviewRequested,
    /// An unsubmitted review.
    Pending,
    Commented,
    /// "approved", of the current head.
    Approved,
    /// "approved before last push".
    StaleApproval,
    ChangesRequested,
    Dismissed,
}

impl ReviewStatus {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review requested",
            Self::ReReviewRequested => "re-review requested",
            Self::Pending => "draft review",
            Self::Commented => "commented",
            Self::Approved => "approved",
            Self::StaleApproval => "approved before last push",
            Self::ChangesRequested => "changes requested",
            Self::Dismissed => "dismissed",
        }
    }
}

/// The pull request's overall review state, for the inbox row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    /// No reviews and no review requested.
    #[default]
    None,
    /// Reviews are requested or required and none decided yet
    /// (GitHub `REVIEW_REQUIRED`). The app shows "needs review". Also the
    /// answer when every approver has been asked again.
    ReviewRequired,
    /// Only comments so far.
    Commented,
    /// At least one approval of the current head that no re-request has
    /// withdrawn, and no change request.
    Approved,
    /// The only approvals were given before the last push.
    StaleApproval,
    ChangesRequested,
}

impl ReviewDecision {
    /// The decision the reviewers alone give. A forge with its own rule
    /// engine (GitHub `reviewDecision`) refines this.
    ///
    /// An approval counts only while it is of the current head and its
    /// author was not asked again; `Requested` and `Dismissed` rows leave the
    /// decision at "needs review".
    #[must_use]
    pub fn from_reviewers(reviewers: &[Reviewer]) -> Self {
        let any = |test: fn(&Reviewer) -> bool| reviewers.iter().any(test);
        if any(|r| r.state == ReviewState::ChangesRequested) {
            Self::ChangesRequested
        } else if any(Reviewer::is_current_approval) {
            Self::Approved
        } else if any(|r| r.state == ReviewState::Approved && r.stale && !r.requested_again) {
            Self::StaleApproval
        } else if any(|r| r.state == ReviewState::Commented) {
            Self::Commented
        } else if any(|r| matches!(r.state, ReviewState::Requested | ReviewState::Dismissed)) {
            Self::ReviewRequired
        } else {
            Self::None
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "no review",
            Self::ReviewRequired => "needs review",
            Self::Commented => "commented",
            Self::Approved => "approved",
            Self::StaleApproval => "approved before last push",
            Self::ChangesRequested => "changes requested",
        }
    }
}

/// CI state of one check or of a rollup. The six states the app draws, plus
/// `None` for "no CI configured".
///
/// Mapping, so every backend agrees:
/// - GitHub check run: `QUEUED`, `PENDING`, `WAITING`, `REQUESTED` -> Queued;
///   `IN_PROGRESS` -> Running; conclusion `SUCCESS`, `NEUTRAL` -> Pass;
///   `FAILURE`, `TIMED_OUT`, `ACTION_REQUIRED`, `STARTUP_FAILURE` -> Fail;
///   `CANCELLED` -> Canceled; `SKIPPED`, `STALE` -> Skipped. Status context:
///   `SUCCESS` -> Pass, `FAILURE`, `ERROR` -> Fail, `PENDING`, `EXPECTED` -> Queued.
/// - GitLab job or pipeline: `created`, `waiting_for_resource`, `preparing`,
///   `pending`, `scheduled`, `waiting_for_callback` -> Queued; `running` ->
///   Running; `success` -> Pass; `failed` -> Fail (allowed-to-fail stays
///   Fail on the job, the pipeline rollup is the forge's); `canceled`,
///   `canceling` -> Canceled; `skipped`, `manual` -> Skipped.
/// - Forgejo commit status: `success` -> Pass, `failure`, `error` -> Fail,
///   `pending` -> Queued (Running when the Actions run reports `running`),
///   `warning` -> Pass. Actions run status: `waiting`, `blocked` -> Queued,
///   `running` -> Running, `cancelled` -> Canceled, `skipped` -> Skipped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiState {
    Pass,
    Fail,
    Running,
    /// Waiting for a runner or for a job it depends on.
    Queued,
    Skipped,
    Canceled,
    /// No CI is configured.
    #[default]
    None,
}

impl CiState {
    /// Rolls several states into one for an inbox row: a failure wins, then
    /// anything still going, then a cancel, then pass. All skipped stays
    /// skipped; nothing at all is `None`.
    #[must_use]
    pub fn rollup(states: impl IntoIterator<Item = Self>) -> Self {
        states
            .into_iter()
            .max_by_key(|state| state.rollup_rank())
            .unwrap_or(Self::None)
    }

    fn rollup_rank(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Skipped => 1,
            Self::Pass => 2,
            Self::Canceled => 3,
            Self::Queued => 4,
            Self::Running => 5,
            Self::Fail => 6,
        }
    }

    /// Still changing, so a subscription should poll fast.
    #[must_use]
    pub fn is_active(self) -> bool {
        matches!(self, Self::Running | Self::Queued)
    }
}

/// A pull request (GitHub, Forgejo) or merge request (GitLab).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub id: PullRequestId,
    /// The repository it merges into.
    pub repo: RepoRef,
    pub number: PrNumber,
    pub title: String,
    /// Description, markdown.
    pub body: String,
    pub author: User,
    pub state: PrState,
    pub base: BranchRef,
    pub head: BranchRef,
    /// Merge base when the forge reports one (GitLab `diff_refs.base_sha`,
    /// Forgejo `merge_base`); otherwise the repo crate computes it.
    pub merge_base: Option<Sha>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
    pub review_decision: ReviewDecision,
    /// Requested and actual reviewers, one row per person, latest state.
    pub reviewers: Vec<Reviewer>,
    /// Rollup of the head commit's checks. `None` when the source did not
    /// report it (a list endpoint without CI); `checks()` fills it in.
    pub ci: Option<CiState>,
    pub labels: Vec<Label>,
    /// Who the pull request is assigned to, in the forge's order.
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub milestone: Option<Milestone>,
    /// Lines and files changed. Filled on inbox rows too; `None` only when
    /// the source did not report it.
    pub diff_stat: Option<DiffStat>,
    /// Conversation comments plus review comments, as the forge counts them
    /// (GitHub `comments` plus the comments of each review thread, GitLab
    /// `userNotesCount`, Forgejo `comments` plus `review_comments`). `None`
    /// when the source did not report it.
    #[serde(default)]
    pub comment_count: Option<u32>,
    /// Review threads not yet resolved (GitHub `reviewThreads.isResolved`,
    /// GitLab resolvable minus resolved discussions). `None` when the
    /// forge's list payload does not carry it (Forgejo inbox rows: only each
    /// review's comments say whether they are resolved), or the pull request
    /// has more threads than one page counts.
    #[serde(default)]
    pub unresolved_threads: Option<u32>,
    pub web_url: String,
    /// The viewer may push to the head branch, so a suggestion can be
    /// applied as a commit there (GitHub `viewerCanApplySuggestion`, GitLab
    /// `userPermissions.pushToSourceBranch`, Forgejo the head repository's
    /// `permissions.push`). False when the forge did not say.
    #[serde(default)]
    pub head_writable: bool,
}

impl PullRequest {
    /// The head branch lives in the repository the pull request merges into,
    /// not in a fork (or a deleted one).
    #[must_use]
    pub fn head_in_base_repo(&self) -> bool {
        self.head
            .repo
            .as_ref()
            .is_some_and(|head| head.same_as(&self.repo))
    }

    /// `account`'s row among the reviewers, by id when both sides have one,
    /// else by login.
    #[must_use]
    pub fn reviewer(&self, account: &User) -> Option<&Reviewer> {
        self.reviewers
            .iter()
            .find(|row| match (&row.user.id, &account.id) {
                (Some(a), Some(b)) => a == b,
                _ => row.user.login == account.login,
            })
    }

    /// What the viewer's own review is called; `None` when they are not a
    /// reviewer.
    #[must_use]
    pub fn viewer_status(&self, viewer: &User) -> Option<ReviewStatus> {
        self.reviewer(viewer).map(Reviewer::status)
    }
}

/// Which side of a diff a line number counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    /// The base side (GitHub `LEFT`, GitLab `old_line`, Forgejo `old_position`).
    Old,
    /// The head side (GitHub `RIGHT`, GitLab `new_line`, Forgejo `new_position`).
    New,
}

/// A line on one side of a diff, 1-based file line number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LineRef {
    pub side: Side,
    pub line: u32,
}

/// Where a comment sits relative to one commit pair.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    /// The commented line; the last line of a multi-line range.
    pub line: LineRef,
    /// First line of a multi-line range. It may be on the other side
    /// (GitHub `startDiffSide`, GitLab `line_range.start`).
    pub start: Option<LineRef>,
    /// The head commit the position is relative to, when known.
    pub commit: Option<Sha>,
}

/// Where a thread is attached in the diff.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Anchor {
    /// Path on the side of `position.line` (the new path unless the comment
    /// is on the old side of a rename).
    pub path: String,
    /// Path before a rename, when it differs.
    pub old_path: Option<String>,
    /// Position on the current diff. `None` for a file-level comment, or for
    /// an outdated thread the forge could not map forward.
    pub position: Option<Position>,
    /// Where the thread was first written (GitHub `originalLine`,
    /// `originalCommit`; GitLab `original_position`). Set when it differs
    /// from `position`, which is always the case for an outdated thread.
    pub original: Option<Position>,
}

impl Anchor {
    /// The current line, or the original one for an outdated thread.
    #[must_use]
    pub fn best_position(&self) -> Option<&Position> {
        self.position.as_ref().or(self.original.as_ref())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Resolution {
    /// The thread cannot be resolved (a plain conversation comment, or a
    /// GitLab discussion with `resolvable: false`).
    NotResolvable,
    Unresolved,
    Resolved {
        /// Who resolved it, when the forge says (GitLab `resolvedBy`,
        /// Forgejo `resolver`; GitHub `resolvedBy`).
        by: Option<User>,
    },
}

/// A discussion: an inline review thread, or a conversation comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Thread {
    pub id: ThreadId,
    /// `None` for conversation-level comments: GitHub issue comments and
    /// review summaries, GitLab non-diff discussions, Forgejo issue comments.
    pub anchor: Option<Anchor>,
    pub resolution: Resolution,
    /// The code under the anchor changed since the thread was written.
    pub outdated: bool,
    /// Oldest first; never empty.
    pub comments: Vec<Comment>,
}

impl Thread {
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        matches!(self.resolution, Resolution::Resolved { .. })
    }

    #[must_use]
    pub fn is_resolvable(&self) -> bool {
        !matches!(self.resolution, Resolution::NotResolvable)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: CommentId,
    pub author: User,
    /// Markdown as written, suggestion fences included.
    pub body: String,
    pub created_at: Timestamp,
    /// Last edit, when the forge reports one that differs from creation.
    pub edited_at: Option<Timestamp>,
    pub web_url: Option<String>,
    /// The review it was submitted in, when it was part of one.
    pub review: Option<ReviewId>,
    /// Set on a review's summary comment: the verdict submitted with it.
    pub verdict: Option<ReviewState>,
    /// The suggestions the forge itself can apply ([L4]); empty elsewhere,
    /// where the app finds suggestion fences in `body`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
}

/// A suggestion block the forge tracks (GitLab `Note.suggestions`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Suggestion {
    pub id: SuggestionId,
    pub applied: bool,
    /// The forge would apply it now (GitLab `appliable`: false once applied,
    /// outdated, or on a closed merge request).
    pub appliable: bool,
    /// The lines GitLab would replace, new side, 1-based and inclusive.
    #[serde(default)]
    pub from_line: Option<u32>,
    #[serde(default)]
    pub to_line: Option<u32>,
    /// The text GitLab would remove and the text it would put there.
    #[serde(default)]
    pub from_content: Option<String>,
    #[serde(default)]
    pub to_content: Option<String>,
    /// Lines the block reaches above and below the commented line
    /// (`suggestion:-N+M`); zero for a block on the commented lines.
    #[serde(default)]
    pub lines_above: u32,
    #[serde(default)]
    pub lines_below: u32,
}

// ---- writes ----------------------------------------------------------------

/// Where a new inline comment goes, as the composer sends it (the store's
/// `DraftAnchor::Line` and `DraftAnchor::File`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommentAnchor {
    /// One line, or a range from `start` to `line` (both on the current
    /// diff; `start` may be on the other side, like [`Position::start`]).
    Line {
        path: String,
        line: LineRef,
        start: Option<LineRef>,
    },
    /// The whole file (GitHub `subjectType: FILE`).
    File { path: String },
}

impl CommentAnchor {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Line { path, .. } | Self::File { path } => path,
        }
    }
}

/// Where a queued comment goes: a new thread, or a reply in an existing one.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingPlace {
    New(CommentAnchor),
    Reply(ThreadId),
}

/// A comment to add to a pending review.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NewComment {
    pub place: PendingPlace,
    /// Markdown, suggestion fences included.
    pub body: String,
}

/// What a review is submitted as. Each forge words them its own way in the
/// UI (GitLab "Approve", "Request changes", "Comment").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    pub const ALL: [Self; 3] = [Self::Comment, Self::Approve, Self::RequestChanges];

    /// The review state a submitted review of this verdict reads back as.
    #[must_use]
    pub fn state(self) -> ReviewState {
        match self {
            Self::Comment => ReviewState::Commented,
            Self::Approve => ReviewState::Approved,
            Self::RequestChanges => ReviewState::ChangesRequested,
        }
    }
}

/// A review, pending or submitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    /// GitLab has no review object (draft notes belong to the user and the
    /// merge request), so its backend makes this id up from the merge request.
    pub id: ReviewId,
    pub author: User,
    /// [`ReviewState::Pending`] until submitted.
    pub state: ReviewState,
    /// The summary, markdown; empty when there is none.
    pub body: String,
    /// The head commit the review was started on, when the forge says.
    pub commit: Option<Sha>,
    /// `None` while pending.
    pub submitted_at: Option<Timestamp>,
    pub web_url: Option<String>,
}

/// One comment of the viewer's pending review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingComment {
    pub id: CommentId,
    pub place: PendingPlace,
    /// The thread a new comment opened, when the forge makes threads before
    /// the review is submitted (GitHub does; GitLab and Forgejo do not).
    pub thread: Option<ThreadId>,
    pub body: String,
    /// `None` where the forge keeps no time for a queued comment (GitLab
    /// draft notes).
    pub created_at: Option<Timestamp>,
}

/// The viewer's unsubmitted review of one pull request, with its comments
/// oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingReview {
    pub review: Review,
    pub comments: Vec<PendingComment>,
}

// ---- pull request lifecycle --------------------------------------------------

/// How a merge puts the pull request's commits on the base branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    /// A merge commit (GitHub `MERGE`, GitLab `merge`/`rebase_merge`,
    /// Forgejo `merge`).
    Merge,
    /// One commit with the combined change (GitHub `SQUASH`, GitLab
    /// `squash`, Forgejo `squash`).
    Squash,
    /// The commits replayed on the base, with no merge commit (GitHub
    /// `REBASE`, GitLab `ff` projects, Forgejo `rebase`).
    Rebase,
}

impl MergeMethod {
    pub const ALL: [Self; 3] = [Self::Merge, Self::Squash, Self::Rebase];
}

/// Why a pull request cannot be merged now. A forge reports only the ones it
/// can name; [`Self::BranchProtection`] stands for any other rule it
/// enforces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeBlocker {
    /// The head conflicts with the base.
    Conflicts,
    /// Required checks failed.
    ChecksFailing,
    /// Required checks have not finished.
    ChecksPending,
    /// Fewer approvals than the base branch requires.
    ApprovalsRequired,
    /// A reviewer's "changes requested" blocks the merge.
    ChangesRequested,
    /// Marked draft.
    Draft,
    /// Threads must be resolved first (GitLab).
    UnresolvedThreads,
    /// The head is behind the base and the rules ask for it to be current
    /// (GitHub `BEHIND`, GitLab `NEED_REBASE`).
    OutOfDate,
    /// Closed or already merged.
    NotOpen,
    /// A rule of the base branch or project not named above.
    BranchProtection,
    /// The forge is still computing it; read again in a moment.
    Computing,
}

/// Whether a pull request can be merged now, and what stands in the way.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mergeability {
    /// The forge's verdict: `Some(true)` merges now, `Some(false)` is
    /// refused (see `reasons`), `None` while it is still computing.
    pub mergeable: Option<bool>,
    /// Empty when `mergeable` is `Some(true)`.
    pub reasons: Vec<MergeBlocker>,
    /// Auto-merge is on: the forge merges once its requirements are met.
    pub auto_merge: bool,
}

/// What a repository allows, for the merge dialog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeSettings {
    /// In the order of [`MergeMethod::ALL`]; never empty.
    pub allowed: Vec<MergeMethod>,
    /// The method the forge preselects; one of `allowed`.
    pub default: MergeMethod,
}

impl MergeSettings {
    /// `allowed` in canonical order, with `default` moved to the first
    /// allowed method when the forge's own default is not allowed. Falls back
    /// to a lone [`MergeMethod::Merge`] when nothing is allowed.
    #[must_use]
    pub fn new(allowed: impl IntoIterator<Item = MergeMethod>, default: MergeMethod) -> Self {
        let allowed: Vec<MergeMethod> = allowed.into_iter().collect();
        let mut allowed: Vec<MergeMethod> = MergeMethod::ALL
            .into_iter()
            .filter(|method| allowed.contains(method))
            .collect();
        if allowed.is_empty() {
            allowed.push(MergeMethod::Merge);
        }
        let default = if allowed.contains(&default) {
            default
        } else {
            allowed[0]
        };
        Self { allowed, default }
    }
}

/// What a merge, or turning on auto-merge, is asked to do.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MergeOptions {
    pub method: MergeMethod,
    /// The merge or squash commit's subject; the forge's own when `None`.
    pub commit_title: Option<String>,
    /// The commit's body; the forge's own when `None`.
    pub commit_message: Option<String>,
    /// Delete the head branch afterwards. Honored only when it lives in the
    /// pull request's own repository, never a fork's.
    pub delete_branch: bool,
    /// The head commit the caller looked at. The forge refuses the merge when
    /// the head has moved since.
    pub sha: Sha,
}

impl MergeOptions {
    #[must_use]
    pub fn new(method: MergeMethod, sha: Sha) -> Self {
        Self {
            method,
            commit_title: None,
            commit_message: None,
            delete_branch: false,
            sha,
        }
    }

    /// Title and body as the one message GitLab takes.
    #[must_use]
    pub fn commit_text(&self) -> Option<String> {
        match (self.commit_title.as_deref(), self.commit_message.as_deref()) {
            (Some(title), Some(message)) if !message.is_empty() => Some(format!(
                "{title}

{message}"
            )),
            (Some(title), _) => Some(title.to_owned()),
            (None, message) => message.map(str::to_owned),
        }
    }
}

/// One row of the CI list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub id: CheckRunId,
    pub name: String,
    /// Workflow name (GitHub), stage (GitLab) or workflow (Forgejo status
    /// context prefix), for grouping.
    pub group: Option<String>,
    pub state: CiState,
    /// Web page of the check or job.
    pub url: Option<String>,
    /// For logs and retry; `None` for checks that are not jobs (a GitHub
    /// status context or a non-Actions check app).
    pub job: Option<JobId>,
    pub run: Option<RunId>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    /// Empty when the forge has no step list (see
    /// [`Capabilities::job_steps`](crate::Capabilities::job_steps)) or the job is queued.
    pub steps: Vec<CheckStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckStep {
    pub number: u32,
    pub name: String,
    pub state: CiState,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
}

/// One workflow run (GitHub, Forgejo) or pipeline (GitLab) of a commit, with
/// its jobs: the detail behind the [`CheckRun`] rollup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiRun {
    pub id: RunId,
    /// Workflow name (GitHub `name`, Forgejo `workflow_id` such as `ci.yml`),
    /// or the pipeline's `name`, `pipeline` when it has none (GitLab).
    pub name: String,
    /// What started it, as the forge words it: `push`, `pull_request`,
    /// `merge_request_event`, `schedule`, ...
    pub event: Option<String>,
    pub state: CiState,
    /// The number people see (GitHub `run_number`, GitLab `iid`, Forgejo
    /// `index_in_repo`).
    pub number: Option<u64>,
    pub head_sha: Sha,
    /// The branch or ref it ran on, when the forge says.
    pub branch: Option<String>,
    pub started_at: Option<Timestamp>,
    /// Set once the run is finished.
    pub completed_at: Option<Timestamp>,
    pub url: Option<String>,
    /// In the forge's order; only the latest attempt of each job.
    pub jobs: Vec<CiJob>,
}

impl CiRun {
    /// The job named `name`, the latest attempt.
    #[must_use]
    pub fn job(&self, name: &str) -> Option<&CiJob> {
        self.jobs.iter().find(|job| job.name == name)
    }
}

/// One job of a [`CiRun`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiJob {
    /// What [`crate::Forge::job_log`] and [`crate::Forge::retry_job`] take.
    pub id: JobId,
    pub name: String,
    /// GitLab's stage; `None` elsewhere.
    pub stage: Option<String>,
    pub state: CiState,
    /// GitHub `run_attempt`, Forgejo `attempt` (a Forgejo re-run keeps the
    /// job id and counts up here). GitLab gives a retried job a new id.
    pub attempt: Option<u32>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub url: Option<String>,
    /// Empty where the forge has no steps (GitLab), before a job starts, and
    /// in a [`CiDetail::Jobs`] read on Forgejo.
    pub steps: Vec<CheckStep>,
}

impl CiJob {
    /// The job will not change any more (until a re-run).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !self.state.is_active()
    }
}

/// How much of a run to read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiDetail {
    /// Runs and jobs. Costs one request per run on every forge, so polling
    /// uses this.
    Jobs,
    /// Also each job's steps; on Forgejo one more request per job.
    Steps,
}

/// A piece of a job's raw log (bytes as the forge stores them: ANSI colors,
/// runner timestamps and a GitHub byte-order mark included).
///
/// Tail a job by asking again from `next_offset` until `complete`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChunk {
    /// Where `bytes` start in the log: the offset asked for, or 0 when the
    /// log turned out shorter than that (a Forgejo re-run reuses the job id
    /// and starts a new log), so the caller starts over.
    pub offset: u64,
    pub bytes: Vec<u8>,
    /// What to ask for next: `offset + bytes.len()`.
    pub next_offset: u64,
    /// The job is finished and this chunk reaches the end of its log.
    pub complete: bool,
}

/// What a pull request subscription compares between polls: cheap to read
/// (one to three conditional requests) and different whenever something a
/// person would be told about changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestPulse {
    pub state: PrState,
    pub head: Sha,
    /// Moves on comments, reviews and pushes.
    pub updated_at: Timestamp,
    /// Rollup of the head's checks.
    pub ci: CiState,
}

/// GitHub's per-file viewed state ([G4]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewedState {
    Viewed,
    Unviewed,
    /// Was viewed, then the file changed.
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ViewedFile {
    pub path: String,
    pub state: ViewedState,
}

/// One pull request in a stack.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StackLink {
    pub number: PrNumber,
    pub title: String,
    pub state: PrState,
    pub head: String,
    pub base: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackSource {
    /// The forge's own stack (GitHub `PullRequest.stack`, GitLab `MergeRequest.stack`).
    Native,
    /// Derived: A's base branch is B's head branch.
    BaseBranch,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stack {
    pub source: StackSource,
    /// Bottom (nearest the trunk) first. Includes the pull request asked
    /// about; a pull request that is not stacked is a stack of one.
    pub links: Vec<StackLink>,
}

impl Stack {
    #[must_use]
    pub fn position_of(&self, number: PrNumber) -> Option<usize> {
        self.links.iter().position(|link| link.number == number)
    }
}

/// Which pull requests an inbox section lists.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxSection {
    /// Open, review requested from me (team requests included where the
    /// forge's query includes them).
    ReviewRequested,
    /// Open, authored by me.
    AuthoredByMe,
    /// Open pull requests in one repository, anyone's.
    OpenInRepo(RepoRef),
    /// A forge-specific query, passed through:
    /// - GitHub: search syntax, `is:pr` is added (`is:open label:bug`).
    /// - GitLab: query parameters of `GET /merge_requests`
    ///   (`scope=all&state=opened&labels=bug`).
    /// - Forgejo: query parameters of `GET /repos/issues/search`
    ///   (`state=open&labels=bug`), `type=pulls` is added.
    Query(String),
}

/// A pull request with its threads and checks, what opening one needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub pull_request: PullRequest,
    pub threads: Vec<Thread>,
    pub checks: Vec<CheckRun>,
}
