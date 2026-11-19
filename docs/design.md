# review: design

A native code reviewer for GitHub, Forgejo and GitLab, built on gpui with a Vercel
(Geist) look. Runs on Windows and Linux.

Work not yet done is in `ROADMAP.md`.

## What it does

1. **Inbox**: pull requests across every configured forge, in sections:
   created by me, review requested from me, and other people's open ones per
   repo. Any of them can be opened and reviewed, my own included.
2. **Diffs**: unified or split, syntax highlighted, read from local git objects.
3. **Review**: see existing threads inline; reply, resolve, write suggestions,
   mark files viewed; queue comments and submit them as one review.
4. **Stacks**: show a pull request's stack and step through it.
5. **CI**: status in the sidebar, run / job / step tree, logs, retry.
6. **Subscriptions**: watch a pull request or a run and get notified on change.
7. **Terminal**: an embedded terminal, local or over SSH to a configured PC,
   opened in the pull request's worktree, for running Claude Code.

## Responsiveness rules

These are the constraints every part is designed around. The benchmarks
(`just bench`) check the numbers.

- **The main thread never waits** on disk, network, git or a child process.
  It only reads in-memory models and paints.
- **Paint from cache, then refresh.** Every list and thread is drawn from the
  local SQLite cache at once; the network result patches it when it arrives.
- **Work only for what is visible.** The file list comes first (a tree diff),
  then hunks for visible files, then the rest in the background. Highlighting
  and word-level diffs are computed for visible rows and cached by blob id.
- **Virtualize every long list**: diff rows, log lines, the inbox.
- **Prefetch on intent.** Selecting or hovering a pull request in the inbox
  starts its git fetch and thread load before it is opened.
- **Budgets** (p99 unless noted, on the Windows development PC): 8 ms per frame while scrolling,
  100 ms (p95) from opening a cached pull request to the first diff paint,
  16 ms from key press to paint in the composer, 350 ms from launch to an
  inbox drawn from cache.

## Window layout and docking

- **Sidebar**: the inbox sections, each collapsible with a count, and under
  the open pull request its files and CI rollup. Sections are saved queries
  (author is me, reviewer is me, repo is X and state is open), so more can be
  added in config without code.
- **Main area** is a dock: every view (diff, conversation, CI job log,
  terminal) is a panel that can be a tab, be split left / right / up / down,
  or be dragged to another split. Typical use: diff on the left, the failing
  job's log or a terminal on the right.
- The dock is `gpui-component`'s `DockArea`, which holds the frame budget
  with a diff panel inside. Panels do not know which dock area hosts them.
- The layout is saved per window and restored on launch.

## Themes and fonts

- A theme is data: color tokens, syntax colors, corner radius and font
  settings. Panels and controls are flat: 1px borders, no shadows or textures.
  Views read tokens, never literals, so a new theme is a new table of values.
- Shipped themes: Vercel (from the public Geist design system), dark and
  light, plus "system" to follow the OS. Users add their own theme files in
  `<config dir>/review/themes`.
- Two font settings: interface (chrome, lists, prose) and buffer (diff, log,
  terminal, composer). Each has family, size, line height and fallbacks.
  `diff_view` and `term_view` already take their font and colors from a style
  struct, so a change rebuilds the struct and re-shapes cached lines.
- Colors come from `theme.c` and fonts from the font settings; views hold
  no literals.

### Buttons

Toolbars and headers use secondary buttons (dark fill, hairline border,
normal text). The solid accent (primary) style is only for the one
confirming action inside a dialog or the merge box. Danger actions keep
danger text and fill only inside their confirmation.

## Workspace layout

```
crates/
  forge/      domain model, the Forge trait, GitHub / Forgejo / GitLab backends
  store/      SQLite cache: pull requests, threads, checks, drafts, viewed, subscriptions
  repo/       git: object stores, fetch, tree and blob diff, worktrees
  diff/       diff model, row projection (unified / split), highlighting, word diff
  diff_view/  gpui element painting a virtualized diff
  log_view/   gpui element painting a virtualized CI log with ANSI colors
  scroll/     shared scrollbar and middle-click autoscroll
  term/       terminal emulation and PTY (local shell, ssh)
  term_view/  gpui element painting a terminal grid
  theme/      themes as data: color tokens, syntax colors, radii, fonts
  perf/       timing samples and budget reports for the benchmarks
  app/        the gpui application (binary `review`)
  app_tests/  headless UI tests of the app on gpui's test platform
```

`forge`, `store`, `repo`, `diff`, `term` and `perf` have no gpui dependency,
so they are tested and benchmarked headless. `diff_view`, `log_view` and
`term_view` paint from a plain style struct, built from the active `theme::Theme`
and font settings.

**Frame time** in the budgets is the CPU time to build one frame: from the
start of the window's draw to the end of its last paint, measured inside the
app. GPU and present time are not included; dropped frames (an interval over
1.5 refresh periods) are counted and reported beside it.

## Dependencies and what comes from Zed

| Need | Choice | License |
|---|---|---|
| UI framework | `gpui-pre` 0.3.12 (weekly crates.io snapshot of Zed's gpui, pinned exactly) | Apache-2.0 |
| Row-height index | `gpui-pre-sum-tree` (Zed's `sum_tree`, published with `gpui-pre`) | Apache-2.0 |
| Comment input, markdown, docking | `gpui-component` `Input`, text view and `DockArea`, themed with the active theme's colors | Apache-2.0 |
| Terminal emulation + PTY | `alacritty_terminal` (the emulator Zed's terminal uses) | Apache-2.0 |
| Git objects and diff | `gix` (uses `imara-diff`); the `git` binary for network fetch and push | MIT / Apache-2.0 |
| Highlighting | `tree-sitter` + per-language grammars | MIT |
| HTTP | `reqwest` on one tokio runtime thread, HTTP/2 keep-alive | MIT / Apache-2.0 |
| Cache | `rusqlite` (bundled, WAL) | MIT |
| Notifications | `tauri-winrt-notification` (Windows toast); gpui's own system notifications elsewhere | MIT / Apache-2.0 |

In `zed-industries/zed` at the time of writing, only `gpui`, `sum_tree`,
`collections` and `util` are Apache-2.0 and free of the in-tree gpui. `rope`,
`text`, `language`, `buffer_diff`, `multi_buffer`, `editor`, `terminal`,
`markdown` and `ui_input` are GPL-3.0-or-later, unpublished, and (all but a
few) depend on Zed's in-tree gpui. So the hybrid takes `sum_tree` plus the same
third-party crates Zed builds on. The app is AGPL-3.0-or-later, and GPLv3
section 13 lets it combine with GPLv3 code, so Zed's GPL crates stay
available, but they need Zed's in-tree gpui rather than
`gpui-pre`, and they pull in most of Zed.

**Why `gpui-pre`**: crates.io `gpui` has not moved since 0.2.2 (October 2025).
`gpui-component` 0.6 and later build on `gpui-pre`, a weekly snapshot of
Zed's main branch, and 0.10.0 pins `gpui-pre` `=0.3.12`. The workspace pins
the same. Upgrades happen in step with
`gpui-component` releases.

Widgets this app needs beyond `gpui-component` (list rows, for example) live
in this repo, as do the app-specific views (diff, log, terminal grid).

## Forge layer

One trait, three backends. The app only sees the shared model.

```
Forge: list_pull_requests, pull_request, threads, checks, job_log (stream),
       submit_review, reply, set_resolved, set_viewed, retry_job,
       mergeability, merge_methods, merge, set_auto_merge, close, reopen,
       set_draft, collaborators, labels, request_reviewers, remove_reviewers,
       set_assignees, set_labels, capabilities
Model: PullRequest, Thread { anchor, resolved, outdated, comments },
       Anchor { path, side, line, start_line, commit }, Review, DraftComment,
       Check { run, jobs, steps, state }, Stack,
       Mergeability { mergeable, reasons: [MergeBlocker], auto_merge },
       MergeSettings { allowed: [MergeMethod], default },
       MergeOptions { method, commit_title, commit_message, delete_branch, sha }
```

- **Auth**: tokens are read from the CLIs at startup (`gh auth token`, the
  `glab` and `fj` config) and held in memory only. The app stores no token.
- **Transport**: GraphQL where the forge has it (GitHub, GitLab), so a pull
  request with its threads and checks is one request; REST elsewhere.
  Conditional requests (ETag) for polling.
- **Capabilities**: each backend reports what it supports and the UI hides the
  rest. Where a forge has no API for a feature the app keeps the state locally
  (for example viewed files, keyed by path and blob id so it resets on change).
- **Stacks** are derived the same way on every forge: pull request A's base
  branch is pull request B's head branch. A native stack API is used where one
  exists.
- **Applying a suggestion** uses GitLab's own API (`PUT /suggestions/:id/apply`,
  `batch_apply`). On GitHub and Forgejo, which have no API for it, it is a
  local commit built from git objects on the pull request's head and pushed.
  Whether the viewer may push to the head is `PullRequest.head_writable`:
  GitHub `viewerCanApplySuggestion`, Forgejo the head repository's
  `permissions.push`. GitLab applies through its own API and reports it per
  suggestion (`Comment.suggestions` carries the GitLab suggestion ids, with
  `applied` and `appliable`).
- **Editing a queued comment**: GitHub (`updatePullRequestReviewComment`) and
  GitLab (`PUT .../draft_notes/{id}`) edit in place. Forgejo has no edit
  (`edit_queued` is false), so the app deletes the comment and adds it again.
- **Lifecycle writes** (merge, auto-merge, close, reopen, draft) follow the
  review-write rules: explicit repository, `RetryMode::Never`, the updated pull
  request returned (a re-read where the forge's answer lacks fields). A merge
  carries the head sha the caller looked at (`MergeOptions::sha`) and the forge
  refuses it when the head moved (GitHub `expectedHeadOid`, GitLab `sha`,
  Forgejo `head_commit_id`; each answers an error, checked live). The head
  branch is deleted only when `delete_branch` is set and the branch lives in
  the pull request's own repository: a fork's branch is never touched. The
  delete flag is always sent where the forge has one (GitLab
  `should_remove_source_branch`, Forgejo `delete_branch_after_merge`), so a
  repository default never deletes unasked; GitHub deletes with a separate
  `DELETE .../git/refs/heads/{branch}` after the merge, whose failure is not
  reported (the merge has happened). `mergeability` reports the forge's own
  verdict plus the blockers it can name; where the forge cannot say (Forgejo
  has no "checking" state, and reports a draft as unmergeable) the reasons
  are narrower, never invented. `merge_methods` is per repository; GitLab has
  one merge method per project, so a merge asks for it by name and the backend
  refuses another instead of letting the project's win.
- **People and labels** (reviewers, assignees, labels) are read on every
  pull request (`reviewers` with each one's `ReviewState` and, by
  `User.kind == Team`, whether it is a team; `assignees`; `labels` with color
  and description; `milestone`), in `pull_request`, `snapshot` and the inbox
  rows alike. `collaborators(repo, query)` (prefix of login or name, at most
  100) and `labels(repo)` feed the pickers and are kept in memory per repository
  for three minutes, on the HTTP client's clock. The writes follow the
  review-write rules and each returns the pull request as it now reads:
  `request_reviewers` and `remove_reviewers` (users, and teams where
  `Capabilities::team_reviewers`), `set_assignees` and `set_labels` (the wanted
  set, sent as a diff against the current one so a change nobody made is not
  sent). `set_labels` refuses a name the repository lacks with `NotFound`
  (after one fresh read of the label list), because GitHub and GitLab would
  otherwise create a label from a typo. The forge's own refusals (GitHub and
  Forgejo: the author as reviewer) come back as its error.
- **Review state** never says "approved" for an approval that no longer
  counts. Each `Reviewer` carries the forge's own `ReviewState`
  (`Requested`, `Pending`, `Commented`, `Approved`, `ChangesRequested`,
  `Dismissed`) plus two flags: `requested_again` (asked to review after an
  earlier review or approval; the state is then `Requested`) and `stale` (an
  approval given before the current head). `Reviewer::status()` turns those
  into the words the app shows: "review requested", "re-review requested",
  "approved", "approved before last push", "changes requested" (and
  "commented", "draft review", "dismissed"); `PullRequest::viewer_status`
  gives the viewer's own. `ReviewDecision::from_reviewers` is the shared rule:
  a change request wins; one current approval (not stale, not asked again)
  gives `Approved`; approvals that are all stale give `StaleApproval`;
  otherwise comments, then "needs review". A forge's own roll-up (GitHub
  `reviewDecision`, GitLab `approved` and `approvedBy`) is not trusted for
  approval, because each keeps an approval after a push or a re-request. Which
  approvals are stale is the forge's to say (see [G11], [L11], [F11]); where it
  cannot, an approval is not marked stale.
- **One pending review per person**: GitHub and Forgejo allow a single one per
  pull request, and GitLab publishes all of a person's drafts at once. So a
  lone comment is refused while a review is pending, instead of being posted
  or swept into the review.
- **GitLab verdicts** are system notes ("approved this merge request",
  "requested changes", "left review comments") that a background job writes
  0.3 to 6 s after the request. `submit_review` waits, bounded by the settle
  interval, for the note above the newest note id seen before it published,
  and fails with `NotReady` when an approve or request-changes note never
  comes, so a read right after sees the verdict.

Capability matrix, checked against the live APIs of GitHub (github.com),
GitLab 19.6.0-pre (gitlab.com) and Forgejo 17.0.1 (a self-hosted instance;
first read on 15.0.9, then re-read against the 17.0.1 OpenAPI spec and the
recorded fixtures). Where a note says a write is "per schema" or "per spec",
it was read from the forge's schema and not tried live. The exact APIs are in
the notes below the table.

| | GitHub | GitLab | Forgejo 17.0.1 |
|---|---|---|---|
| Queued review, submit with verdict (approve, request changes, comment) | yes [G1] | yes [L1]; request changes: yes | yes [F1]; a line range through `extra_lines_count` (v17, confirmed live) |
| Reply in a thread | yes [G2] | yes [L2] | partial [F2]: no reply API; a new comment on the same path and line joins the thread |
| Comment on a whole file | yes [G1] (`subjectType: FILE`) | yes [L1] (`position_type: file`; checked live as a draft note, read back as a file anchor) | no [F1] |
| Approve or request changes on your own pull request | no (GitHub forbids it) | yes, checked live on gitlab.com Free: `POST .../approve` answers 201 and a `bulk_publish` with `reviewer_state=requested_changes` is accepted on your own merge request, though the project's `merge_requests_author_approval` is false; request changes only moves the review state for a requested reviewer (the note "requested changes" appears either way) | no [F1]: 422 "approve your own pull is not allowed" (and "reject your own pull"), live on 17.0.1 |
| Resolve / unresolve | yes [G3] | yes [L3] | partial [F3]: state readable, writing is web-route only |
| Suggestion blocks rendered by forge | yes | yes [L4] | no [F4]: plain fenced code |
| Viewed files synced | yes [G4] | no [L4]: no API | partial [F5]: server state exists, web-route only |
| Check / pipeline status | yes [G5] | yes [L5] | yes [F6] (commit statuses and Actions runs) |
| Job list, steps, logs | yes [G6] | partial [L6]: jobs and log, no steps | yes [F7]: jobs per run, steps per job (one request each), logs (v17) |
| Retry job | yes [G7] | yes [L7] | yes [F8] (v17): API re-run of a job or a run |
| Inbox: review requested of me, authored by me | yes [G8] | yes [L8] | yes [F9] |
| Native stacks | yes [G9] | partial [L9]: field exists, not confirmed on a real stack | no: use the base-branch rule |
| Apply a suggestion through the forge (not the default, see above) | no | yes [L4] | no |
| Mergeability with blockers, allowed merge methods | yes [G10] | yes [L10] | partial [F10]: `mergeable` plus reviews, and the protection rule when readable; no "computing" state |
| Merge (merge, squash, rebase) with expected head, optional branch delete | yes [G10] | partial [L10]: the project's one method (`merge`, `rebase_merge` or `ff`) plus an optional squash | yes [F10] |
| Auto-merge | yes [G10] | yes [L10] | not used [F10] |
| Close, reopen | yes [G10] | yes [L10] | yes [F10] |
| Draft toggle | yes [G10] | yes [L10] | partial [F10]: `WIP:` title prefix |
| Reviewers (request, withdraw), assignees, labels, with pickers | yes [G11] | yes [L11]: no review requests to groups | yes [F11] |
| Ask the author for a review of their own pull request | no [G11]: 422 | yes [L11] | no [F11]: 422 |

**How the app copes**
- GitHub: everything is available. Approve, request changes and comment are
  the three submit events.
- GitLab: viewed state is local (path and blob id), as designed. There is no
  step list, so a job is one log; split it on the `section_start` markers of
  the raw trace if a step list is wanted.
- Forgejo 17.0.1: hide thread "Resolve". Show resolved state read-only from
  `resolver`. Send "Reply" as a new review comment on the same path and line
  (the sandbox's seeded reply shares path and line with its parent, so the
  app groups them into one thread). Viewed state
  is local. Job and run status come from commit statuses joined with
  `actions/runs` and `runs/{run_id}/jobs`; logs, steps and re-run use the v17
  API. On 15.x and 16.x the capability probe hides what the version lacks (jobs
  and logs from 16, re-run from 17) and the app links out to the web page.
- The Forgejo web routes named below are what the forge's own UI uses, but they
  need a login session (cookie plus CSRF), not an API token. With `fj`'s token,
  `/{owner}/{repo}`, `/actions/runs/1` and its `jobs/0/.../logs` all answer 404
  on a private repo, while `/raw/...` accepts the token. The app has no login
  flow, so it does not build on those routes.
- Forgejo's API gaps close with upgrades: v16 adds the job list and log
  endpoints, v17 adds `rerun` (job and run), the step list on a single job and
  a line range on review comments. Resolve and reply endpoints are still
  missing from the v17.0.1 spec (they exist only on the unreleased v18
  development branch). Gate each by probing the endpoint or reading
  `/api/v1/version`, not by assuming one.
- Polling: GitHub REST and GitLab REST send ETags; GraphQL is POST and has
  none. Forgejo sends no ETag on `/pulls`, `/repos/issues/search` or reviews
  (checked on 17.0.1 as well); poll with `updated_at` / `since` and
  `X-Total-Count`, and page with `page` and `limit` (at most 50).

**GitHub APIs** (GraphQL unless marked REST)
- [G1] `addPullRequestReview(pullRequestId, commitOID, threads: [{path, line,
  side, startLine, startSide, body}])` with `event` left out makes a PENDING
  review (`reviews(states: [PENDING])` reads it, `deletePullRequestReview`
  drops it). More threads: `addPullRequestReviewThread(pullRequestReviewId,
  ...)`. Submit: `submitPullRequestReview(event: COMMENT | APPROVE |
  REQUEST_CHANGES)`. Review states: PENDING, COMMENTED, APPROVED,
  CHANGES_REQUESTED, DISMISSED.
- [G2] `addPullRequestReviewThreadReply(pullRequestReviewThreadId,
  pullRequestReviewId?, body)`; the link back is `PullRequestReviewComment.replyTo`.
- [G3] `resolveReviewThread` / `unresolveReviewThread(threadId)`. Read
  `PullRequestReviewThread{isResolved, isOutdated, viewerCanResolve, line,
  startLine, diffSide, path}` (confirmed live).
- [G4] `markFileAsViewed` / `unmarkFileAsViewed(pullRequestId, path)`; read
  `PullRequest.files.nodes{path, viewerViewedState}` (VIEWED, UNVIEWED,
  DISMISSED = viewed but changed since). The read is confirmed live; the writes
  are per schema.
- [G5] `commit.statusCheckRollup{state, contexts}` and
  `commit.checkSuites.checkRuns{databaseId, name, status, conclusion,
  detailsUrl, steps{number, name, status, conclusion}}` (confirmed live).
- [G6] Steps: `CheckRun.steps` (empty while queued and for non-Actions apps).
  `CheckRun.databaseId` is the Actions job id. Log, REST: `GET
  /repos/{o}/{r}/actions/jobs/{job_id}/logs` (302 to a blob URL, plain text;
  poll for a running job). Job detail, REST: `GET .../actions/jobs/{job_id}`.
  Checked live: the redirect goes to `*.blob.core.windows.net` (allowed, with
  `*.actions.githubusercontent.com`, https only); while the job runs the blob
  does not exist yet (404 BlobNotFound), so a running GitHub job cannot be
  tailed through REST: its log arrives whole when the job ends.
- [G7] REST: `POST /repos/{o}/{r}/actions/jobs/{job_id}/rerun`, `POST
  .../actions/runs/{run_id}/rerun-failed-jobs`, `POST .../actions/runs/{run_id}/rerun`.
  Non-Actions checks: `POST /repos/{o}/{r}/check-runs/{id}/rerequest`. GraphQL
  has only `rerequestCheckSuite`.
- [G8] `search(type: ISSUE, query: "is:pr is:open review-requested:@me")` and
  `author:@me` (both confirmed live). `user-review-requested:@me` leaves out
  team requests. Per pull request: `PullRequest.reviewRequests`.
- [G9] `PullRequest.stack{number, size, baseRefName, entries}` and
  `stackEntry{position}`; REST `GET /repos/{o}/{r}/stacks[/{n}]` (confirmed live
  on an 8-pull-request stack). Use the base-branch rule when `stack` is null.
- Applying a suggestion: no mutation or endpoint (`viewerCanApplySuggestion`
  is only a flag), so the local commit in the bullet above stays.
- [G10] Lifecycle, checked live on the sandbox. Read: `PullRequest.mergeable`
  (MERGEABLE, CONFLICTING, UNKNOWN: conflicts only), `mergeStateStatus`
  (CLEAN, UNSTABLE, HAS_HOOKS merge; DIRTY, BEHIND, BLOCKED, DRAFT, UNKNOWN do
  not), `reviewDecision`, `isDraft`, `autoMergeRequest`, the head's
  `statusCheckRollup.state`; `BLOCKED` is explained by the review decision and
  the rollup, else it is branch protection. Methods:
  `Repository.mergeCommitAllowed`, `squashMergeAllowed`, `rebaseMergeAllowed`,
  `viewerDefaultMergeMethod`. Writes: `mergePullRequest` (`mergeMethod`,
  `commitHeadline`, `commitBody`, `expectedHeadOid`: a moved head answers
  "Head branch was modified"), `enablePullRequestAutoMerge` (same input) and
  `disablePullRequestAutoMerge`, `closePullRequest`, `reopenPullRequest`,
  `convertPullRequestToDraft`, `markPullRequestReadyForReview`; each returns
  `pullRequest`. Branch delete: REST `DELETE
  /repos/{o}/{r}/git/refs/heads/{branch}` (204). Auto-merge needs the
  repository setting and a protection rule, and the free private sandbox has
  neither, so it is replay-tested only.
- [G11] People and labels, checked live on the sandbox. Read, in the pull
  request fragment: `reviewRequests{requestedReviewer}` (User, Team with
  `organization{login}`, Bot, Mannequin), `latestReviews`, `assignees`
  (User nodes only, so no Bot fragment), `labels{name color description}`,
  `milestone{title}`. Review state: `latestReviews{state commit{oid}}` per
  author; an approval whose `commit.oid` is not `headRefOid` is `stale` (a
  missing commit is not). A pending `reviewRequests` entry for someone who
  already reviewed is a re-request: the row becomes `Requested` with
  `requested_again`. `reviewDecision` stays APPROVED after a push or a
  re-request (and is null without a required-review rule), so the backend keeps
  its APPROVED only while a current approval remains, and answers
  `StaleApproval` or "needs review" otherwise; CHANGES_REQUESTED and
  REVIEW_REQUIRED pass through. Pickers: `repository.assignableUsers(first: 100, query:)`
  and `repository.labels(first: 100)` paged. Writes are REST by name, so no node
  ids are needed: `POST` / `DELETE .../pulls/{n}/requested_reviewers`
  (`reviewers` logins, `team_reviewers` slugs; the author is refused with 422
  "Review cannot be requested from pull request author."), `POST` / `DELETE
  .../issues/{n}/assignees`, `POST .../issues/{n}/labels` and `DELETE
  .../issues/{n}/labels/{name}`; each is followed by a `PullRequest` read. REST
  label `POST` creates an unknown label, hence the check in `set_labels`.

**GitLab APIs** (draft notes and approve exist only in REST, so the backend is
hybrid)
- [L1] REST: `POST /projects/:id/merge_requests/:iid/draft_notes` (`note`,
  `position{base_sha, head_sha, start_sha, old_path, new_path, position_type,
  old_line, new_line, line_range}`, `in_reply_to_discussion_id`,
  `resolve_discussion`); `GET` lists them (confirmed live, empty). Publish all:
  `POST .../draft_notes/bulk_publish` with `note` and
  `reviewer_state=requested_changes | reviewed` (it falls back to `reviewed` if
  the user cannot request changes). Approve: `POST .../approve` (`sha`);
  `POST .../unapprove`. GraphQL has no draft-note or approve mutation (checked
  all 733 mutations) but has `mergeRequestRequestChanges(projectPath, iid)` and
  `mergeRequestDestroyRequestedChanges`. Read the state with
  `MergeRequest.reviewers.nodes.mergeRequestInteraction.reviewState`
  (UNREVIEWED, REVIEWED, REQUESTED_CHANGES, APPROVED, UNAPPROVED,
  REVIEW_STARTED; confirmed live) or REST `GET .../reviewers`.
- [L2] `createNote(noteableId, discussionId, body)` or REST `POST
  .../discussions/:id/notes`. Inside a queued review: a draft note with
  `in_reply_to_discussion_id`.
- [L3] `discussionToggleResolve(id, resolve)` or REST `PUT
  .../discussions/:id` (`resolved`). Read `discussions.nodes{resolvable,
  resolved, resolvedBy, notes{position{filePath, newLine, oldLine,
  positionType}}}` (confirmed live).
- [L4] A suggestion is the `suggestion` fence in a note; read
  `Note.suggestions{applied, fromLine, toLine, fromContent, toContent}`; apply
  with REST `PUT /suggestions/:id/apply` or `PUT /suggestions/batch_apply`.
  Viewed files: no REST or GraphQL field or mutation (searched the whole
  schema); the UI checkbox has no public API.
- [L5] `MergeRequest.headPipeline{status, stages{nodes{name, jobs{nodes{id,
  name, status, retryable}}}}}`; REST `GET .../merge_requests/:iid/pipelines`,
  `GET /projects/:id/pipelines/:pid/jobs` (confirmed live).
- [L6] Log: REST `GET /projects/:id/jobs/:job_id/trace` (text/plain with ANSI,
  `Accept-Ranges: bytes`; `Range: bytes=-120` returned 206, so tail and poll
  with ranges). GraphQL `CiJob.trace.htmlSummary` is an HTML summary only.
  There is no step list.
  Checked live: while the job runs the trace ignores `Range` and returns the
  whole log on each poll, growing in bursts; a finished job honors it.
- [L7] `jobRetry(id)`, `pipelineRetry`; REST `POST /projects/:id/jobs/:job_id/retry`,
  `POST /projects/:id/pipelines/:id/retry`.
- [L8] `currentUser.reviewRequestedMergeRequests(state: opened)` and
  `authoredMergeRequests` (both confirmed live); REST `GET
  /merge_requests?scope=all&state=opened&reviewer_username=<me>` and
  `scope=created_by_me`.
- [L9] `MergeRequest.stack`: the other merge requests of the stack, null when
  not stacked or when there are more than 20 (it returned `[]` for an unstacked
  one). Keep the base-branch rule as the fallback.
- [L10] Lifecycle, checked live on the sandbox. Read: REST `GET
  /projects/:id/merge_requests/:iid` (`detailed_merge_status`: mergeable,
  checking, unchecked, draft_status, ci_must_pass, ci_still_running,
  not_approved, requested_changes, discussions_not_resolved, need_rebase,
  broken_status, not_open, and rule names the backend files under branch
  protection; `has_conflicts`; `merge_when_pipeline_succeeds`); methods: `GET
  /projects/:id` (`merge_method`, `squash_option`). A merge call has no method
  of its own, so the backend compares the asked one with the project's.
  Write: `PUT .../merge_requests/:iid/merge` (`sha`, `squash`,
  `squash_commit_message`, `merge_commit_message`, `should_remove_source_branch`,
  and `auto_merge` for auto-merge; a wrong `sha` answers 409 "SHA does not
  match HEAD of source branch"), `POST .../cancel_merge_when_pipeline_succeeds`,
  `PUT .../merge_requests/:iid` with `state_event=close|reopen`, GraphQL
  `mergeRequestSetDraft(projectPath, iid, draft)`. The merge runs in the
  background: the backend reads the merge request again until it is not open
  (the settle interval, at most 15 looks). The sandbox project removes the
  source branch after merge by default, and `squash_option` is `default_off`.
- [L11] People and labels, checked live on the sandbox. Read, in `MrFields`:
  `reviewers{... mergeRequestInteraction{reviewState approved approvedAt}}`,
  `approvedBy`, `mergeRequestDiffs(first: 1){nodes{createdAt}}`, `assignees`,
  `labels{title color description}`, `milestone{title}`. Review state (checked
  live on gitlab.com 19.6): `reviewState` maps UNREVIEWED to `Requested`,
  REVIEW_STARTED to `Pending`, REVIEWED to `Commented`, REQUESTED_CHANGES to
  `ChangesRequested`, APPROVED to `Approved` and UNAPPROVED to `Dismissed`.
  GitLab keeps the approval after a push and after "request review again", so
  `approved` and `approvedBy` still hold it. UNREVIEWED on someone with an
  approval on record (`approvedAt`, `approved`, or in `approvedBy`) is
  `requested_again` (the dashed circle in GitLab's reviewer list), and an
  APPROVED reviewer whose `approvedAt` is older than the newest diff version
  (the last push; versions come newest first) is `stale`. UNAPPROVED is the
  person withdrawing their own approval, so it is not a re-request. Approvers
  who are not reviewers have no `approvedAt` in GraphQL (`UserCore` has no
  `mergeRequestInteraction`), so they are never stale. The merge request's
  `approved` flag is no longer read: it means "the approval rules are met",
  which is true with no rule at all. Pickers: REST `GET
  /projects/:id/users?search=&per_page=100` (members, inherited ones included)
  and `GET /projects/:id/labels` (group labels included). Writes: one `PUT
  /projects/:id/merge_requests/:iid` with `reviewer_ids`, `assignee_ids`
  (both replace the list, so the backend reads the current one from REST
  first: GraphQL's reviewer list also holds people who only approved; `[0]`
  clears it, checked live), `add_labels` or `remove_labels` (comma-separated
  names; an unknown name would create a label), then a `PullRequest` read. The
  numeric user id comes from the global id (`gid://gitlab/User/1`), or from
  `GET /users?username=`. GitLab accepts the author as reviewer. There is no
  review request to a group.

**Forgejo APIs** (REST `/api/v1`; OpenAPI at `/swagger.v1.json`, version
`17.0.1+gitea-1.22.0` on the instance these notes were checked against, first
`15.0.9+gitea-1.22.0`; `{run}` in
web routes and in status `target_url`s is `index_in_repo`, not the run id)
- [F1] `POST /repos/{o}/{r}/pulls/{i}/reviews` with `{body, commit_id, event,
  comments: [{path, body, old_position, new_position}]}`; an `event` that is
  empty, `PENDING` or unknown makes a pending review (read in the v15.0.9
  handler source). Add to it: `POST .../reviews/{id}/comments`. Submit: `POST
  .../reviews/{id}` with `{event: APPROVED | REQUEST_CHANGES | COMMENT, body}`;
  `DELETE .../reviews/{id}`. A pending review is visible to its author only.
  Rules: no approve or request changes on your own pull request;
  REQUEST_CHANGES needs a body; COMMENT needs a body or a comment. A comment is
  on one line (`new_position` on the new side, `old_position` on the old side);
  v17 adds `extra_lines_count` (lines after the commented line, 0 = single
  line) to the create and read shapes; confirmed live on 17.0.1: a comment
  with `new_position` 3 and `extra_lines_count` 2 reads back as lines 3 to 5.
  Live write findings (17.0.1): a pending review is made by leaving `event`
  out, but it still needs a non-blank `body` (an explicit `PENDING`, an empty
  event with an empty body and a whitespace body all answer 422 "requires a
  body"), and so does a submit when no comment is sent in that same request,
  even if comments were queued; the backend sends a zero-width space and reads
  it back as no summary. The `APPROVE` event name is not accepted (422 body
  check), `APPROVED` is. Create-comment and submit answer 200 with the object,
  deletes 204. The server groups a conversation by path and first line only, so
  a range and a single-line comment that start on the same line share a web
  thread; the backend also keys on `extra_lines_count`, and a reply repeats it.
  Read: `GET .../reviews` (`state` is APPROVED, PENDING,
  COMMENT, REQUEST_CHANGES or REQUEST_REVIEW; also `stale`, `dismissed`,
  `official`, `comments_count`), then `GET .../reviews/{id}/comments` per
  review (there is no flat list across reviews). A comment reads `path`,
  `position` (new-side line, 0 when on the old side), `original_position`
  (old-side line, 0 when on the new side), `commit_id` (the head it was written
  on; the merge base for an old-side comment), `diff_hunk` (ends at the
  commented line), `extra_lines_count` and `resolver`. There is no outdated
  flag; the backend calls a new-side comment outdated when its commit is not
  the head and the last `diff_hunk` line no longer equals that line of the
  head's file (`GET .../raw/{path}?ref={head}`).
- [F2] `PullReviewComment` has no `in_reply_to`, and the create-comment body has
  no reply field (still true in the v17.0.1 spec). Threads are the comments of one path and line (the web UI
  groups them that way). Web: `POST /{o}/{r}/pulls/{i}/files/reviews/comments`
  with form field `reply` (session). The unreleased v18 development branch has
  `POST .../comments/{id}/replies`.
- [F3] Read `PullReviewComment.resolver` (non-null = resolved). Write, web only
  (session): `POST /{o}/{r}/issues/resolve_conversation` with form
  `action=Resolve | UnResolve`, `comment_id`, `origin`. The unreleased v18
  development branch has API `.../resolve` and `.../unresolve`.
- [F4] No suggestion rendering or apply route in v15.0.9, v16.0.7 or v17.0.1
  (the locale strings and the `apply_suggestion` route exist only on the
  development branch). Parse the fence in the app and apply locally.
- [F5] Web only (session): `POST /{o}/{r}/pulls/{i}/viewed-files` with JSON
  `{files: {path: bool}, headCommitSHA}`; the API does not expose it. Keep it
  local.
- [F6] `GET /repos/{o}/{r}/commits/{ref}/status` (combined `state`) and
  `/statuses`. Actions write one status per job: context `workflow / job
  (event)`, `target_url` `/{o}/{r}/actions/runs/{run}/jobs/{jobIndex}`
  (confirmed live). Runs: `GET /repos/{o}/{r}/actions/runs?head_sha=&ref=&status=&event=&workflow_id=`
  (`{total_count, workflow_runs[]}`, each with `id`, `index_in_repo`,
  `status`, `workflow_id`, `html_url`, `started`, `stopped`; confirmed live,
  empty on the sandbox) and `.../actions/runs/{run_id}`. v17 statuses also
  allow `skipped`.
- [F7] Checked live: logs tail well by job id (`jobs/1165/logs`, not the task
  id) with `Range` (206, and 416 past the end); before the first line exists
  the server answers 500 "file does not exist", read as an empty chunk.
  v16 added, and v17.0.1 has: `GET /repos/{o}/{r}/actions/runs/{run_id}/jobs`
  (a plain array of `{id, name, status, run_id, html_url, task_id, needs,
  runs_on}`; the job at index n of the run, sorted by id, is the status
  `target_url`'s `jobs/{n}`), `GET .../actions/jobs/{job_id}` (the job plus
  `steps[{number, name, status, started, stopped}]`, with "Set up job" as
  number 0 and "Complete job" last; steps only come from this call, one request
  per job), `GET .../actions/jobs/{job_id}/logs` (plain text, 200 or 206;
  `attempt`, `step`, `q` and `format` filters) and `GET .../runs/{run_id}/logs`.
  `GET .../actions/tasks` (one row per job: `name`, `run_number`, `status`,
  `head_sha`) still exists. On 15.x none of the job, step or log endpoints
  exist (404); the statuses in [F6] are the job list there. The job id in these
  routes is `ActionRunJob.id`, not the task id. Web only (session) and not
  used: `POST /{o}/{r}/actions/runs/{run}/jobs/{job}`.
- [F8] v17.0.1: `POST /repos/{o}/{r}/actions/jobs/{job_id}/rerun` (204; "a
  completed workflow job and its dependent jobs"), `POST
  .../actions/runs/{run_id}/rerun` and `POST .../actions/runs/{run_id}/cancel`.
  Before v17 only the web route `POST /{o}/{r}/actions/runs/{run}/rerun`
  (session) exists. Same paths in the v17.0.1 spec; not tried
  live (the sandbox has no Actions runs), covered by an inline replay test.
- [F9] `GET /repos/issues/search?type=pulls&state=open&review_requested=true`
  (team requests included), `&created=true`, `&reviewed=true` (`created=true` returns the
  sandbox's six open pull requests, `review_requested=true` is empty; add
  `sort=recentupdate`, `page`, `limit` and `X-Total-Count` for paging). A row
  is an issue: no `base` or `head`, only `pull_request{merged, draft,
  html_url}` and `repository.full_name`, so the backend fetches each pull
  request. Per
  pull request: `requested_reviewers` on `GET .../pulls/{i}`. Stacks: none;
  match base and head refs from `GET /repos/{o}/{r}/pulls` (or look one up with
  `GET .../pulls/{base}/{head}`).
- [F10] Lifecycle, checked live on 17.0.1. Read: `GET .../pulls/{i}`
  (`mergeable`, `draft`, `merged`, `state`; a draft reads `mergeable: false`),
  `GET .../reviews`, `GET .../branch_protections/{base}` (404 when there is no
  rule, or the viewer cannot administer the repository; read as none) for
  `enable_status_check`, `required_approvals`, `block_on_rejected_reviews`; the
  head's combined status for checks. Methods: `GET /repos/{o}/{r}`
  (`allow_merge_commits`, `allow_squash_merge`, `allow_rebase`,
  `default_merge_style`). Writes: `POST .../pulls/{i}/merge` with `{Do: merge |
  squash | rebase, MergeTitleField, MergeMessageField, head_commit_id,
  delete_branch_after_merge}` (a wrong `head_commit_id` answers 409 "head out of
  date"), `PATCH .../pulls/{i}` with `state: closed | open`, or with `title`
  for the draft state: the server reads a leading `WIP:` or `[WIP]` (any case)
  as a draft, and the backend adds `WIP: ` or strips those. The API also has
  `merge_when_checks_succeed` and `DELETE .../pulls/{i}/merge` for auto-merge;
  not used yet.
- [F11] People and labels, checked live on 17.0.1. Read from `GET .../pulls/{i}`:
  `requested_reviewers`, `requested_reviewers_teams`, `assignees`, `labels`
  (`id`, `name`, `color`, `description`), `milestone`. Pickers: `GET
  /repos/{o}/{r}/assignees` (who can be assigned; it has no search, so the
  prefix is matched in the backend, over at most 500 rows) and `GET
  /repos/{o}/{r}/labels`. Writes: `POST` / `DELETE .../pulls/{i}/requested_reviewers`
  (`reviewers`, `team_reviewers`; the poster is refused with 422 "poster of pr
  can't be reviewer"), `PATCH .../issues/{i}` with the whole `assignees` list
  (there is no add or remove call), `POST .../issues/{i}/labels` with names and
  `DELETE .../issues/{i}/labels/{id}` (the id from the label list); an unknown
  name is refused by the backend first. Team reviewers need an organization
  team, which the personal sandbox lacks, so they are replay-tested only.
  Review state: a review request is a row of `GET .../reviews` of its own
  (`REQUEST_REVIEW`), so the latest row per person (highest id) is the state.
  A request newer than an approval makes the person `Requested` with
  `requested_again`, and so does a `requested_reviewers` entry for someone
  whose latest row is a submitted review. `stale` on an APPROVED row is the
  forge's own `stale` flag (the head moved since); a `dismissed` row is
  `Dismissed`.

## Git layer

- **Object stores**: per repo, either an app-owned bare clone (under the app's
  data dir) or an existing clone registered in config. Both are read the same
  way through `gix`.
- **Opening a pull request** fetches its head ref (`refs/pull/N/head`,
  `refs/merge-requests/N/head`) with one `git fetch`, then diffs
  merge-base..head straight from objects. No files are written.
- **Worktree on demand** ("open in editor", "terminal here", hand to an
  agent): anything that starts an agent uses the app-owned bare store under
  the app's data dir (`<name>.worktrees/pr-N` beside `<name>.git`), even when
  a registered clone exists, so the checkout never inherits the trust of a
  folder the user opened in an agent. Only a plain worktree the user asks for
  may go in a registered clone, at `.claude/worktrees/pr-N`.
- **Cold start** for a repo with no local objects is the one slow path: the
  app creates an empty bare store and fetches the pull request's head and its
  base branch in full (no shallow fetch), so merge bases and blobs are local
  from then on.

## Diff engine and view

- **Model**: per file, the old and new blob ids, a line-start offset table for
  each blob, and hunks as line ranges. No line is copied into a string until
  it is painted.
- **Rows** are a projection of the model: file header, hunk header / context
  expander, code line (unified) or line pair (split), and blocks (thread,
  composer). Switching unified / split re-projects; nothing is recomputed.
- **One continuous list** across all files, virtualized. Code rows share one
  height; blocks are measured. A `sum_tree` of row heights maps scroll offset
  to row in O(log n), so a 100k-row diff with threads scrolls like a short one.
- **Painting** is a custom gpui element that shapes and paints only visible
  rows. Text appears plain first and is recolored when highlighting for that
  blob lands (monospace, so nothing shifts).
- **Context expansion** is instant because whole blobs are local. Arrows in
  a hunk header's gutter reveal 20 hidden lines at a time (a gap of 20 or
  fewer at once, which joins the hunks), an expander row does the same after
  a file's last hunk, and only that file's rows are re-projected and spliced
  into the list and its height index.
- Lockfiles, generated files and very large diffs start collapsed.

## CI and notifications

- Sidebar shows the rollup per pull request; the detail pane shows run, job
  and step state, and a virtualized log viewer with ANSI colors that tails a
  running job.
- **Subscriptions** poll with an adaptive interval: fast while a run is
  active, slow when idle, using conditional requests.
- A row or stack member showing queued or running CI is read again every
  minute until it settles, watched or not, in one batched request per
  repository where the forge allows (`ci_settle.rs`, `Forge::ci_states`).
  Settled rows are only read with the inbox.
- A state change raises an OS toast and an in-app badge. The app has no tray
  and does nothing once its window is closed. Webhook or Discord channels and
  a background service are not built (see `ROADMAP.md`).

## Terminal and machines

- `alacritty_terminal` does emulation and the PTY (ConPTY on Windows); a custom
  gpui element paints the cell grid.
- **SSH** runs the system `ssh` binary inside a local PTY, so it uses the
  existing SSH config, agent and VPN. No SSH library.
- **Machines** are this PC and every `Host` of `~/.ssh/config`; `[[machines]]`
  in config adds, per host, where repos live there (`repos_root`, `clones`),
  the command to start (default `claude`) and the shell.
- "Open on *machine*" from a pull request: connect, make sure the worktree
  exists there, start the command in it. Sessions end with the app.
- **Launch policy** (`agent.rs`, shared by "Open on machine" and "Review with
  Claude"). A pull request's content is untrusted: anyone can open one.
  - *Where*: the worktree is in a repository the app owns, here beside the
    bare store under the data dir, over SSH in the scratch clone under
    `~/.cache/review`; never in the user's clone (`Placement::AppOwned`).
  - *How*: Claude starts with `--setting-sources user --strict-mcp-config`.
    Checked against Claude Code 2.1.326 (here and on a remote machine): with them the
    checkout's `.claude/settings*.json` (hooks, plugins, env), `CLAUDE.md`,
    `.claude/{skills,agents,commands}` and `.mcp.json` are not loaded. The
    user's own settings and hooks still are. Only Claude starts by itself.
    Any other program (gemini, codex, aider, a script) has no known safe
    launch, so it never runs on its own: the card below always shows, its
    default opens a plain shell in the worktree, and the program runs only
    after a confirmation naming it and the worktree.
  - *What changed*: before starting, the worktree's commit is diffed against
    its merge base (in this PC's copy, also for a remote worktree) for agent
    config: `.claude/`, `.cursor/`, `.gemini/`, `.codex/`, `.continue/`,
    `.mcp.json`, `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`, `GEMINI.md`
    and names starting `.aider`, `.windsurf` or `.env`, at any depth;
    `.github/copilot-instructions.md`, `.vscode/settings.json`; case
    ignored. A change (or a comparison that failed) opens a card beside the
    diff listing the files with "View diff", where the worktree lives, and
    two choices: "Start without the PR's agent config" (the default) or
    "Trust this PR's config", which starts the plain command only after a
    second confirmation naming the files, the program and the worktree.
  - *Which program*: every program a terminal starts (shell, `ssh`, the
    command) is resolved to an absolute path from the PATH's absolute
    directories (`term::program`), never the working directory; Windows
    children also get `NoDefaultCurrentDirectoryInExePath=1`, and `cmd`
    receives the quoted absolute path. Over SSH, `/bin/sh` runs the command
    only if the remote PATH has no empty or relative entry and
    `command -v` gives an absolute path. The lookup runs off the UI thread;
    the terminal shows "Starting..." until it is done.
  - *Review with agent* (`claude_review/`, `workspaces.rs`): the agent runs
    interactively in a terminal tab. Each repository has a workspace
    (`[[workspaces]]` in `config.toml`, edited on the settings page): the
    machine, the repository's directory (a clone the worktree is added off
    at `.claude/worktrees/pr-N`, or an empty one cloned into first), an
    optional working directory, the command and a prompt template. The pull
    request's title, description, diff and threads go in
    `<worktree>/.review/context.md` (excluded from git; over SSH written
    through `ssh`'s stdin), never on the command line: the prompt's
    placeholders are only the forge's names for the pull request and paths
    from the user's config. Claude starts with `--mcp-config` naming a file
    (never the JSON itself, which any user could read in the process list):
    in the app's data folder under the user's profile (`0600` on Unix), or
    over SSH `~/.cache/review/mcp/` written under `umask 077` through
    stdin; named by a hash of the token, removed when the terminal ends and
    swept at startup. It configures a local MCP server (127.0.0.1 only,
    a 128-bit token per launch in the URL,
    compared in constant time, capped bodies and comments, revoked when the
    terminal ends; over SSH reached through `ssh -R`) whose one tool,
    `add_review_comment`, is allowed without a prompt and turns a finding on
    a line the diff shows into a draft marked "from Claude". Every launch
    passes `--disallowedTools`: WebFetch and WebSearch; Bash, PowerShell,
    Edit, Write and NotebookEdit unless the workspace sets `allow_commands`;
    and Read, Grep and Glob on the platform's credential paths (`~/.ssh`,
    `~/.config`, `~/.aws`, ..., `~/AppData/Roaming` and `~/AppData/Local` on
    Windows, `~/Library/Keychains` and `~/Library/Application Support` on
    macOS, `/etc`, and the app's own folders), a second layer behind Claude
    Code asking before a file tool reads outside its working directories;
    deny wins over the allow rules of the user's project settings. Over SSH
    the machine's `~/.cache/review` (scratch clones, MCP configs) is denied
    too. No rule is ever dropped: a worktree or working directory inside a
    denied tree refuses the launch. The deny rules bind the file tools only:
    `allow_commands` gives the agent the user's full file and network
    access (a shell reads anything) while the pull request's text is in its
    context. With a working
    directory Claude starts there (the user's project settings, MCP servers
    and skills apply) with the worktree as `--add-dir` and
    `CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD` cleared; Claude loads an
    `--add-dir` folder's skills, so that layout is used only when the pull
    request changes no agent config, and a worktree inside the working
    directory is refused. Otherwise it starts in the worktree with the safe
    flags. A checkout with symbolic links leading outside it is refused.
    Another agent gets "Open on machine"'s confirm-first launch. Drafts are
    only drafts: nothing is sent until the user queues and submits them.
- **Not built**: a small daemon per machine that owns the PTYs so sessions
  outlive the app, reports agent status from Claude Code hooks, and runs
  rules such as "CI failed, start an agent" (see `ROADMAP.md`).

## Config

`<config dir>/review/config.toml` holds the forges (kind, host, inbox
sections, clone URL), repositories with their own inbox section and optional
registered clone, machines, and the "Review with agent" workspaces. The app
writes it on first run from the forge CLIs that are logged in, with comments
explaining each field (`crates/app/src/config.rs`). `settings.toml` beside it
holds the theme, mode, fonts and notification switches, and is written by the
settings panel. Unsent comment drafts live in the SQLite cache, written on
every edit, so a crash loses nothing.
