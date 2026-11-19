//! Merging, closing and draft state of an open pull request: what the forge
//! says about mergeability and allowed methods, the action waiting for a
//! confirmation, and the one in flight. Results patch the pull request in
//! place (its state chip and its inbox row).

use std::sync::Arc;

use forge::Capabilities;
use forge::model::{
    CiRun, CiState, ForgeKind, MergeBlocker, MergeMethod, MergeOptions, MergeSettings,
    Mergeability, PrState, Sha,
};
use gpui::{Context, SharedString};

use crate::model::{PrKey, PullRequest};
use crate::session::{OpenPr, Session};

/// A forge read that may not have answered yet.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Load<T> {
    #[default]
    Idle,
    Loading,
    Ready(T),
    Failed(SharedString),
    /// The forge has no API for it.
    Unsupported,
}

impl<T> Load<T> {
    pub fn ready(&self) -> Option<&T> {
        match self {
            Self::Ready(value) => Some(value),
            _ => None,
        }
    }
}

/// Something done to the pull request as a whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// `head` is the commit the user was looking at when they asked; the
    /// forge is sent exactly it, so it refuses if the branch moved since.
    Merge {
        method: MergeMethod,
        delete_branch: bool,
        head: Sha,
    },
    /// Merge once the forge's requirements are met, at `head` as above.
    AutoMerge {
        method: MergeMethod,
        delete_branch: bool,
        head: Sha,
    },
    CancelAutoMerge,
    Close,
    Reopen,
    /// `true` converts to draft, `false` marks it ready.
    SetDraft(bool),
}

impl Action {
    /// Merges and closes always ask first.
    #[must_use]
    pub fn needs_confirm(&self) -> bool {
        matches!(
            self,
            Self::Merge { .. } | Self::AutoMerge { .. } | Self::Close
        )
    }

    /// The verb on its button.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Merge { .. } => "Merge",
            Self::AutoMerge { .. } => "Merge when checks pass",
            Self::CancelAutoMerge => "Cancel auto-merge",
            Self::Close => "Close",
            Self::Reopen => "Reopen",
            Self::SetDraft(true) => "Convert to draft",
            Self::SetDraft(false) => "Mark ready for review",
        }
    }

    /// The head a merge was asked at.
    #[must_use]
    pub fn head(&self) -> Option<&Sha> {
        match self {
            Self::Merge { head, .. } | Self::AutoMerge { head, .. } => Some(head),
            _ => None,
        }
    }

    /// What a confirmation asks, naming host, repository, number, the
    /// commit a merge lands and its method.
    #[must_use]
    pub fn question(&self, key: &PrKey, forge: ForgeKind) -> String {
        let target = format!(
            "{}/{}{}{} on {}",
            key.repo.owner,
            key.repo.name,
            forge.sigil(),
            key.number,
            key.repo.host
        );
        match self {
            Self::Merge {
                method,
                delete_branch,
                head,
            }
            | Self::AutoMerge {
                method,
                delete_branch,
                head,
            } => {
                let when = if matches!(self, Self::AutoMerge { .. }) {
                    " once its checks pass"
                } else {
                    ""
                };
                let branch = if *delete_branch {
                    ", then delete the branch"
                } else {
                    ""
                };
                format!(
                    "Merge {target} at {} {}{when}{branch}?",
                    head.short(),
                    method_phrase(*method)
                )
            }
            Self::Close => format!("Close {target} without merging?"),
            other => format!("{} {target}?", other.label()),
        }
    }
}

/// A merge method as the menu names it.
#[must_use]
pub fn method_label(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "Merge commit",
        MergeMethod::Squash => "Squash and merge",
        MergeMethod::Rebase => "Rebase and merge",
    }
}

/// How a merge method lands, for a confirmation.
fn method_phrase(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "with a merge commit",
        MergeMethod::Squash => "as one squashed commit",
        MergeMethod::Rebase => "by rebasing its commits",
    }
}

/// Why a merge is blocked, in words.
#[must_use]
pub fn blocker_label(blocker: MergeBlocker) -> &'static str {
    match blocker {
        MergeBlocker::Conflicts => "the branch has conflicts with the base",
        MergeBlocker::ChecksFailing => "required checks are failing",
        MergeBlocker::ChecksPending => "required checks have not finished",
        MergeBlocker::ApprovalsRequired => "more approvals are required",
        MergeBlocker::ChangesRequested => "a reviewer requested changes",
        MergeBlocker::Draft => "it is a draft",
        MergeBlocker::UnresolvedThreads => "threads must be resolved",
        MergeBlocker::OutOfDate => "the branch is behind its base",
        MergeBlocker::NotOpen => "it is not open",
        MergeBlocker::BranchProtection => "a branch rule blocks it",
        MergeBlocker::Computing => "the forge is still checking",
    }
}

/// How an action went, under the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub text: SharedString,
    pub error: bool,
}

/// The lifecycle state of one open pull request.
#[derive(Clone, Debug, Default)]
pub struct Lifecycle {
    pub mergeability: Load<Mergeability>,
    pub settings: Load<MergeSettings>,
    /// The method picked in the merge menu; the forge's default until then.
    pub method: Option<MergeMethod>,
    pub delete_branch: bool,
    /// Waiting for the user to confirm.
    pub confirm: Option<Action>,
    /// Sent, not answered yet.
    pub busy: Option<Action>,
    pub notice: Option<Notice>,
}

impl Lifecycle {
    /// What fixtures say: every method allowed, squash preselected, and
    /// blockers from the draft flag and the runs.
    #[must_use]
    pub fn fixture(pr: Option<&forge::model::PullRequest>, runs: &[CiRun]) -> Self {
        let mut lifecycle = Self {
            settings: Load::Ready(MergeSettings::new(MergeMethod::ALL, MergeMethod::Squash)),
            delete_branch: true,
            ..Self::default()
        };
        if let Some(pr) = pr {
            lifecycle.mergeability = Load::Ready(fixture_mergeability(pr.state, runs));
        }
        lifecycle
    }

    /// The method a merge uses now.
    #[must_use]
    pub fn chosen_method(&self) -> Option<MergeMethod> {
        let settings = self.settings.ready()?;
        Some(
            self.method
                .filter(|method| settings.allowed.contains(method))
                .unwrap_or(settings.default),
        )
    }

    /// Drops a merge waiting for its confirmation when the head it was asked
    /// at is no longer `head`, so the user never confirms a commit they did
    /// not see.
    pub fn head_moved(&mut self, head: &Sha) {
        if self
            .confirm
            .as_ref()
            .and_then(Action::head)
            .is_some_and(|asked| asked != head)
        {
            self.confirm = None;
            self.notice = Some(Notice {
                text: HEAD_MOVED.into(),
                error: true,
            });
        }
    }

    /// A pull request that left the open states cannot merge any more.
    pub fn track(&mut self, pr: &forge::model::PullRequest) {
        if !pr.state.is_open() {
            self.mergeability = Load::Ready(Mergeability {
                mergeable: Some(false),
                reasons: vec![MergeBlocker::NotOpen],
                auto_merge: false,
            });
        }
    }
}

fn fixture_mergeability(state: PrState, runs: &[CiRun]) -> Mergeability {
    let jobs = || runs.iter().flat_map(|run| &run.jobs);
    let mut reasons = Vec::new();
    match state {
        PrState::Merged | PrState::Closed => reasons.push(MergeBlocker::NotOpen),
        PrState::Draft => reasons.push(MergeBlocker::Draft),
        PrState::Open => {}
    }
    if jobs().any(|job| job.state == CiState::Fail) {
        reasons.push(MergeBlocker::ChecksFailing);
    } else if jobs().any(|job| matches!(job.state, CiState::Running | CiState::Queued)) {
        reasons.push(MergeBlocker::ChecksPending);
    }
    Mergeability {
        mergeable: Some(reasons.is_empty()),
        reasons,
        auto_merge: false,
    }
}

/// Whether `caps` offers `action` at all.
#[must_use]
pub fn supported(caps: &Capabilities, action: &Action) -> bool {
    match action {
        Action::Merge { .. } => caps.merge,
        Action::AutoMerge { .. } | Action::CancelAutoMerge => caps.auto_merge,
        Action::Close | Action::Reopen => caps.close_reopen,
        Action::SetDraft(_) => caps.set_draft,
    }
}

impl Session {
    /// Reads `key`'s mergeability and its repository's merge methods.
    /// Fixtures computed theirs when the pull request opened.
    pub fn refresh_lifecycle(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(services) = self.services().cloned() else {
            return;
        };
        let Some(caps) = self.capabilities_for(key) else {
            return;
        };
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        if !caps.merge {
            open.lifecycle.mergeability = Load::Unsupported;
            open.lifecycle.settings = Load::Unsupported;
            return;
        }
        open.lifecycle.mergeability = Load::Loading;
        if open.lifecycle.settings.ready().is_none() {
            open.lifecycle.settings = Load::Loading;
        }
        let (repo, number) = (key.repo.clone(), key.number);
        let read = services.forge(&key.repo.host, move |forge| async move {
            let mergeability = forge.mergeability(&repo, number).await;
            let settings = forge.merge_methods(&repo).await;
            Ok((mergeability, settings))
        });
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let Ok((mergeability, settings)) = read.await else {
                return;
            };
            this.update(cx, |session, cx| {
                let Some(open) = session.pr_mut(&key) else {
                    return;
                };
                open.lifecycle.mergeability = loaded(mergeability);
                open.lifecycle.settings = loaded(settings);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub fn set_merge_method(&mut self, key: &PrKey, method: MergeMethod, cx: &mut Context<Self>) {
        if let Some(open) = self.pr_mut(key) {
            open.lifecycle.method = Some(method);
            cx.notify();
        }
    }

    pub fn set_delete_branch(&mut self, key: &PrKey, delete: bool, cx: &mut Context<Self>) {
        if let Some(open) = self.pr_mut(key) {
            open.lifecycle.delete_branch = delete;
            cx.notify();
        }
    }

    /// Asks for `action` on `key`: merges and closes wait for
    /// [`Self::confirm_action`], the rest go at once.
    pub fn request_action(&mut self, key: &PrKey, action: Action, cx: &mut Context<Self>) {
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        if open.lifecycle.busy.is_some() {
            return;
        }
        open.lifecycle.notice = None;
        if refuse_moved_head(open, &action) {
            cx.notify();
            return;
        }
        if action.needs_confirm() {
            open.lifecycle.confirm = Some(action);
            cx.notify();
        } else {
            self.run_action(key, action, cx);
        }
    }

    pub fn confirm_action(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        let Some(action) = self
            .pr_mut(key)
            .and_then(|open| open.lifecycle.confirm.take())
        else {
            return;
        };
        self.run_action(key, action, cx);
    }

    pub fn cancel_action(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if let Some(open) = self.pr_mut(key) {
            open.lifecycle.confirm = None;
            cx.notify();
        }
    }

    fn run_action(&mut self, key: &PrKey, action: Action, cx: &mut Context<Self>) {
        let services = self.services().cloned();
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        let Some(source) = open.forge_pr().cloned() else {
            return;
        };
        if refuse_moved_head(open, &action) {
            cx.notify();
            return;
        }
        let Some(services) = services else {
            // Fixtures: the forge's answer is invented from the request.
            let mut pr = source;
            match &action {
                Action::Merge { .. } => pr.state = PrState::Merged,
                Action::Close => pr.state = PrState::Closed,
                Action::Reopen | Action::SetDraft(false) => pr.state = PrState::Open,
                Action::SetDraft(true) => pr.state = PrState::Draft,
                Action::AutoMerge { .. } | Action::CancelAutoMerge => {}
            }
            let auto = matches!(action, Action::AutoMerge { .. });
            self.action_done(key, &action, Ok(pr), cx);
            if let Some(open) = self.pr_mut(key)
                && let Load::Ready(mergeability) = &mut open.lifecycle.mergeability
            {
                mergeability.auto_merge = auto;
            }
            return;
        };
        open.lifecycle.busy = Some(action.clone());
        cx.notify();
        let (repo, number) = (key.repo.clone(), key.number);
        let sent = action.clone();
        let call = services.forge(&key.repo.host, move |forge| async move {
            let options = |method, delete_branch, head| MergeOptions {
                delete_branch,
                ..MergeOptions::new(method, head)
            };
            match sent {
                Action::Merge {
                    method,
                    delete_branch,
                    head,
                } => {
                    forge
                        .merge(&repo, number, &options(method, delete_branch, head))
                        .await
                }
                Action::AutoMerge {
                    method,
                    delete_branch,
                    head,
                } => {
                    let options = options(method, delete_branch, head);
                    forge.set_auto_merge(&repo, number, Some(&options)).await
                }
                Action::CancelAutoMerge => forge.set_auto_merge(&repo, number, None).await,
                Action::Close => forge.close(&repo, number).await,
                Action::Reopen => forge.reopen(&repo, number).await,
                Action::SetDraft(draft) => forge.set_draft(&repo, number, draft).await,
            }
        });
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let result = call.await.map_err(|error| error.to_string());
            this.update(cx, |session, cx| {
                session.action_done(&key, &action, result, cx);
                session.refresh_lifecycle(&key, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Patches the pull request with what the forge answered.
    fn action_done(
        &mut self,
        key: &PrKey,
        action: &Action,
        result: Result<forge::model::PullRequest, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        open.lifecycle.busy = None;
        let pr = match result {
            Ok(pr) => pr,
            Err(error) => {
                open.lifecycle.notice = Some(Notice {
                    text: format!("{} failed: {error}", action.label()).into(),
                    error: true,
                });
                cx.notify();
                return;
            }
        };
        let done = match pr.state {
            PrState::Merged => "Merged",
            PrState::Closed => "Closed",
            PrState::Draft => "Marked draft",
            PrState::Open => match action {
                Action::AutoMerge { .. } => "Auto-merge is on",
                Action::CancelAutoMerge => "Auto-merge is off",
                Action::Reopen => "Reopened",
                _ => "Ready for review",
            },
        };
        open.lifecycle.notice = Some(Notice {
            text: done.into(),
            error: false,
        });
        open.lifecycle.track(&pr);
        self.patch_forge_pr(key, pr, cx);
    }

    /// Takes the pull request as a write answered it: the open record, its
    /// row and the inbox row, without reading it again.
    pub(crate) fn patch_forge_pr(
        &mut self,
        key: &PrKey,
        pr: forge::model::PullRequest,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        let row = if open.pr.source.is_some() {
            let row = PullRequest::from_forge(Arc::new(pr.clone()), open.pr.forge);
            PullRequest {
                stat: row.stat.or(open.pr.stat),
                comments: row.comments.or(open.pr.comments),
                unresolved: row.unresolved.or(open.pr.unresolved),
                ..row
            }
        } else {
            // A fixture row has no forge record to rebuild from.
            PullRequest {
                state: pr.state,
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
                ..open.pr.clone()
            }
        };
        open.set_forge_pr(pr);
        open.pr = row.clone();
        self.patch_row(row);
        cx.notify();
    }
}

const HEAD_MOVED: &str = "The pull request changed; look again before merging";

/// Whether `action` is a merge asked at a head `open` no longer shows; if
/// so, says why under the header.
fn refuse_moved_head(open: &mut OpenPr, action: &Action) -> bool {
    let Some(asked) = action.head() else {
        return false;
    };
    if open.merge_head().as_ref() == Some(asked) {
        return false;
    }
    open.lifecycle.notice = Some(Notice {
        text: HEAD_MOVED.into(),
        error: true,
    });
    true
}

fn loaded<T>(result: forge::Result<T>) -> Load<T> {
    match result {
        Ok(value) => Load::Ready(value),
        Err(forge::Error::Unsupported { .. }) => Load::Unsupported,
        Err(error) => Load::Failed(error.to_string().into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PrNumber, RepoRef};

    #[test]
    fn merges_and_closes_ask_first_and_name_the_target() {
        let key = PrKey::new(RepoRef::new("gitlab.com", "group", "proj"), PrNumber(7));
        let merge = Action::Merge {
            method: MergeMethod::Squash,
            delete_branch: true,
            head: Sha::new("abc1234def5678"),
        };
        assert!(merge.needs_confirm() && Action::Close.needs_confirm());
        assert!(!Action::SetDraft(true).needs_confirm() && !Action::Reopen.needs_confirm());
        assert_eq!(
            merge.question(&key, ForgeKind::GitLab),
            "Merge group/proj!7 on gitlab.com at abc1234 as one squashed commit, then delete the branch?"
        );
        assert_eq!(
            Action::Close.question(&key, ForgeKind::GitLab),
            "Close group/proj!7 on gitlab.com without merging?"
        );
    }

    #[test]
    fn a_moved_head_drops_the_merge_confirmation() {
        let asked = Sha::new("aaaaaaa1");
        let mut lifecycle = Lifecycle {
            confirm: Some(Action::Merge {
                method: MergeMethod::Merge,
                delete_branch: false,
                head: asked.clone(),
            }),
            ..Lifecycle::default()
        };
        lifecycle.head_moved(&asked);
        assert!(lifecycle.confirm.is_some());
        lifecycle.head_moved(&Sha::new("bbbbbbb2"));
        assert!(lifecycle.confirm.is_none());
        assert!(lifecycle.notice.as_ref().is_some_and(|n| n.error));
    }

    #[test]
    fn fixture_blockers_follow_draft_and_runs() {
        let blocked = fixture_mergeability(PrState::Draft, &[]);
        assert_eq!(blocked.mergeable, Some(false));
        assert_eq!(blocked.reasons, [MergeBlocker::Draft]);
        assert_eq!(
            fixture_mergeability(PrState::Open, &[]).mergeable,
            Some(true)
        );
    }
}
