//! The view model the shell draws: inbox rows and sections, and the parts of
//! an open pull request. Forge results and fixtures are both mapped onto it.

use std::collections::HashSet;
use std::sync::Arc;

use chrono::Utc;
use gpui::SharedString;

pub use forge::model::{CiState, ForgeKind, PrNumber, RepoRef, ReviewStatus, Timestamp};
pub use store::PrKey;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Pending,
    Approved,
    ChangesRequested,
    Commented,
    Draft,
}

impl ReviewState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "needs review",
            Self::Approved => "approved",
            Self::ChangesRequested => "changes requested",
            Self::Commented => "commented",
            Self::Draft => "draft",
        }
    }

    fn of(pr: &forge::model::PullRequest) -> Self {
        use forge::model::{PrState, ReviewDecision};
        if pr.state == PrState::Draft {
            return Self::Draft;
        }
        match pr.review_decision {
            // A stale approval needs a fresh review; the row's own label comes
            // from the viewer's reviewer status.
            ReviewDecision::None
            | ReviewDecision::ReviewRequired
            | ReviewDecision::StaleApproval => Self::Pending,
            ReviewDecision::Commented => Self::Commented,
            ReviewDecision::Approved => Self::Approved,
            ReviewDecision::ChangesRequested => Self::ChangesRequested,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: SharedString,
    /// Id of the new-side blob (the old one for a deletion); viewed state is
    /// keyed by it so a changed file is unviewed again.
    pub blob: SharedString,
    pub status: FileStatus,
    pub added: u32,
    pub removed: u32,
}

/// How much of a set of files is viewed, for a directory's checkbox.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Coverage {
    Empty,
    Partial,
    Full,
}

/// The files the user has marked viewed, keyed by path and blob id.
#[derive(Clone, Debug, Default)]
pub struct Viewed(HashSet<(SharedString, SharedString)>);

impl Viewed {
    fn key(file: &ChangedFile) -> (SharedString, SharedString) {
        (file.path.clone(), file.blob.clone())
    }

    /// The marks of `set` that apply to `files` at their current blobs.
    pub fn from_store(set: &store::ViewedSet, files: &[ChangedFile]) -> Self {
        Self(
            files
                .iter()
                .filter(|file| set.is_viewed(&file.path, &file.blob))
                .map(Self::key)
                .collect(),
        )
    }

    pub fn contains(&self, file: &ChangedFile) -> bool {
        self.0.contains(&Self::key(file))
    }

    /// How many (path, blob) marks it holds; changes whenever a file is
    /// marked or unmarked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn set(&mut self, file: &ChangedFile, viewed: bool) {
        if viewed {
            self.0.insert(Self::key(file));
        } else {
            self.0.remove(&Self::key(file));
        }
    }

    pub fn toggle(&mut self, file: &ChangedFile) {
        let viewed = self.contains(file);
        self.set(file, !viewed);
    }

    pub fn count<'a>(&self, files: impl IntoIterator<Item = &'a ChangedFile>) -> usize {
        files.into_iter().filter(|f| self.contains(f)).count()
    }

    pub fn coverage(&self, files: &[ChangedFile]) -> Coverage {
        match self.count(files) {
            0 => Coverage::Empty,
            n if n == files.len() => Coverage::Full,
            _ => Coverage::Partial,
        }
    }

    /// Marks every file viewed, or unviewed when all already were. Returns
    /// the new state.
    pub fn toggle_all(&mut self, files: &[ChangedFile]) -> bool {
        let viewed = self.coverage(files) != Coverage::Full;
        for file in files {
            self.set(file, viewed);
        }
        viewed
    }
}

#[derive(Clone, Debug)]
pub struct Check {
    pub name: SharedString,
    pub state: CiState,
    pub detail: SharedString,
}

impl Check {
    pub fn from_forge(run: &forge::model::CheckRun) -> Self {
        let detail = match (run.started_at, run.completed_at) {
            (Some(start), Some(end)) if !run.state.is_active() => {
                duration_label((end - start).num_seconds())
            }
            (Some(start), None) if run.state == CiState::Running => {
                format!(
                    "running, {}",
                    duration_label((Utc::now() - start).num_seconds())
                )
            }
            _ => crate::ui::ci_label(run.state).to_owned(),
        };
        let name = match &run.group {
            Some(group) if !group.is_empty() => format!("{group} / {}", run.name),
            _ => run.name.clone(),
        };
        Self {
            name: name.into(),
            state: run.state,
            detail: detail.into(),
        }
    }
}

pub(crate) fn duration_label(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m {:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h {:02}m", seconds / 3600, seconds % 3600 / 60),
    }
}

/// One inbox row.
#[derive(Clone, Debug)]
pub struct PullRequest {
    pub key: PrKey,
    pub forge: ForgeKind,
    pub title: SharedString,
    pub author: SharedString,
    pub author_avatar: Option<SharedString>,
    pub reviewers: Vec<SharedString>,
    /// Each reviewer's avatar URL, in the order of `reviewers`.
    pub reviewer_avatars: Vec<Option<SharedString>>,
    pub head: SharedString,
    pub base: SharedString,
    /// The head commit, short.
    pub sha: SharedString,
    pub ci: CiState,
    pub review: ReviewState,
    /// Open, draft, merged or closed.
    pub state: forge::model::PrState,
    pub updated_at: Timestamp,
    pub description: SharedString,
    /// Lines added and removed, when the forge reported them.
    pub stat: Option<(u32, u32)>,
    /// Comments as the forge counts them (Forgejo's undercounts); `None`
    /// when it did not say.
    pub comments: Option<u32>,
    /// Review threads not resolved yet, where the forge's list says.
    pub unresolved: Option<u32>,
    /// The forge's own record, for fetching; `None` for fixtures.
    pub source: Option<Arc<forge::model::PullRequest>>,
}

impl PullRequest {
    pub fn from_forge(pr: Arc<forge::model::PullRequest>, forge: ForgeKind) -> Self {
        Self {
            key: PrKey::of(&pr),
            forge,
            title: pr.title.clone().into(),
            author: pr.author.login.clone().into(),
            author_avatar: pr.author.avatar_url.clone().map(Into::into),
            reviewers: pr
                .reviewers
                .iter()
                .map(|r| r.user.login.clone().into())
                .collect(),
            reviewer_avatars: pr
                .reviewers
                .iter()
                .map(|r| r.user.avatar_url.clone().map(Into::into))
                .collect(),
            head: pr.head.name.clone().into(),
            base: pr.base.name.clone().into(),
            sha: pr.head.sha.short().to_owned().into(),
            ci: pr.ci.unwrap_or_default(),
            review: ReviewState::of(&pr),
            state: pr.state,
            updated_at: pr.updated_at,
            description: pr.body.clone().into(),
            stat: pr.diff_stat.map(|s| (s.additions, s.deletions)),
            comments: pr.comment_count,
            unresolved: pr.unresolved_threads,
            source: Some(pr),
        }
    }

    pub fn number(&self) -> u64 {
        self.key.number.0
    }

    /// `owner/name#12`, or `!12` on GitLab.
    pub fn ref_label(&self) -> String {
        format!(
            "{}{}{}",
            self.key.repo.path(),
            self.forge.sigil(),
            self.key.number
        )
    }

    pub fn minutes_ago(&self) -> u32 {
        let minutes = (Utc::now() - self.updated_at).num_minutes();
        u32::try_from(minutes.max(0)).unwrap_or(u32::MAX)
    }
}

/// What an inbox row says about reviews.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowReview {
    /// The viewer is a reviewer: their own status, so a re-request after
    /// they approved reads as such whatever the others did.
    Mine(ReviewStatus),
    /// The viewer is not a reviewer (or not known yet): the overall decision.
    Overall(ReviewState),
}

impl RowReview {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Mine(status) => status.label(),
            Self::Overall(state) => state.label(),
        }
    }
}

impl PullRequest {
    /// The row's review label for `viewer`, the account signed in on its
    /// forge. A fixture row has only the reviewers' logins, and the fixture
    /// viewer is only ever asked.
    #[must_use]
    pub fn row_review(&self, viewer: Option<&forge::model::User>) -> RowReview {
        let mine = viewer.and_then(|viewer| match &self.source {
            Some(pr) => pr.viewer_status(viewer),
            None => (self.review == ReviewState::Pending
                && self
                    .reviewers
                    .iter()
                    .any(|login| login.eq_ignore_ascii_case(&viewer.login)))
            .then_some(ReviewStatus::ReviewRequested),
        });
        mine.map_or(RowReview::Overall(self.review), RowReview::Mine)
    }

    /// Reviews submitted so far (not requests or drafts), for the seen mark.
    #[must_use]
    pub fn review_count(&self) -> u32 {
        use forge::model::ReviewState as Forge;
        match &self.source {
            Some(pr) => {
                let submitted = pr.reviewers.iter().filter(|r| {
                    matches!(
                        r.state,
                        Forge::Commented
                            | Forge::Approved
                            | Forge::ChangesRequested
                            | Forge::Dismissed
                    )
                });
                u32::try_from(submitted.count()).unwrap_or(u32::MAX)
            }
            None => u32::from(matches!(
                self.review,
                ReviewState::Approved | ReviewState::ChangesRequested | ReviewState::Commented
            )),
        }
    }

    /// The forge's link to it: its own web URL, else one built from the key.
    #[must_use]
    pub fn web_link(&self) -> String {
        self.source
            .as_ref()
            .map(|source| source.web_url.clone())
            .filter(|url| crate::untrusted::is_web_url(url))
            .unwrap_or_else(|| {
                crate::references::pr_url(self.forge, &self.key.repo, self.key.number)
            })
    }
}

pub fn relative_time(minutes: u32) -> String {
    match minutes {
        0 => "now".into(),
        1..=59 => format!("{minutes}m"),
        60..=1439 => format!("{}h", minutes / 60),
        1440..=10079 => format!("{}d", minutes / 1440),
        _ => format!("{}w", minutes / 10080),
    }
}

/// Where a section's list stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SectionState {
    /// Nothing cached yet and the forge has not answered.
    Loading,
    /// From the cache; the forge has not answered yet.
    Cached,
    Fresh,
    /// The forge failed; the list is what the cache had.
    Failed(SharedString),
}

#[derive(Clone, Debug)]
pub struct Section {
    pub title: SharedString,
    /// Indices into [`Inbox::prs`], most recently updated first.
    pub items: Vec<usize>,
    pub expanded_by_default: bool,
    pub state: SectionState,
    /// The repository a `[[repos]]` section lists; `None` for the built-in
    /// ones.
    pub repo: Option<RepoRef>,
}

#[derive(Clone, Debug, Default)]
pub struct Inbox {
    /// Each pull request once, however many sections list it.
    pub prs: Vec<PullRequest>,
    pub sections: Vec<Section>,
}

impl Inbox {
    pub fn index_of(&self, key: &PrKey) -> Option<usize> {
        self.prs.iter().position(|pr| &pr.key == key)
    }

    pub fn get(&self, key: &PrKey) -> Option<&PullRequest> {
        self.prs.iter().find(|pr| &pr.key == key)
    }
}

/// One pull request of a stack.
#[derive(Clone, Debug)]
pub struct StackMember {
    pub key: PrKey,
    pub title: SharedString,
    pub author: SharedString,
    pub author_avatar: Option<SharedString>,
    pub ci: CiState,
    pub head: SharedString,
    /// Its base: the branch of the member below it, or the trunk.
    pub base: SharedString,
}

/// A stack, bottom (on the trunk) first, and the branch the bottom merges into.
#[derive(Clone, Debug, Default)]
pub struct Stack {
    pub members: Vec<StackMember>,
    pub base: SharedString,
}

impl Stack {
    pub fn from_forge(stack: &forge::model::Stack, repo: &RepoRef, inbox: &Inbox) -> Self {
        let members = stack
            .links
            .iter()
            .map(|link| {
                let key = PrKey::new(repo.clone(), link.number);
                let pr = inbox.get(&key);
                StackMember {
                    ci: pr.map_or(CiState::None, |pr| pr.ci),
                    author: pr.map(|pr| pr.author.clone()).unwrap_or_default(),
                    author_avatar: pr.and_then(|pr| pr.author_avatar.clone()),
                    key,
                    title: link.title.clone().into(),
                    head: link.head.clone().into(),
                    base: link.base.clone().into(),
                }
            })
            .collect();
        let base = stack
            .links
            .first()
            .map(|link| link.base.clone())
            .unwrap_or_default();
        Self {
            members,
            base: base.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, blob: &str) -> ChangedFile {
        ChangedFile {
            path: path.to_owned().into(),
            blob: blob.to_owned().into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
        }
    }

    #[test]
    fn coverage_rolls_up_none_some_all() {
        let files = [file("a", "1"), file("b", "2"), file("c", "3")];
        let mut viewed = Viewed::default();
        assert_eq!(viewed.coverage(&files), Coverage::Empty);
        viewed.set(&files[0], true);
        assert_eq!(viewed.coverage(&files), Coverage::Partial);
        viewed.set(&files[1], true);
        viewed.set(&files[2], true);
        assert_eq!(viewed.coverage(&files), Coverage::Full);
        assert_eq!(viewed.count(&files), 3);
    }

    #[test]
    fn bulk_toggle_marks_all_then_clears_all() {
        let files = [file("a", "1"), file("b", "2")];
        let mut viewed = Viewed::default();
        viewed.set(&files[0], true);
        assert!(viewed.toggle_all(&files));
        assert_eq!(viewed.coverage(&files), Coverage::Full);
        assert!(!viewed.toggle_all(&files));
        assert_eq!(viewed.coverage(&files), Coverage::Empty);
    }

    #[test]
    fn a_changed_blob_resets_viewed() {
        let mut viewed = Viewed::default();
        viewed.set(&file("a", "1"), true);
        assert!(viewed.contains(&file("a", "1")));
        assert!(!viewed.contains(&file("a", "2")));
    }

    fn reviewer(login: &str, state: forge::model::ReviewState) -> forge::model::Reviewer {
        forge::model::Reviewer::new(forge::model::User::new(login), state)
    }

    #[test]
    fn a_re_requested_reviewer_reads_re_review_requested_whatever_the_decision() {
        use forge::model::{ReviewDecision, ReviewState as Forge, User};

        let fixtures = crate::fixtures::Fixtures::load();
        let key = fixtures.inbox.prs[0].key.clone();
        let mut record = fixtures.forge_pr(&key).expect("a record");
        // GitLab: another reviewer approved the head, and the viewer, who
        // approved earlier, was asked again.
        let mut me = reviewer("cora", Forge::Requested);
        me.requested_again = true;
        record.reviewers = vec![reviewer("mira", Forge::Approved), me];
        record.review_decision = ReviewDecision::from_reviewers(&record.reviewers);
        assert_eq!(record.review_decision, ReviewDecision::Approved);
        let row = PullRequest::from_forge(Arc::new(record.clone()), ForgeKind::GitLab);
        assert_eq!(row.review, ReviewState::Approved, "the overall decision");

        let viewer = User::new("cora");
        let label = row.row_review(Some(&viewer));
        assert_eq!(label, RowReview::Mine(ReviewStatus::ReReviewRequested));
        assert_eq!(label.label(), "re-review requested");
        assert_eq!(
            row.row_review(Some(&User::new("someone"))),
            RowReview::Overall(ReviewState::Approved),
            "not a reviewer: the decision"
        );
        assert_eq!(
            row.row_review(None),
            RowReview::Overall(ReviewState::Approved)
        );

        // An approval before the last push, and a current one.
        record.reviewers = vec![reviewer("cora", Forge::Approved)];
        record.reviewers[0].stale = true;
        let row = PullRequest::from_forge(Arc::new(record.clone()), ForgeKind::GitHub);
        assert_eq!(
            row.row_review(Some(&viewer)).label(),
            "approved before last push"
        );
        record.reviewers[0].stale = false;
        let row = PullRequest::from_forge(Arc::new(record), ForgeKind::GitHub);
        assert_eq!(row.row_review(Some(&viewer)).label(), "approved");
        assert_eq!(row.review_count(), 1);
    }

    #[test]
    fn a_fixture_row_asks_the_viewer_to_review() {
        let fixtures = crate::fixtures::Fixtures::load();
        let viewer = forge::model::User::new("cora");
        let row = fixtures
            .inbox
            .prs
            .iter()
            .find(|pr| {
                pr.review == ReviewState::Pending && pr.reviewers.iter().any(|r| r == "cora")
            })
            .expect("a fixture asking the viewer");
        assert_eq!(
            row.row_review(Some(&viewer)),
            RowReview::Mine(ReviewStatus::ReviewRequested)
        );
    }

    #[test]
    fn durations_read_like_ci_pages() {
        assert_eq!(duration_label(42), "42s");
        assert_eq!(duration_label(190), "3m 10s");
        assert_eq!(duration_label(3720), "1h 02m");
    }
}
