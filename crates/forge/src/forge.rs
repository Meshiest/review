use async_trait::async_trait;

use crate::capabilities::Capabilities;
#[cfg(doc)]
use crate::capabilities::{ReplyMode, ResolveMode, ViewedSync};
use crate::error::{Error, Result};
#[cfg(doc)]
use crate::model::PendingPlace;
use crate::model::{
    CheckRun, CiDetail, CiJob, CiRun, Comment, CommentId, ForgeKind, InboxSection, JobId, Label,
    LogChunk, MergeOptions, MergeSettings, Mergeability, NewComment, PendingComment, PendingReview,
    PrNumber, PullRequest, PullRequestPulse, RepoRef, Review, ReviewId, RunId, Sha, Snapshot,
    Stack, SuggestionId, Thread, ThreadId, User, Verdict, ViewedFile,
};
#[cfg(doc)]
use crate::people::COLLABORATOR_LIMIT;

/// Most pull requests one inbox call returns (most recently updated first).
pub const INBOX_LIMIT: usize = 100;

/// One forge account on one host: the reads and the review writes.
///
/// Every method runs on the forge runtime (see [`crate::ForgeHub`]), sends
/// through the backend's [`crate::http::HttpClient`] (never its own client),
/// and returns the shared model fully mapped: no forge-specific strings leak
/// to the UI. The `[G..]`, `[L..]`, `[F..]` tags are the footnotes of the
/// capability matrix in `docs/design.md`.
///
/// Rules for every backend:
/// - Paginate to the end (or to [`INBOX_LIMIT`] for the inbox); a partial
///   list is a bug, not a page.
/// - Name every GraphQL operation: recordings match on the operation name
///   and variables.
/// - A missing pull request is [`Error::NotFound`]; a feature the forge
///   lacks is [`Error::Unsupported`], never an empty success, unless the
///   doc below says empty.
/// - Only open pull requests are listed, except by
///   [`InboxSection::Query`].
#[async_trait]
pub trait Forge: Send + Sync + 'static {
    fn kind(&self) -> ForgeKind;

    /// The web host this backend serves (`github.com`, `gitlab.com`, ...),
    /// the same as [`RepoRef::host`] for its repositories.
    fn host(&self) -> &str;

    /// What this forge supports, for the UI to hide the rest. Fixed after
    /// construction; Forgejo's depends on the server version it probed.
    fn capabilities(&self) -> Capabilities;

    /// The signed-in account.
    /// GitHub `viewer`; GitLab `currentUser` or `GET /user`; Forgejo
    /// `GET /user`.
    async fn whoami(&self) -> Result<User>;

    /// The pull requests of one sidebar section, most recently updated
    /// first, at most [`INBOX_LIMIT`].
    ///
    /// - GitHub [G8]: `search(type: ISSUE)` with `is:pr is:open
    ///   review-requested:@me` / `author:@me` / `repo:o/r`, fetching the
    ///   full pull request fragment (refs, `reviewDecision`,
    ///   `statusCheckRollup`, labels, stats) in the same query.
    /// - GitLab [L8]: `currentUser.reviewRequestedMergeRequests(state:
    ///   opened)` and `authoredMergeRequests`, with `headPipeline.status`;
    ///   a repo is `project(fullPath).mergeRequests(state: opened)`.
    /// - Forgejo [F9]: `GET /repos/issues/search?type=pulls&state=open`
    ///   with `review_requested=true` or `created=true`; a repo is `GET
    ///   /repos/{o}/{r}/pulls?state=open`. Search rows lack refs, so fetch
    ///   each pull request (bounded concurrency). `ci` may stay `None`.
    async fn inbox(&self, section: &InboxSection) -> Result<Vec<PullRequest>>;

    /// One pull request with every field the forge can fill.
    ///
    /// GitHub: `repository.pullRequest(number)`; GitLab:
    /// `project.mergeRequest(iid)` (`diffRefs` for `merge_base`); Forgejo:
    /// `GET /repos/{o}/{r}/pulls/{i}` plus `GET .../reviews` for reviewers
    /// and the combined commit status for `ci` [F6].
    async fn pull_request(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest>;

    /// Every thread: inline review threads with anchors, and the
    /// conversation (anchor `None`), oldest first. The viewer's own pending
    /// review comments are left out.
    ///
    /// - GitHub [G3]: `reviewThreads{isResolved, isOutdated, line,
    ///   startLine, diffSide, startDiffSide, originalLine, path, comments}`,
    ///   plus issue comments and review summaries with a body (`verdict`).
    /// - GitLab [L3]: `discussions{resolvable, resolved, resolvedBy,
    ///   notes{position}}`; system notes are left out; `outdated` when the
    ///   note's position head differs from the current `diffRefs.headSha`
    ///   and cannot be mapped.
    /// - Forgejo [F1][F2][F3]: `GET .../reviews` then `GET
    ///   .../reviews/{id}/comments` per review; group comments by path,
    ///   side and line into one thread (oldest comment's id is the thread
    ///   id); `resolver` non-null means resolved; `stale` review comments
    ///   are outdated. Conversation: `GET /repos/{o}/{r}/issues/{i}/comments`.
    async fn threads(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<Thread>>;

    /// The checks of the head commit, latest attempt of each.
    ///
    /// - GitHub [G5][G6]: `statusCheckRollup.contexts` (check runs with
    ///   steps and `databaseId` as the job id, and status contexts).
    /// - GitLab [L5]: `headPipeline.stages.jobs` (no steps [L6]); `group`
    ///   is the stage.
    /// - Forgejo [F6][F7]: `GET .../commits/{sha}/statuses` (one per job,
    ///   context `workflow / job (event)`, latest per context), joined with
    ///   `actions/runs?head_sha=` for run ids; no steps.
    async fn checks(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<CheckRun>>;

    /// The stack the pull request is in, bottom first.
    ///
    /// - GitHub [G9]: `PullRequest.stack{entries}`; when `stack` is null,
    ///   the base-branch rule.
    /// - GitLab [L9]: `MergeRequest.stack`; when null or `[]`, the
    ///   base-branch rule.
    /// - Forgejo: the base-branch rule over `GET /repos/{o}/{r}/pulls?state=open`.
    ///
    /// The rule is [`crate::stack_by_base_branch`]; feed it the repo's open
    /// pull requests.
    async fn stack(&self, repo: &RepoRef, number: PrNumber) -> Result<Stack>;

    /// GitHub's per-file viewed state [G4] (`PullRequest.files{path,
    /// viewerViewedState}`). Forges without it ([L4], [F5]) keep the
    /// default, which says so; the app then uses its local viewed state.
    async fn viewed_files(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<ViewedFile>> {
        let _ = (repo, number);
        Err(Error::Unsupported {
            kind: self.kind(),
            feature: "viewed files",
        })
    }

    /// What opening a pull request needs, at once. The default runs the
    /// three reads concurrently; GitHub and GitLab override it with one
    /// GraphQL query, as the design asks.
    async fn snapshot(&self, repo: &RepoRef, number: PrNumber) -> Result<Snapshot> {
        let (pull_request, threads, checks) = tokio::try_join!(
            self.pull_request(repo, number),
            self.threads(repo, number),
            self.checks(repo, number),
        )?;
        Ok(Snapshot {
            pull_request,
            threads,
            checks,
        })
    }

    // ---- writes ------------------------------------------------------------
    //
    // Rules for every write:
    // - It takes the target repository explicitly and touches nothing else.
    // - It sends with `RetryMode::Never`: a 5xx or a lost response is
    //   returned to the caller, never resent (it may have been applied).
    // - It returns the created or updated object, mapped like a read, so the
    //   UI patches its state without a re-read. Deletes, re-runs and
    //   forge-side suggestion applies return `()`: there is nothing left to
    //   show, or the result is a new head commit or attempt that the next
    //   read picks up.
    // - A write the forge lacks keeps the default below (`Unsupported`, which
    //   `capabilities()` already reports), never an empty success.

    /// Starts the viewer's pending review on `commit` (the head the viewer is
    /// looking at), with no comments yet.
    ///
    /// - GitHub [G1]: `addPullRequestReview(pullRequestId, commitOID)` with
    ///   no `event`. One pending review per person and pull request.
    /// - GitLab [L1]: no review object; nothing is sent, and the review's id
    ///   names the merge request. Draft notes are the comments.
    /// - Forgejo [F1]: `POST .../pulls/{i}/reviews` with `event: PENDING`
    ///   and `commit_id`.
    ///
    /// Gated by [`Capabilities::queued_review`].
    async fn start_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        commit: &Sha,
    ) -> Result<PendingReview> {
        let _ = (repo, number, commit);
        unsupported(self.kind(), "queued reviews")
    }

    /// The viewer's pending review with its comments, `None` when there is
    /// none. Used to resume a review started in another session or on the
    /// web.
    ///
    /// - GitHub [G1]: `reviews(states: PENDING)` and the review threads
    ///   whose comments belong to it.
    /// - GitLab [L1]: `GET .../draft_notes` (`None` when empty).
    /// - Forgejo [F1]: `GET .../reviews` with `state: PENDING`, then its
    ///   comments.
    async fn pending_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<Option<PendingReview>> {
        let _ = (repo, number);
        unsupported(self.kind(), "queued reviews")
    }

    /// Queues one comment in the pending review: a new thread at an anchor,
    /// or a reply in an existing thread.
    ///
    /// - GitHub [G1][G2]: `addPullRequestReviewThread(pullRequestReviewId,
    ///   path, line, side, startLine, startSide, subjectType)`; a reply is
    ///   `addPullRequestReviewThreadReply(pullRequestReviewId, ...)`.
    /// - GitLab [L1][L2]: `POST .../draft_notes` with `position` (and
    ///   `line_range` for a range), or `in_reply_to_discussion_id`.
    /// - Forgejo [F1][F2]: `POST .../reviews/{id}/comments` with
    ///   `new_position` or `old_position` (v17: `extra_lines_count` for a
    ///   range); a reply is a comment on the thread's path and line.
    ///
    /// Gated by [`Capabilities::queued_review`]; a range by
    /// [`Capabilities::multi_line_comments`], a file anchor by
    /// [`Capabilities::file_comments`] (both [`Error::Unsupported`] when
    /// missing).
    async fn add_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
        comment: &NewComment,
    ) -> Result<PendingComment> {
        let _ = (repo, number, review, comment);
        unsupported(self.kind(), "queued reviews")
    }

    /// Removes one comment from the pending review (the viewer's own only).
    ///
    /// GitHub [G1] `deletePullRequestReviewComment`; GitLab [L1] `DELETE
    /// .../draft_notes/{id}`; Forgejo [F1] `DELETE
    /// .../reviews/{id}/comments/{comment}`.
    async fn delete_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        comment: &CommentId,
    ) -> Result<()> {
        let _ = (repo, number, comment);
        unsupported(self.kind(), "queued reviews")
    }

    /// Changes the text of one comment of the pending review (the viewer's
    /// own only) and returns it as it now reads.
    ///
    /// GitHub [G1] `updatePullRequestReviewComment`; GitLab [L1] `PUT
    /// .../draft_notes/{id}`. Forgejo has no edit for a review comment
    /// ([`Capabilities::edit_queued`] is false): the default; the app deletes
    /// the comment and adds it again.
    async fn edit_review_comment(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        comment: &PendingComment,
        body: &str,
    ) -> Result<PendingComment> {
        let _ = (repo, number, comment, body);
        unsupported(self.kind(), "editing queued comments")
    }

    /// Submits the pending review with a verdict and a summary body (may be
    /// empty for `Comment` when the review has comments).
    ///
    /// - GitHub [G1]: `submitPullRequestReview(event: COMMENT | APPROVE |
    ///   REQUEST_CHANGES, body)`.
    /// - GitLab [L1]: `POST .../draft_notes/bulk_publish` with `note` and
    ///   `reviewer_state`, then `POST .../approve` for `Approve`; the
    ///   returned review reads `reviewState`.
    /// - Forgejo [F1]: `POST .../reviews/{id}` with `{event, body}`;
    ///   `RequestChanges` needs a body.
    ///
    /// On your own pull request `Approve` and `RequestChanges` are refused
    /// unless [`Capabilities::own_pull_request_verdicts`]; the forge's error
    /// is returned as it comes.
    async fn submit_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
        verdict: Verdict,
        body: &str,
    ) -> Result<Review> {
        let _ = (repo, number, review, verdict, body);
        unsupported(self.kind(), "queued reviews")
    }

    /// Drops the pending review and its comments.
    ///
    /// GitHub [G1] `deletePullRequestReview`; GitLab [L1] `DELETE` of each
    /// draft note; Forgejo [F1] `DELETE .../reviews/{id}`.
    async fn discard_review(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: &ReviewId,
    ) -> Result<()> {
        let _ = (repo, number, review);
        unsupported(self.kind(), "queued reviews")
    }

    /// Replies in a thread at once (not queued; queue a reply with
    /// [`Self::add_review_comment`] and [`PendingPlace::Reply`]).
    ///
    /// - GitHub [G2]: `addPullRequestReviewThreadReply(pullRequestReviewThreadId,
    ///   body)`.
    /// - GitLab [L2]: `POST .../discussions/{id}/notes`.
    /// - Forgejo [F2] ([`ReplyMode::SamePathAndLine`]): a one-comment review
    ///   submitted as COMMENT on the thread's path and line, which joins the
    ///   thread on the next read.
    async fn reply(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        thread: &ThreadId,
        body: &str,
    ) -> Result<Comment> {
        let _ = (repo, number, thread, body);
        unsupported(self.kind(), "replies")
    }

    /// Resolves or unresolves a thread and returns it as it now reads.
    ///
    /// GitHub [G3] `resolveReviewThread` / `unresolveReviewThread`; GitLab
    /// [L3] `PUT .../discussions/{id}` with `resolved`. Forgejo [F3] has no
    /// API ([`ResolveMode::ReadOnly`]): the default.
    async fn set_resolved(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<Thread> {
        let _ = (repo, number, thread, resolved);
        unsupported(self.kind(), "resolving threads")
    }

    /// Adds a top-level conversation comment.
    ///
    /// GitHub `addComment(subjectId: <pull request id>)`; GitLab `POST
    /// .../merge_requests/{iid}/notes`; Forgejo `POST
    /// /repos/{o}/{r}/issues/{i}/comments`.
    async fn add_comment(&self, repo: &RepoRef, number: PrNumber, body: &str) -> Result<Comment> {
        let _ = (repo, number, body);
        unsupported(self.kind(), "conversation comments")
    }

    /// Marks a file viewed or unviewed on the forge.
    ///
    /// GitHub [G4] `markFileAsViewed` / `unmarkFileAsViewed`. GitLab [L4]
    /// and Forgejo [F5] have no API ([`ViewedSync::Local`]): the default.
    async fn set_viewed(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        path: &str,
        viewed: bool,
    ) -> Result<ViewedFile> {
        let _ = (repo, number, path, viewed);
        unsupported(self.kind(), "viewed files")
    }

    /// Applies suggestions through the forge as one commit on the head.
    ///
    /// GitLab [L4] `PUT /suggestions/{id}/apply`, or `PUT
    /// /suggestions/batch_apply` for several. GitHub and Forgejo [F4] have
    /// no API: the default answers [`Error::ApplyLocally`] and the `repo`
    /// crate commits the change locally.
    async fn apply_suggestions(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        suggestions: &[SuggestionId],
    ) -> Result<()> {
        let _ = (repo, number, suggestions);
        Err(Error::ApplyLocally { kind: self.kind() })
    }

    /// Re-runs one finished job; the new attempt shows in the next
    /// [`Self::checks`].
    ///
    /// GitHub [G7] `POST .../actions/jobs/{id}/rerun`; GitLab [L7] `POST
    /// /projects/:id/jobs/:job_id/retry`; Forgejo v17 [F8] `POST
    /// .../actions/jobs/{id}/rerun`. Gated by [`Capabilities::retry_job`].
    async fn retry_job(&self, repo: &RepoRef, job: &JobId) -> Result<()> {
        let _ = (repo, job);
        unsupported(self.kind(), "re-running jobs")
    }

    /// Re-runs the failed jobs of a run or pipeline.
    ///
    /// GitHub [G7] `POST .../actions/runs/{id}/rerun-failed-jobs`; GitLab
    /// [L7] `POST /projects/:id/pipelines/:id/retry`; Forgejo v17 [F8] has
    /// only a whole-run `POST .../actions/runs/{id}/rerun`, so it re-runs
    /// each failed job. Gated by [`Capabilities::retry_job`].
    async fn retry_failed_jobs(&self, repo: &RepoRef, run: &RunId) -> Result<()> {
        let _ = (repo, run);
        unsupported(self.kind(), "re-running jobs")
    }

    // ---- pull request lifecycle --------------------------------------------
    //
    // Same rules as the writes above. A lifecycle write returns the pull
    // request as it now reads (a re-read where the forge's answer lacks
    // fields). None of them checks the viewer's rights beforehand: the
    // forge's refusal is returned as it comes.

    /// Whether the pull request can be merged now, and what blocks it.
    ///
    /// - GitHub: `mergeable`, `mergeStateStatus`, `reviewDecision`,
    ///   `isDraft` and the head's check rollup. `BLOCKED` with no named
    ///   reason is [`MergeBlocker::BranchProtection`].
    /// - GitLab: `detailed_merge_status`.
    /// - Forgejo: `mergeable`, `draft`, the reviews and, when the base
    ///   branch has a protection rule the viewer can read, its status check
    ///   and approval rules.
    ///
    /// [`MergeBlocker::BranchProtection`]: crate::model::MergeBlocker::BranchProtection
    async fn mergeability(&self, repo: &RepoRef, number: PrNumber) -> Result<Mergeability> {
        let _ = (repo, number);
        unsupported(self.kind(), "merging")
    }

    /// The merge methods the repository allows and the one it preselects.
    ///
    /// - GitHub: `mergeCommitAllowed`, `squashMergeAllowed`,
    ///   `rebaseMergeAllowed`, `viewerDefaultMergeMethod`.
    /// - GitLab: the project's `merge_method` (`merge` and `rebase_merge`
    ///   make a merge commit, `ff` is a rebase) and `squash_option`.
    /// - Forgejo: `allow_merge_commits`, `allow_squash_merge`,
    ///   `allow_rebase`, `default_merge_style`.
    async fn merge_methods(&self, repo: &RepoRef) -> Result<MergeSettings> {
        let _ = repo;
        unsupported(self.kind(), "merging")
    }

    /// Merges the pull request and returns it as merged.
    ///
    /// The forge refuses when the head is not `options.sha` any more, and
    /// the backend refuses a method the repository does not allow. The head
    /// branch is deleted only when `options.delete_branch` is set and it
    /// lives in the pull request's own repository; a failed delete after a
    /// successful merge is not an error.
    ///
    /// - GitHub: `mergePullRequest(mergeMethod, commitHeadline, commitBody,
    ///   expectedHeadOid)`, then `DELETE .../git/refs/heads/{branch}`.
    /// - GitLab: `PUT .../merge_requests/:iid/merge` (`squash`,
    ///   `squash_commit_message`, `merge_commit_message`, `sha`,
    ///   `should_remove_source_branch`). The method must be the project's.
    /// - Forgejo: `POST .../pulls/{i}/merge` (`Do`, `MergeTitleField`,
    ///   `MergeMessageField`, `head_commit_id`, `delete_branch_after_merge`).
    ///
    /// Gated by [`Capabilities::merge`].
    async fn merge(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        options: &MergeOptions,
    ) -> Result<PullRequest> {
        let _ = (repo, number, options);
        unsupported(self.kind(), "merging")
    }

    /// Turns auto-merge on with `Some(options)` or off with `None`, and
    /// returns the pull request; [`Mergeability::auto_merge`] reads it back.
    /// GitLab merges at once a merge request that could be merged already.
    /// `options.delete_branch` reaches only GitLab (the merge then removes
    /// the branch); GitHub deletes a merged branch by its repository setting.
    ///
    /// GitHub `enablePullRequestAutoMerge` / `disablePullRequestAutoMerge`;
    /// GitLab `PUT .../merge` with `auto_merge` and `POST
    /// .../cancel_merge_when_pipeline_succeeds`. Gated by
    /// [`Capabilities::auto_merge`].
    async fn set_auto_merge(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        options: Option<&MergeOptions>,
    ) -> Result<PullRequest> {
        let _ = (repo, number, options);
        unsupported(self.kind(), "auto-merge")
    }

    /// Closes the pull request without merging.
    ///
    /// GitHub `closePullRequest`; GitLab `PUT .../merge_requests/:iid` with
    /// `state_event=close`; Forgejo `PATCH .../pulls/{i}` with `state:
    /// closed`. Gated by [`Capabilities::close_reopen`].
    async fn close(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        let _ = (repo, number);
        unsupported(self.kind(), "closing")
    }

    /// Reopens a closed (not merged) pull request.
    ///
    /// GitHub `reopenPullRequest`; GitLab `state_event=reopen`; Forgejo
    /// `state: open`.
    async fn reopen(&self, repo: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        let _ = (repo, number);
        unsupported(self.kind(), "reopening")
    }

    /// Marks the pull request draft (`true`) or ready for review (`false`).
    ///
    /// - GitHub: `convertPullRequestToDraft` / `markPullRequestReadyForReview`.
    /// - GitLab: `mergeRequestSetDraft(draft)`.
    /// - Forgejo: no draft flag in the API; the title's `WIP:` prefix is
    ///   added or removed (`PATCH .../pulls/{i}` with `title`).
    ///
    /// Gated by [`Capabilities::set_draft`].
    async fn set_draft(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        draft: bool,
    ) -> Result<PullRequest> {
        let _ = (repo, number, draft);
        unsupported(self.kind(), "draft state")
    }

    // ---- people and labels -------------------------------------------------
    //
    // Same rules as the writes above. Each write returns the pull request as
    // it now reads, with `reviewers`, `assignees` and `labels` filled, and
    // sends only what changed: a set is diffed against the current one first.

    /// The accounts that can be asked to review or be assigned, whose login
    /// or name starts with `query` (every one for an empty query), at most
    /// [`COLLABORATOR_LIMIT`]. The list is kept in memory per repository and
    /// query for a few minutes.
    ///
    /// - GitHub: `repository.assignableUsers(query:, first: 100)`.
    /// - GitLab: `GET /projects/:id/users?search=` (members, inherited ones
    ///   included).
    /// - Forgejo: `GET /repos/{o}/{r}/assignees`, filtered here (it has no
    ///   search).
    async fn collaborators(&self, repo: &RepoRef, query: &str) -> Result<Vec<User>> {
        let _ = (repo, query);
        unsupported(self.kind(), "listing collaborators")
    }

    /// Every label of the repository, kept in memory per repository for a
    /// few minutes.
    ///
    /// GitHub `repository.labels`; GitLab `GET /projects/:id/labels` (group
    /// labels included); Forgejo `GET /repos/{o}/{r}/labels`.
    async fn labels(&self, repo: &RepoRef) -> Result<Vec<Label>> {
        let _ = repo;
        unsupported(self.kind(), "listing labels")
    }

    /// Asks people (and, where [`Capabilities::team_reviewers`], teams: an
    /// account of kind [`AccountKind::Team`]) to review. Anyone already asked
    /// stays asked; an account that already reviewed is asked again.
    ///
    /// - GitHub: `POST .../pulls/{n}/requested_reviewers` with `reviewers`
    ///   (logins) and `team_reviewers` (slugs).
    /// - GitLab: `PUT .../merge_requests/:iid` with `reviewer_ids`, the
    ///   current ones plus the new.
    /// - Forgejo: `POST .../pulls/{i}/requested_reviewers` with `reviewers`
    ///   and `team_reviewers`.
    ///
    /// The forge refuses what it refuses (GitHub and Forgejo: the author);
    /// its error is returned as it comes.
    ///
    /// [`AccountKind::Team`]: crate::model::AccountKind::Team
    async fn request_reviewers(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
        teams: &[User],
    ) -> Result<PullRequest> {
        let _ = (repo, number, users, teams);
        unsupported(self.kind(), "requesting reviewers")
    }

    /// Withdraws review requests. Accounts that are not reviewers are left
    /// alone.
    ///
    /// GitHub and Forgejo `DELETE .../requested_reviewers`; GitLab
    /// `reviewer_ids` without them.
    async fn remove_reviewers(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
        teams: &[User],
    ) -> Result<PullRequest> {
        let _ = (repo, number, users, teams);
        unsupported(self.kind(), "requesting reviewers")
    }

    /// Makes `users` the pull request's assignees, adding and removing the
    /// difference.
    ///
    /// - GitHub: `POST` and `DELETE .../issues/{n}/assignees`.
    /// - GitLab: `PUT .../merge_requests/:iid` with `assignee_ids`.
    /// - Forgejo: `PATCH .../issues/{i}` with `assignees` (it takes the whole
    ///   list).
    async fn set_assignees(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        users: &[User],
    ) -> Result<PullRequest> {
        let _ = (repo, number, users);
        unsupported(self.kind(), "assignees")
    }

    /// Makes `labels` (names of labels the repository has; a name it lacks is
    /// [`Error::NotFound`], never a new label) the pull request's labels,
    /// adding and removing the difference.
    ///
    /// - GitHub: `POST .../issues/{n}/labels`, `DELETE .../labels/{name}`.
    /// - GitLab: `PUT .../merge_requests/:iid` with `add_labels` and
    ///   `remove_labels`.
    /// - Forgejo: `POST .../issues/{i}/labels`, `DELETE .../labels/{id}`.
    async fn set_labels(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        labels: &[String],
    ) -> Result<PullRequest> {
        let _ = (repo, number, labels);
        unsupported(self.kind(), "labels")
    }

    // ---- CI detail and polling ----------------------------------------------
    //
    // REST on every forge, so polling is conditional where the forge sends
    // ETags. `checks()` stays the rollup; these are the run / job / step tree
    // behind it and the job logs.

    /// Every workflow run or pipeline of commit `sha`, newest first, with
    /// jobs and (where the forge has them) steps.
    ///
    /// - GitHub [G6]: `GET .../actions/runs?head_sha=`, then `GET
    ///   .../actions/runs/{id}/jobs` (latest attempt, steps included).
    /// - GitLab [L5][L6]: `GET /projects/:id/pipelines?sha=`, then `GET
    ///   .../pipelines/:id/jobs`; no steps.
    /// - Forgejo v16+ [F6][F7]: `GET .../actions/runs?head_sha=`, `GET
    ///   .../runs/{id}/jobs`, and on v17 `GET .../actions/jobs/{id}` per job
    ///   for its steps. Before v16, runs without jobs.
    async fn ci_runs(&self, repo: &RepoRef, sha: &Sha) -> Result<Vec<CiRun>> {
        let _ = (repo, sha);
        unsupported(self.kind(), "CI runs")
    }

    /// One run with its jobs; steps only with [`CiDetail::Steps`] (they
    /// cost a request per job on Forgejo).
    async fn ci_run(&self, repo: &RepoRef, run: &RunId, detail: CiDetail) -> Result<CiRun> {
        let _ = (repo, run, detail);
        unsupported(self.kind(), "CI runs")
    }

    /// One job's current state (and steps where the forge has them).
    ///
    /// GitHub `GET .../actions/jobs/{id}`; GitLab `GET /projects/:id/jobs/:id`;
    /// Forgejo v16+ `GET .../actions/jobs/{id}` (steps from v17).
    async fn ci_job(&self, repo: &RepoRef, job: &JobId) -> Result<CiJob> {
        let _ = (repo, job);
        unsupported(self.kind(), "CI jobs")
    }

    /// The job's log from byte `from` on, at most
    /// [`crate::ci_log::LOG_FETCH_MAX`] bytes per call. Reads the job first,
    /// so `complete` is only set once the job is finished and the chunk
    /// reaches the end. Never retried from a cache: every call asks the
    /// forge (`Range: bytes=from-`), and a forge that ignores the range has
    /// its answer cut to `from` in place.
    ///
    /// - GitHub [G6]: `GET .../actions/jobs/{id}/logs`, which redirects to
    ///   blob storage; only that redirect is followed. The blob exists only
    ///   once the job has ended (`404 BlobNotFound` before), so a running
    ///   job reads as empty and its whole log arrives at the end.
    /// - GitLab [L6]: `GET /projects/:id/jobs/:id/trace` with `Range`. A
    ///   running job's trace ignores the range (whole `200`s, cut here) and
    ///   grows in bursts every few tens of seconds.
    /// - Forgejo v16+ [F7]: `GET .../actions/jobs/{id}/logs` with `Range`
    ///   and the API token; it grows every second or so. Before the first
    ///   lines it answers `500` ("file does not exist"), read as empty.
    ///
    /// Gated by [`Capabilities::job_logs`].
    async fn job_log(&self, repo: &RepoRef, job: &JobId, from: u64) -> Result<LogChunk> {
        let _ = (repo, job, from);
        unsupported(self.kind(), "job logs")
    }

    /// What a subscription to a pull request polls. The default reads the
    /// whole pull request; backends override it with conditional REST.
    ///
    /// - GitHub: `GET .../pulls/{n}`, `GET .../commits/{sha}/check-runs` and
    ///   `GET .../commits/{sha}/status`, all with ETags.
    /// - GitLab: `GET /projects/:id/merge_requests/:iid` with an ETag (its
    ///   `head_pipeline` carries the CI state).
    /// - Forgejo: `GET .../pulls/{n}` (`updated_at`; no ETag) and `GET
    ///   .../commits/{sha}/status`.
    async fn pull_request_pulse(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestPulse> {
        let pull_request = self.pull_request(repo, number).await?;
        Ok(PullRequestPulse {
            state: pull_request.state,
            head: pull_request.head.sha,
            updated_at: pull_request.updated_at,
            ci: pull_request.ci.unwrap_or_default(),
        })
    }
}

/// The answer of a write the forge has no API for.
fn unsupported<T>(kind: ForgeKind, feature: &'static str) -> Result<T> {
    Err(Error::Unsupported { kind, feature })
}
