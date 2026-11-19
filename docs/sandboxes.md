# Sandboxes

Three private repositories, named `review-sandbox`, are the only place live
tests may write to: one on GitHub, one on GitLab and one on a Forgejo
instance. They belong to whoever runs the live checks, so nothing in this
repository names them: the tools read them from the environment.

## Configuration

| Variable | Value | Used by |
|---|---|---|
| `REVIEW_SANDBOX_GITHUB` | `owner/review-sandbox` on github.com | live examples, `seed_sandbox.py`, recording |
| `REVIEW_SANDBOX_GITLAB` | `owner/review-sandbox` on gitlab.com | same |
| `REVIEW_SANDBOX_FORGEJO` | `host/owner/review-sandbox` | same |
| `REVIEW_SANDBOX_FORGEJO_REMOTE` | git remote URL, when it is not `git@host:owner/review-sandbox.git` (a non-default SSH port, say) | same |
| `REVIEW_REMOTE_HOST` | an SSH host name for the remote worktree check | `worktree_check` |
| `REVIEW_RECORD_REWRITES` | `real=>placeholder;...` pairs (display names, account ids) | recording |
| `REVIEW_RECORD` | `1` to record fixtures instead of replaying | conformance tests, live examples |

Only repositories named `review-sandbox` are accepted; any other name is
refused, and an unset variable stops the tool with a message naming it.
Keep the values in `.scratch/sandbox.env` (`KEY=VALUE` lines; `.scratch/` is
git-ignored). `just live` reads that file for any variable not already set in
the environment; for the other tools, load it into the shell first.

`just live` (`python scripts/checks.py live [--only NAME]`) runs the live
checks: `live_smoke`, `inbox_check_*` and `round_trip` and `ci_live` per forge
(`cargo run -p forge --example ...`), then `notify_bench` and `worktree_check`
(`cargo run -p app --example ...`). Each check's output goes to
`.scratch/check-logs/`. The other live tools run by hand:
`cargo run -p forge --example lifecycle` and `people`,
`cargo run -p app --example suggestion_live`, and
`python scripts/seed_sandbox.py`. None of them runs in `cargo test`.

### Recording fixtures

The committed fixtures under `crates/forge/tests/fixtures` describe fictional
accounts: GitHub and GitLab user `Lovelace`, Forgejo host
`forgejo.example.com` with user `cora`, all with the repository
`review-sandbox`. With `REVIEW_RECORD=1` the recorder talks to the real
sandboxes named by the variables above and writes the placeholders: the
owner and host are rewritten in requests and responses, hashes in avatar
URLs are zeroed, emails and tokens are scrubbed as before. A conformance
test therefore runs unchanged against the real sandbox and against the
recording. Things the recorder cannot know, such as your display name or
numeric account ids, go in `REVIEW_RECORD_REWRITES`, for example
`Real Name=>Ada Lovelace;1234=>1234567`; check a fresh recording with
`git grep` for your own names before committing it.

## Repositories

| Forge | Page | Git remote |
|---|---|---|
| GitHub | `https://github.com/<owner>/review-sandbox` | `git@github.com:<owner>/review-sandbox.git` |
| GitLab | `https://gitlab.com/<owner>/review-sandbox` | `git@gitlab.com:<owner>/review-sandbox.git` |
| Forgejo | `https://<host>/<owner>/review-sandbox` | `git@<host>:<owner>/review-sandbox.git` (or `REVIEW_SANDBOX_FORGEJO_REMOTE`) |

Each repository has one CI workflow on `main` (see [CI](#ci)).

## Contents

`main` has 34 files plus the CI workflow file: Rust and Markdown, a nested directory
(`src/deep/nested/dir/`), a CRLF file (`data/crlf.txt`), a file without a
trailing newline (`data/no_newline.txt`), a PNG (`assets/logo.png`) and a
shell script (`scripts/run.sh`).

Every forge has the same numbers (GitLab calls them `!n`), created in this
order:

| # | Head branch | Base | State | What it is |
|---|---|---|---|---|
| 1 | `stack-1` | `main` | open | stack 1/3 (`lib.rs`, `util/math.rs`) |
| 2 | `stack-2` | `stack-1` | open | stack 2/3 (`util/strings.rs`, `docs/guide.md`) |
| 3 | `stack-3` | `stack-2` | open | stack 3/3 (`main.rs`, `README.md`) |
| 4 | `threads` | `main` | open | review threads, all on `src/threads_target.rs` (below) |
| 5 | `big` | `main` | open | 27 files: rename (`docs/old_name.md` to `new_name.md`), deletion (`docs/obsolete.md`), mode change (`scripts/run.sh` to 755), PNG change, CRLF edit, no-newline edit |
| 6 | `feature-merged` | `main` | merged (merge commit) | adds `docs/merged-note.md` |
| 7 | `feature-closed` | `main` | closed, unmerged | adds `docs/closed-note.md` |
| 8 | `draft-wip` | `main` | draft (`WIP:` title prefix on Forgejo) | adds `src/draft.rs` |

Threads on #4 (commit 1 is `threads-initial`, commit 2 changes `is_even`):

| Thread | Line | GitHub | GitLab | Forgejo |
|---|---|---|---|---|
| resolved | 8 (`saturating_sub`) | resolved | resolved | skipped, unresolved |
| unresolved + reply | 16 (`checked_div`) | 2 comments | 2 notes | 2 comments on the same line |
| multi-line | 29 to 31 (`for` loop in `pow`) | lines 29-31 | lines 29-31 | skipped |
| removed line | old line 8 (`a - b`) | LEFT side | old line | old position |
| outdated | 24 (`is_even`) | `isOutdated` true | system note "changed this line in version 2" | see below |

## Skipped

- **Forgejo, resolved thread**: resolving is a web route that needs a login
  session (`design.md` [F3]). The comment exists, unresolved.
- **Forgejo, multi-line comment**: skipped when seeded on 15.0.9, which had one
  position only. Since 17.0.1, `extra_lines_count` gives a range [F1]; re-seeding
  can add it.
- **Forgejo, reply**: there is no reply API [F2]. The reply is a second review
  comment on the same path and line, which the web UI groups as a thread.
- **Forgejo, outdated thread**: the comment is posted on the line that commit 2
  changes, but the 15.0.9 API keeps `position` at 24 and exposes no outdated
  flag. Not verifiable through the API; check the web page if it matters.

## Re-running

```sh
python scripts/seed_sandbox.py                 # all three (needs all three variables)
python scripts/seed_sandbox.py --forge gitlab  # one
```

`python scripts/seed_sandbox.py --ci` only adds or updates the CI workflow
(see [CI](#ci)).

Needs `git`, `gh`, `glab` and `fj` logged in. The script rebuilds the source in
`.scratch/sandbox-src` with fixed identity and dates, so commit ids are
identical on every run, and adds only what is missing: it never force-pushes,
never rewrites a branch, and prints `N changes` per forge. A repeat run prints
`0 changes`. The Forgejo token is read from `fj`'s key file and only sent in a
request header.

To start over, delete the PRs' branches and the repository by hand, then run
the script again. It does not delete anything.

## Listing (after the second run, 0 changes)

GitHub (`gh pr list -R <owner>/review-sandbox --state all`):

```
8  Start a feature                             draft-wip       DRAFT
7  Closed: add a note, never merged            feature-closed  CLOSED
6  Merged: add a note                          feature-merged  MERGED
5  Big: modules, rename, deletion, mode change big             OPEN
4  Threads: review comments on several lines   threads         OPEN
3  Stack 3/3: log to stderr, link guide        stack-3         OPEN
2  Stack 2/3: add whisper, expand guide        stack-2         OPEN
1  Stack 1/3: bump version, add triple         stack-1         OPEN
```

GitLab (`glab mr list -R <owner>/review-sandbox --all`):

```
!8  Draft: Start a feature                      (main) <- (draft-wip)
!7  Closed: add a note, never merged            (main) <- (feature-closed)
!6  Merged: add a note                          (main) <- (feature-merged)
!5  Big: modules, rename, deletion, mode change (main) <- (big)
!4  Threads: review comments on several lines   (main) <- (threads)
!3  Stack 3/3: log to stderr, link guide        (stack-2) <- (stack-3)
!2  Stack 2/3: add whisper, expand guide        (stack-1) <- (stack-2)
!1  Stack 1/3: bump version, add triple         (main) <- (stack-1)
```

Forgejo (`fj -H <host> pr search -r <owner>/review-sandbox -s all`;
#6 is merged, the API shows `merged: true`):

```
#8: WIP: Start a feature
#7: Closed: add a note, never merged
#6: Merged: add a note
#5: Big: modules, rename, deletion, mode change
#4: Threads: review comments on several lines
#3: Stack 3/3: log to stderr, link guide
#2: Stack 2/3: add whisper, expand guide
#1: Stack 1/3: bump version, add triple
```

## CI

One workflow per forge, the same three jobs, run on push and on pull request
events (never on a schedule). The job bodies are POSIX `sh`, shared through
`CI_*` constants in `scripts/seed_sandbox.py`.

| Forge | File | Runner |
|---|---|---|
| GitHub | `.github/workflows/ci.yml` | `runs-on: ubuntu-latest` (GitHub-hosted) |
| GitLab | `.gitlab-ci.yml` (stages `build`, `test`, `slow`; every job has `needs: []`, so all three start at once and a failing `test` does not hide `slow`; image `alpine:3.20`) | GitLab.com shared runner `saas-linux-small-amd64` |
| Forgejo | `.forgejo/workflows/ci.yml` | `runs-on: docker`, a label of the instance runner (labels `docker`, `rust`) |

- `build`: three steps (`Configure`, `Compile`, `Package`), each an `echo`; passes.
- `test`: fails (`exit 1`) while `ci/fail` exists in the checkout, passes
  otherwise. GitHub and Forgejo check out with `actions/checkout@v4`; GitLab
  clones by itself. `ci/fail` is not on `main`.
- `slow`: prints 2,000 lines `line 0001 ...` with a one-second pause every 33
  lines (about 60 s). Lines ending in 0 are red, in 5 green, in 7 bold; the
  rest are plain: 200 lines of each color
  (counted in the GitHub and GitLab logs).

`--ci` pushes one commit on top of the remote `main`, fast-forward only,
with the fixed sandbox identity and dates, and does nothing when the file on
`main` already equals the script's text (`0 changes`). Change the `CI_*`
constants and run `--ci` again to update the workflow; the other branches
and pull requests are not touched, so the existing PR branches (cut from the
old `main`) have no workflow file and start no runs. Branches cut from the
current `main` do.

### Verified runs

All three forges run the workflow; no runner problem. Two runs per forge: a
push to `main` (all pass) and a push of a scratch branch `ci-verify` that adds
`ci/fail` (`test` fails, the other two pass). The scratch branches were deleted
afterwards; the runs remain. Times are job durations as the forge reports them.

GitHub, run `44098589245` (`main`, success, 67 s wall clock):

| Job | Id | Result | Time | Steps (number, name) |
|---|---|---|---|---|
| `build` | `134092191101` | success | 4 s | 1 Set up job, 2 Configure, 3 Compile, 4 Package, 5 Complete job |
| `test` | `134092191793` | success | 6 s | 1 Set up job, 2 Check out, 3 Run tests, 6 Post Check out, 7 Complete job |
| `slow` | `134092190477` | success | 64 s | 1 Set up job, 2 Print 2000 lines, 3 Complete job |

Failing run `44098654166` (`ci-verify`): `test` is job `134092412359`
(failure at step 3 `Run tests`, 4 s), `build` `134092412802` (6 s),
`slow` `134092412707` (64 s).

GitLab, pipeline id `3079272264` (iid 1, `main`, success, 72 s). GitLab has no
steps: each job is one script and one log (`.../jobs/<id>/trace`), and the
three `build` commands are lines of that log.

| Job | Id | Result | Time |
|---|---|---|---|
| `build` | `18107982308` | success | 15.7 s |
| `test` | `18107982309` | success | 13.2 s |
| `slow` | `18107982310` | success | 72.7 s |

Failing pipeline `3079273161` (iid 2, `ci-verify`): `test` is job `18107989637`
(`failed`, `script_failure`, 16.1 s), `build` `18107989635`, `slow` `18107989638`
(75 s).

Forgejo 17.0.1, run id `196` (index 1, web `.../actions/runs/1`, `main`,
success, 67 s). The API lists jobs and tasks, not steps; steps appear only in
the web UI (`.../actions/runs/1/jobs/<n>`, `n` = 0, 1, 2).

| Job | Job id | Task id | Result | Time |
|---|---|---|---|---|
| `build` | `1161` | `63` | success | 2 s |
| `test` | `1162` | `64` | success | 3 s |
| `slow` | `1163` | `65` | success | 61 s |

Failing run `197` (index 2, `ci-verify`): `build` job `1164` (task 66),
`test` job `1165` (task 67, failure, 3 s), `slow` job `1166` (task 68, 61 s).
Forgejo ran the jobs one after another (`build`, `test`, `slow`), so the run
takes about the sum.

Where to look: GitHub `gh api repos/<owner>/review-sandbox/actions/runs/<run>/jobs`;
GitLab `glab api projects/<owner>%2Freview-sandbox/pipelines/<id>/jobs`
(run it inside a repository checkout: outside one, `glab api` with a query
string panics); Forgejo `.../api/v1/repos/<owner>/review-sandbox/actions/runs`,
`.../actions/runs/<id>/jobs` and `.../actions/tasks`.

Runner facts, from the APIs: Forgejo reports one instance
runner (idle, v13.2.0, labels `docker` and `rust`), and no
repository-level runner; a workflow whose `runs-on` matches no label would
stay queued. GitLab.com ran the job on shared runner
`saas-linux-small-amd64` without account verification. The Forgejo runner
resolves `actions/checkout@v4` without extra setup.

Notes:
- Forgejo's log API takes the job id (for example 1165), not the task id
  (`jobs/67/logs` is a 404).
- The workflow's `push` trigger is limited to `main`, so a PR's
  commits run CI once through `pull_request` instead of twice. GitHub Actions
  minutes on this private repo count against the account's quota.

## Numbers drift after round trips

`cargo run -p forge --example round_trip` opens and closes a scratch PR on
each run, so each sandbox gains closed PRs beyond #8 (#9, #10, ...) and the
numbers stop matching across forges. PRs #1-#8 keep their
meaning. Before re-recording the `inbox` fixtures, filter closed PRs to #6 and
#7, or the conformance assertion `closed == [6, 7]` breaks.

## Lifecycle runs change main

`cargo run -p forge --example lifecycle -- --forge <f>` squash-merges a scratch
PR into each sandbox's `main`, adding one file `scratch/lifecycle-<stamp>-merge.md`
per run, and opens, closes and reopens a second scratch PR. It never touches
the CI workflow or the seeded PR branches, and deletes only its own
`lifecycle/*` branches. The GitLab sandbox removes source branches after merge
by default and has `squash_option=default_off`.
