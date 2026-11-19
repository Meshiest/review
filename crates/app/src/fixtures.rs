//! Invented inbox data for the shell, behind `--fixtures`, so screenshots
//! and benchmarks run offline. Every name here is made up.

use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::{Duration, Utc};
use diff::synth::{SynthConfig, synthesize};
use diff::{Layout, Segment};
use forge::model::{
    Anchor, CheckStep, CiJob, CiRun, Comment, CommentId, JobId, LineRef, Position, Resolution,
    ReviewId, ReviewState as Verdict, RunId, Sha, Side, Thread, ThreadId, Timestamp, User,
};
use gpui::SharedString;

use crate::avatars;
use crate::commits::PrCommit;
use crate::model::{
    ChangedFile, Check, CiState, FileStatus, ForgeKind, Inbox, PrKey, PrNumber, PullRequest,
    RepoRef, ReviewState, Section, SectionState, Stack, StackMember, Viewed,
};
use crate::session::{ForgeState, ForgeStatus};

const ME: &str = "cora";

const API: usize = 0;
const WEB: usize = 1;
const REVIEW: usize = 2;

fn repo_of(ix: usize) -> (ForgeKind, RepoRef) {
    match ix {
        API => (ForgeKind::GitHub, RepoRef::new("github.com", "acme", "api")),
        WEB => (ForgeKind::GitLab, RepoRef::new("gitlab.com", "acme", "web")),
        _ => (
            ForgeKind::Forgejo,
            RepoRef::new("forgejo.example.com", "cora", "review"),
        ),
    }
}

/// The three fixture forges, signed in as the fixture user.
pub fn forge_statuses() -> Vec<ForgeStatus> {
    [API, WEB, REVIEW]
        .map(|ix| {
            let (kind, repo) = repo_of(ix);
            ForgeStatus {
                host: repo.host.into(),
                kind,
                state: ForgeState::Ok,
                account: Some(user(ME)),
            }
        })
        .into()
}

fn s(text: &str) -> SharedString {
    text.to_owned().into()
}

/// The built-in avatar of a fixture person.
fn avatar_of(login: &str) -> SharedString {
    avatars::bundled_url(login).into()
}

/// A fixture user with a built-in avatar.
fn user(login: &str) -> User {
    User {
        avatar_url: Some(avatars::bundled_url(login)),
        ..User::new(login)
    }
}

fn file(path: &str, status: FileStatus, added: u32, removed: u32) -> ChangedFile {
    ChangedFile {
        path: s(path),
        blob: SharedString::default(),
        status,
        added,
        removed,
    }
}

fn checks_for(state: CiState) -> Vec<Check> {
    let job = |name: &str, state: CiState, detail: &str| Check {
        name: s(name),
        state,
        detail: s(detail),
    };
    use CiState::{Canceled, Fail, Pass, Queued, Running, Skipped};
    match state {
        CiState::None => Vec::new(),
        Pass => vec![
            job("lint", Pass, "42s"),
            job("build", Pass, "3m 10s"),
            job("test (linux)", Pass, "4m 51s"),
            job("test (windows)", Pass, "6m 02s"),
        ],
        Fail => vec![
            job("lint", Pass, "39s"),
            job("build", Pass, "3m 22s"),
            job("test (linux)", Fail, "failed after 2m 14s"),
            job("test (windows)", Fail, "failed after 2m 40s"),
            job("e2e (chromium)", Running, "running, 1m 31s"),
            job("e2e (firefox)", Canceled, "canceled"),
            job("package", Skipped, "skipped"),
            job("deploy preview", Queued, "queued"),
        ],
        Running => vec![
            job("lint", Pass, "41s"),
            job("build", Running, "running, 1m 08s"),
            job("test (linux)", Queued, "queued"),
            job("test (windows)", Queued, "queued"),
        ],
        Queued => vec![
            job("lint", Queued, "queued"),
            job("build", Queued, "queued"),
        ],
        Canceled => vec![
            job("lint", Pass, "40s"),
            job("build", Canceled, "canceled after 1m 02s"),
            job("test (linux)", Skipped, "skipped"),
        ],
        Skipped => vec![job("build", Skipped, "skipped")],
    }
}

/// A stable fake blob id for a file in a pull request.
fn blob_id(number: u32, path: &str) -> SharedString {
    let hash = path
        .bytes()
        .fold(u64::from(number) ^ 0xCBF2_9CE4_8422_2325, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3)
        });
    format!("{:07x}", hash & 0x0FFF_FFFF).into()
}

struct Spec {
    repo: usize,
    number: u32,
    title: &'static str,
    author: &'static str,
    reviewers: &'static [&'static str],
    head: &'static str,
    base: &'static str,
    ci: CiState,
    review: ReviewState,
    minutes_ago: u32,
    description: &'static str,
    files: Vec<ChangedFile>,
}

/// What opening a fixture pull request shows.
#[derive(Clone, Debug, Default)]
pub struct Detail {
    pub files: Vec<ChangedFile>,
    pub checks: Vec<Check>,
    pub runs: Vec<CiRun>,
}

fn repo_of_host(host: &str) -> ForgeKind {
    [API, WEB, REVIEW]
        .map(repo_of)
        .into_iter()
        .find(|(_, repo)| repo.host == host)
        .map_or(ForgeKind::GitHub, |(kind, _)| kind)
}

/// The steps of a fixture job: GitHub and Forgejo have them, GitLab not.
fn fixture_steps(job: &str, state: CiState, started: Timestamp, seconds: i64) -> Vec<CheckStep> {
    let middle = if job.starts_with("test") || job.starts_with("e2e") {
        ["Install toolchain", "Build", "Run tests"]
    } else {
        ["Configure", "Compile", "Package"]
    };
    let names = ["Set up job", "Check out"]
        .into_iter()
        .chain(middle)
        .chain(["Complete job"]);
    let count = 6i64;
    let each = (seconds / count).max(1);
    names
        .enumerate()
        .map(|(ix, name)| {
            let ix = ix as i64;
            let start = started + Duration::seconds(ix * each);
            let step_state = match state {
                CiState::Fail if ix == 4 => CiState::Fail,
                CiState::Fail if ix == 5 => CiState::Skipped,
                CiState::Running if ix == 4 => CiState::Running,
                CiState::Running if ix == 5 => CiState::Queued,
                other if other.is_active() => CiState::Queued,
                _ => CiState::Pass,
            };
            CheckStep {
                number: (ix + 1) as u32,
                name: name.to_owned(),
                state: step_state,
                started_at: (!matches!(step_state, CiState::Queued | CiState::Skipped))
                    .then_some(start),
                completed_at: (!step_state.is_active() && step_state != CiState::Skipped)
                    .then(|| start + Duration::seconds(each)),
            }
        })
        .collect()
}

/// One workflow run per fixture pull request, its jobs from the checks.
fn fixture_runs(number: u32, repo: &RepoRef, checks: &[Check]) -> Vec<CiRun> {
    if checks.is_empty() {
        return Vec::new();
    }
    let forge = repo_of_host(&repo.host);
    let run_number = u64::from(number) * 3 + 100;
    let run_url = format!(
        "https://{}/{}/actions/runs/{run_number}",
        repo.host,
        repo.path()
    );
    let now = Utc::now();
    let jobs: Vec<CiJob> = checks
        .iter()
        .enumerate()
        .map(|(ix, check)| {
            let seconds = 40 + 37 * ix as i64;
            let started = now - Duration::seconds(seconds + 20);
            let ran = !matches!(check.state, CiState::Queued | CiState::Skipped);
            let finished = ran && !check.state.is_active();
            let stage = (forge == ForgeKind::GitLab).then(|| {
                check
                    .name
                    .split([' ', '('])
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            });
            CiJob {
                id: JobId::new(format!("fixture-{number}-{ix}")),
                name: check.name.to_string(),
                stage,
                state: check.state,
                attempt: Some(if check.state == CiState::Fail { 2 } else { 1 }),
                started_at: ran.then_some(started),
                completed_at: finished.then(|| started + Duration::seconds(seconds)),
                url: Some(format!("{run_url}/jobs/{ix}")),
                steps: if forge == ForgeKind::GitLab || !ran {
                    Vec::new()
                } else {
                    fixture_steps(&check.name, check.state, started, seconds)
                },
            }
        })
        .collect();
    let state = CiState::rollup(jobs.iter().map(|job| job.state));
    let started_at = jobs.iter().filter_map(|job| job.started_at).min();
    let completed_at = (!state.is_active())
        .then(|| jobs.iter().filter_map(|job| job.completed_at).max())
        .flatten();
    vec![CiRun {
        id: RunId::new(format!("fixture-run-{number}")),
        name: "ci".to_owned(),
        event: Some("pull_request".to_owned()),
        state,
        number: Some(run_number),
        head_sha: Sha::new(format!("{number:040x}")),
        branch: None,
        started_at,
        completed_at,
        url: Some(run_url),
        jobs,
    }]
}

/// An invented log for a fixture job: GitHub-style timestamps and
/// `##[group]` sections per step, colored test output and, for a failed
/// job, the error at the end.
#[must_use]
pub fn job_log(job: &CiJob) -> Vec<u8> {
    let start = job.started_at.unwrap_or_else(Utc::now);
    // GitHub's runner stamps lines with seven fractional digits.
    let stamp = |at: Timestamp| {
        format!(
            "{}.{:07}Z",
            at.format("%Y-%m-%dT%H:%M:%S"),
            at.timestamp_subsec_nanos() / 100
        )
    };
    let mut out = String::new();
    let mut line = |at: Timestamp, text: &str| {
        out.push_str(&stamp(at));
        out.push(' ');
        out.push_str(text);
        out.push('\n');
    };
    let steps = if job.steps.is_empty() {
        fixture_steps(&job.name, job.state, start, 60)
    } else {
        job.steps.clone()
    };
    for step in &steps {
        let Some(at) = step.started_at else { continue };
        line(at, &format!("##[group]{}", step.name));
        match step.name.as_str() {
            "Run tests" => {
                for n in 1..=120 {
                    let at = at + Duration::milliseconds(n * 40);
                    let fails = step.state == CiState::Fail && n % 37 == 0;
                    let text = if fails {
                        format!("test rows::case_{n:03} ... \u{1b}[31mFAILED\u{1b}[0m")
                    } else {
                        format!("test rows::case_{n:03} ... \u{1b}[32mok\u{1b}[0m")
                    };
                    line(at, &text);
                }
                if step.state == CiState::Fail {
                    let at = at + Duration::seconds(5);
                    line(at, "");
                    line(
                        at,
                        "\u{1b}[1mfailures:\u{1b}[0m rows::case_037, rows::case_074, rows::case_111",
                    );
                    line(
                        at,
                        "test result: \u{1b}[31mFAILED\u{1b}[0m. 117 passed; 3 failed",
                    );
                    line(at, "##[error]Process completed with exit code 101.");
                }
            }
            "Compile" | "Build" => {
                for (n, krate) in ["diff", "repo", "store", "forge", "app"].iter().enumerate() {
                    let at = at + Duration::milliseconds(n as i64 * 300);
                    line(
                        at,
                        &format!("\u{1b}[1;32m   Compiling\u{1b}[0m {krate} v0.1.0"),
                    );
                }
                line(
                    at + Duration::seconds(2),
                    "\u{1b}[33mwarning\u{1b}[0m: unused import",
                );
                line(
                    at + Duration::seconds(3),
                    "\u{1b}[1;32m    Finished\u{1b}[0m release in 41s",
                );
            }
            _ => {
                line(at, &format!("Running {}", step.name.to_lowercase()));
                line(at + Duration::milliseconds(200), "done");
            }
        }
        line(step.completed_at.unwrap_or(at), "##[endgroup]");
    }
    out.into_bytes()
}

impl Spec {
    fn build(self) -> (PullRequest, Detail) {
        let sha = format!(
            "{:07x}",
            u64::from(self.number).wrapping_mul(0x9E37_79B9) & 0x0FFF_FFFF
        );
        let (forge, repo) = repo_of(self.repo);
        let files: Vec<ChangedFile> = self
            .files
            .into_iter()
            .map(|f| ChangedFile {
                blob: blob_id(self.number, &f.path),
                ..f
            })
            .collect();
        let stat = (
            files.iter().map(|f| f.added).sum(),
            files.iter().map(|f| f.removed).sum(),
        );
        let row = PullRequest {
            key: PrKey::new(repo.clone(), PrNumber(u64::from(self.number))),
            forge,
            title: s(self.title),
            author: s(self.author),
            author_avatar: Some(avatar_of(self.author)),
            reviewers: self.reviewers.iter().map(|r| s(r)).collect(),
            reviewer_avatars: self.reviewers.iter().map(|r| Some(avatar_of(r))).collect(),
            head: s(self.head),
            base: s(self.base),
            sha: sha.into(),
            ci: self.ci,
            review: self.review,
            state: if self.review == ReviewState::Draft {
                forge::model::PrState::Draft
            } else {
                forge::model::PrState::Open
            },
            updated_at: Utc::now() - Duration::minutes(i64::from(self.minutes_ago)),
            description: s(self.description),
            stat: Some(stat),
            // Invented, but stable per number; GitHub alone reports threads.
            comments: Some(self.number * 7 % 23),
            unresolved: (forge == ForgeKind::GitHub).then_some(self.number % 4),
            source: None,
        };
        let checks = checks_for(self.ci);
        let detail = Detail {
            files,
            runs: fixture_runs(self.number, &repo, &checks),
            checks,
        };
        (row, detail)
    }
}

const HOUR: u32 = 60;
const DAY: u32 = 24 * HOUR;

#[expect(clippy::too_many_lines, reason = "a flat table of fixture rows")]
fn specs() -> Vec<Spec> {
    use CiState::{Canceled, Fail, None as NoCi, Pass, Queued, Running};
    use FileStatus::{Added, Deleted, Modified, Renamed};
    use ReviewState::{Approved, ChangesRequested, Commented, Draft, Pending};

    let specs = vec![
        Spec {
            repo: REVIEW,
            number: 38,
            title: "Read objects straight from pack files",
            author: ME,
            reviewers: &["mira"],
            head: "cora/pack-reader",
            base: "main",
            ci: Pass,
            review: Approved,
            minutes_ago: 3 * HOUR,
            description: "Adds a pack index reader and a delta resolver so blobs come from the pack without shelling out to git.\n\nFirst of four: the tree diff, the hunk cache and the split renderer stack on this one.\n\n- reads v2 indexes with the fan-out table\n- resolves OFS and REF deltas with a small LRU for bases\n- falls back to loose objects",
            files: vec![
                file("crates/repo/src/pack/index.rs", Added, 212, 0),
                file("crates/repo/src/pack/delta.rs", Added, 148, 0),
                file("crates/repo/src/pack/mod.rs", Added, 64, 0),
                file("crates/repo/src/odb.rs", Modified, 37, 11),
                file("crates/repo/tests/pack_roundtrip.rs", Added, 96, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 39,
            title: "Tree diff for the file list",
            author: ME,
            reviewers: &["mira"],
            head: "cora/tree-diff",
            base: "cora/pack-reader",
            ci: Pass,
            review: Commented,
            minutes_ago: 3 * HOUR + 12,
            description: "Walks two trees and yields changed paths with add and remove counts, so the sidebar can show files before any hunk is computed.\n\nStacked on #38.",
            files: vec![
                file("crates/repo/src/tree_diff.rs", Added, 188, 0),
                file("crates/repo/src/lib.rs", Modified, 4, 1),
                file("crates/repo/tests/tree_diff.rs", Added, 74, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 40,
            title: "Hunk cache keyed by blob id",
            author: ME,
            reviewers: &[],
            head: "cora/hunk-cache",
            base: "cora/tree-diff",
            ci: Running,
            review: Pending,
            minutes_ago: 22,
            description: "Caches computed hunks by (old blob, new blob, options) so reopening a file is a lookup.\n\nStacked on #39.",
            files: vec![
                file("crates/diff/src/cache.rs", Added, 131, 0),
                file("crates/diff/src/lib.rs", Modified, 9, 3),
                file("crates/store/src/schema.sql", Modified, 12, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 41,
            title: "Split view renderer",
            author: ME,
            reviewers: &[],
            head: "cora/split-view",
            base: "cora/hunk-cache",
            ci: Fail,
            review: Draft,
            minutes_ago: 9,
            description: "Side-by-side layout with aligned rows and a shared scroll position.\n\nStacked on #40. Windows tests fail on the row-height rounding; looking at it.",
            files: vec![
                file("crates/diff_view/src/split.rs", Added, 302, 0),
                file("crates/diff_view/src/unified.rs", Modified, 28, 14),
                file("crates/diff_view/src/rows.rs", Renamed, 17, 17),
                file("crates/diff_view/src/legacy.rs", Deleted, 0, 96),
                file("crates/diff_view/tests/split.rs", Added, 84, 0),
                file("docs/split-view.md", Added, 23, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 42,
            title: "Fix line wrap on empty hunks",
            author: "mira",
            reviewers: &[ME],
            head: "mira/empty-hunk-wrap",
            base: "main",
            ci: Pass,
            review: Pending,
            minutes_ago: 45,
            description: "An empty hunk header wrapped to a second line and shifted every row below it by one.\n\nAdds a regression test with the recorded repro.",
            files: vec![
                file("crates/diff_view/src/rows.rs", Modified, 8, 3),
                file("crates/diff_view/tests/wrap.rs", Added, 41, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 43,
            title: "Machine config for SSH terminals",
            author: "tobias",
            reviewers: &["mira"],
            head: "tobias/machines",
            base: "main",
            ci: NoCi,
            review: Pending,
            minutes_ago: DAY + 2 * HOUR,
            description: "Reads `[[machine]]` tables from the config file and exposes them to the terminal launcher.",
            files: vec![
                file(".claude/settings.json", Modified, 9, 1),
                file(".mcp.json", Added, 8, 0),
                file("crates/term/src/machine.rs", Added, 119, 0),
                file("crates/term/src/lib.rs", Modified, 6, 0),
            ],
        },
        Spec {
            repo: REVIEW,
            number: 44,
            title: "Terminal resize without a reflow flash",
            author: "wen",
            reviewers: &[],
            head: "wen/term-resize",
            base: "main",
            ci: Queued,
            review: Draft,
            minutes_ago: 5 * DAY,
            description: "Draft. Debounces the PTY resize and keeps the old grid on screen until the new one is ready.",
            files: vec![file("crates/term_view/src/grid.rs", Modified, 54, 21)],
        },
        Spec {
            repo: API,
            number: 1288,
            title: "Rate-limit token refresh per client",
            author: "priya",
            reviewers: &[ME],
            head: "priya/refresh-limit",
            base: "main",
            ci: Fail,
            review: Pending,
            minutes_ago: HOUR + 10,
            description: "Refresh storms from one misbehaving client were starving the rest. Adds a token bucket keyed by client id in front of the refresh handler.\n\nThe integration job fails when Redis is slow; see the checks tab.",
            files: vec![
                file("src/auth/refresh.go", Modified, 64, 18),
                file("src/auth/bucket.go", Added, 93, 0),
                file("src/auth/bucket_test.go", Added, 120, 0),
                file("docs/auth.md", Modified, 11, 2),
            ],
        },
        Spec {
            repo: API,
            number: 1291,
            title: "Paginate /v2/orders with keyset cursors",
            author: "ansel",
            reviewers: &["priya"],
            head: "ansel/orders-keyset",
            base: "main",
            ci: Pass,
            review: Approved,
            minutes_ago: 6 * HOUR,
            description: "Offset pagination got slow past page 400. Switches the orders list to keyset cursors and keeps `page` working for one release.",
            files: vec![
                file("src/orders/list.go", Modified, 77, 41),
                file("src/orders/cursor.go", Added, 58, 0),
                file("src/orders/list_test.go", Modified, 66, 12),
            ],
        },
        Spec {
            repo: API,
            number: 1295,
            title: "Drop the legacy webhook signer",
            author: "dara",
            reviewers: &["ansel"],
            head: "dara/drop-signer-v1",
            base: "main",
            ci: Canceled,
            review: ChangesRequested,
            minutes_ago: 2 * DAY,
            description: "Removes the v1 HMAC signer. All known receivers moved to v2 in March.",
            files: vec![
                file("src/webhooks/signer_v1.go", Deleted, 0, 140),
                file("src/webhooks/dispatch.go", Modified, 5, 33),
            ],
        },
        Spec {
            repo: API,
            number: 1297,
            title: "Idempotency keys for POST /payments",
            author: ME,
            reviewers: &["priya", "ansel"],
            head: "cora/idempotency",
            base: "main",
            ci: Pass,
            review: Commented,
            minutes_ago: 14 * HOUR,
            description: "Clients may send an `Idempotency-Key` header. Same key and same body returns the stored response; same key and a different body is a 422.\n\nKeys expire after 24 hours.",
            files: vec![
                file("src/payments/create.go", Modified, 52, 9),
                file("src/payments/idem.go", Added, 141, 0),
                file("src/payments/idem_test.go", Added, 188, 0),
                file("migrations/0042_idempotency.sql", Added, 18, 0),
            ],
        },
        Spec {
            repo: API,
            number: 1299,
            title: "Bump the HTTP client to 4.2",
            author: "dara",
            reviewers: &[],
            head: "dara/http-4-2",
            base: "main",
            ci: NoCi,
            review: Pending,
            minutes_ago: 9 * DAY,
            description: "Routine bump. Release notes list no breaking changes.",
            files: vec![
                file("go.mod", Modified, 2, 2),
                file("go.sum", Modified, 6, 6),
            ],
        },
        Spec {
            repo: WEB,
            number: 412,
            title: "Lazy-load the settings route",
            author: "jun",
            reviewers: &[ME, "mira"],
            head: "jun/lazy-settings",
            base: "main",
            ci: Pass,
            review: Pending,
            minutes_ago: 2 * HOUR,
            description: "Splits the settings screens into their own chunk. First load drops by 38 kB gzipped.",
            files: vec![
                file("src/routes/settings/index.tsx", Modified, 14, 6),
                file("src/routes/index.tsx", Modified, 5, 2),
                file("vite.config.ts", Modified, 8, 0),
            ],
        },
        Spec {
            repo: WEB,
            number: 415,
            title: "Fix the focus trap in nested dialogs",
            author: "mira",
            reviewers: &["jun"],
            head: "mira/focus-trap",
            base: "main",
            ci: Pass,
            review: Pending,
            minutes_ago: DAY + 4 * HOUR,
            description: "Closing an inner dialog returned focus to the page instead of the outer dialog.",
            files: vec![
                file("src/ui/dialog.tsx", Modified, 31, 12),
                file("src/ui/dialog.test.tsx", Modified, 47, 3),
            ],
        },
        Spec {
            repo: WEB,
            number: 418,
            title: "Move buttons to design tokens",
            author: "wen",
            reviewers: &["jun"],
            head: "wen/button-tokens",
            base: "main",
            ci: Fail,
            review: ChangesRequested,
            minutes_ago: 3 * DAY,
            description: "Replaces hard-coded colors in the button family with tokens. Visual diffs are attached to the pipeline.",
            files: vec![
                file("src/ui/button.tsx", Modified, 44, 39),
                file("src/ui/button.css", Modified, 61, 70),
                file("src/ui/tokens.css", Modified, 12, 0),
            ],
        },
    ];

    specs
}

/// The fixture inbox with what each of its pull requests opens to.
pub struct Fixtures {
    pub inbox: Inbox,
    details: HashMap<PrKey, Detail>,
}

enum Query {
    ReviewRequested,
    CreatedByMe,
    Repo(usize),
}

impl Fixtures {
    pub fn load() -> Self {
        let (prs, details): (Vec<PullRequest>, Vec<Detail>) =
            specs().into_iter().map(Spec::build).unzip();
        let details = prs.iter().map(|pr| pr.key.clone()).zip(details).collect();
        let section = |title: &str, query: Query, expanded_by_default: bool| {
            let items = prs
                .iter()
                .enumerate()
                .filter(|(_, pr)| match query {
                    Query::ReviewRequested => {
                        pr.reviewers.iter().any(|r| r == ME) && pr.author != ME
                    }
                    Query::CreatedByMe => pr.author == ME,
                    Query::Repo(repo) => pr.key.repo == repo_of(repo).1,
                })
                .map(|(ix, _)| ix)
                .collect();
            Section {
                title: s(title),
                items,
                expanded_by_default,
                state: SectionState::Fresh,
                repo: match query {
                    Query::Repo(repo) => Some(repo_of(repo).1),
                    Query::ReviewRequested | Query::CreatedByMe => None,
                },
            }
        };
        let sections = vec![
            section("Review requested", Query::ReviewRequested, true),
            section("Created by me", Query::CreatedByMe, true),
            section("cora/review: open", Query::Repo(REVIEW), false),
            section("acme/api: open", Query::Repo(API), false),
            section("acme/web: open", Query::Repo(WEB), false),
        ];
        Self {
            inbox: Inbox { prs, sections },
            details,
        }
    }

    pub fn detail(&self, key: &PrKey) -> Detail {
        self.details.get(key).cloned().unwrap_or_default()
    }

    /// The forge's record of a fixture pull request, open and with a head
    /// the viewer may push to, so cards offer what a writable pull request
    /// would (Apply suggestion).
    pub fn forge_pr(&self, key: &PrKey) -> Option<forge::model::PullRequest> {
        let row = self.inbox.get(key)?;
        let branch = |name: &SharedString| forge::model::BranchRef {
            repo: Some(key.repo.clone()),
            name: name.to_string(),
            sha: forge::model::Sha::new(row.sha.to_string()),
        };
        Some(forge::model::PullRequest {
            id: forge::model::PullRequestId::new(format!("fixture-{}", key.number)),
            repo: key.repo.clone(),
            number: key.number,
            title: row.title.to_string(),
            body: row.description.to_string(),
            author: user(&row.author),
            state: row.state,
            base: branch(&row.base),
            head: branch(&row.head),
            merge_base: None,
            created_at: row.updated_at,
            updated_at: row.updated_at,
            review_decision: forge::model::ReviewDecision::None,
            reviewers: row
                .reviewers
                .iter()
                .map(|login| {
                    forge::model::Reviewer::new(user(login), fixture_review(login, row.review))
                })
                .collect(),
            ci: Some(row.ci),
            comment_count: None,
            unresolved_threads: None,
            labels: fixture_labels_of(key.number.0),
            diff_stat: None,
            assignees: vec![user(&row.author)],
            milestone: key
                .number
                .0
                .is_multiple_of(2)
                .then(|| forge::model::Milestone {
                    title: "v0.5".to_owned(),
                }),
            web_url: String::new(),
            head_writable: true,
        })
    }

    /// The stack by the base-branch rule, bottom first.
    pub fn stack(&self, key: &PrKey) -> Stack {
        let prs = &self.inbox.prs;
        let Some(start) = prs.iter().position(|pr| &pr.key == key) else {
            return Stack::default();
        };
        let mut stack = vec![start];
        // Bounded so a cycle in bad data cannot hang the UI thread.
        for _ in 0..prs.len() {
            let first = &prs[stack[0]];
            let Some(parent) = prs
                .iter()
                .position(|p| p.key.repo == first.key.repo && p.head == first.base)
            else {
                break;
            };
            stack.insert(0, parent);
        }
        for _ in 0..prs.len() {
            let last = &prs[stack[stack.len() - 1]];
            let Some(child) = prs
                .iter()
                .position(|p| p.key.repo == last.key.repo && p.base == last.head)
            else {
                break;
            };
            stack.push(child);
        }
        Stack {
            base: prs[stack[0]].base.clone(),
            members: stack
                .into_iter()
                .map(|ix| StackMember {
                    key: prs[ix].key.clone(),
                    title: prs[ix].title.clone(),
                    author: prs[ix].author.clone(),
                    author_avatar: prs[ix].author_avatar.clone(),
                    ci: prs[ix].ci,
                    head: prs[ix].head.clone(),
                    base: prs[ix].base.clone(),
                })
                .collect(),
        }
    }

    /// Files already viewed when the app starts: enough to show every
    /// directory state in the sidebar.
    pub fn viewed(&self, key: &PrKey, files: &[ChangedFile]) -> Viewed {
        const SEEDED: &[(u64, &str)] = &[
            (41, "crates/diff_view/src/split.rs"),
            (41, "crates/diff_view/src/unified.rs"),
            (41, "crates/diff_view/tests/split.rs"),
            (38, "crates/repo/src/pack/index.rs"),
            (38, "crates/repo/src/pack/delta.rs"),
            (38, "crates/repo/src/pack/mod.rs"),
            (38, "crates/repo/src/odb.rs"),
        ];
        let mut viewed = Viewed::default();
        for file in files {
            if SEEDED.contains(&(key.number.0, &*file.path)) {
                viewed.set(file, true);
            }
        }
        viewed
    }
}

/// A reviewer's state on a fixture pull request: the viewer is only asked,
/// so the header reads "not reviewed by you"; the others gave the row's
/// verdict.
fn fixture_review(login: &str, review: ReviewState) -> Verdict {
    if login == ME {
        return Verdict::Requested;
    }
    match review {
        ReviewState::Approved => Verdict::Approved,
        ReviewState::ChangesRequested => Verdict::ChangesRequested,
        ReviewState::Commented => Verdict::Commented,
        ReviewState::Pending | ReviewState::Draft => Verdict::Requested,
    }
}

/// The people a fixture repository can ask to review or assign.
#[must_use]
pub fn collaborators() -> Vec<User> {
    let mut people: Vec<User> = std::iter::once(ME)
        .chain(PEOPLE)
        .chain(["jun", "tobias-k"])
        .map(user)
        .collect();
    for (person, name) in people.iter_mut().zip([
        "Cora",
        "Mira Okafor",
        "Tobias Lind",
        "Wen Zhou",
        "Priya Raman",
        "Ansel Ward",
        "Jun Park",
        "Tobias Klein",
    ]) {
        person.name = Some(name.to_owned());
    }
    people
}

/// The labels every fixture repository has: forge colors from the dark to
/// the very light, so chips are checked on both.
#[must_use]
pub fn labels() -> Vec<forge::model::Label> {
    [
        ("bug", "d73a4a", "Something is not working"),
        (
            "documentation",
            "0075ca",
            "Improvements or additions to documentation",
        ),
        ("enhancement", "a2eeef", "New feature or request"),
        ("good first issue", "7057ff", "Good for newcomers"),
        ("performance", "fbca04", "Makes something faster or smaller"),
        ("needs rebase", "000000", "Conflicts with the base branch"),
        ("wontfix", "ffffff", "This will not be worked on"),
        ("security", "b60205", ""),
    ]
    .into_iter()
    .map(|(name, color, description)| forge::model::Label {
        name: name.to_owned(),
        color: Some(color.to_owned()),
        description: (!description.is_empty()).then(|| description.to_owned()),
    })
    .collect()
}

/// A stable pick of two or three labels per pull request.
fn fixture_labels_of(number: u64) -> Vec<forge::model::Label> {
    let all = labels();
    let count = 2 + (number % 2) as usize;
    (0..count)
        .map(|k| all[(number as usize * 3 + k * 2) % all.len()].clone())
        .collect()
}

/// A synthetic diff over the pull request's file paths; `min_rows` asks for
/// a big one, for benchmarks.
pub fn synthetic_diff(number: u64, files: &[ChangedFile], min_rows: Option<usize>) -> diff::Diff {
    let seed = number ^ 0x5eed_d1ff;
    let config = match min_rows {
        Some(rows) => SynthConfig::rows(seed, Layout::Unified, rows),
        None => SynthConfig {
            seed,
            max_files: files.len(),
            target_rows: None,
            block_every: 0,
        },
    };
    // Blocks come from the threads, not from the synthesizer.
    let config = SynthConfig {
        block_every: 0,
        ..config
    };
    let mut diff = synthesize(&config);
    for (file, changed) in diff.files.iter_mut().zip(files) {
        file.path = changed.path.to_string();
    }
    diff
}

/// A stable fake commit id: 40 hex digits from `number` and `ix`.
fn commit_sha(number: u64, ix: usize) -> String {
    let mut hash = number ^ 0xC0FF_EE00_D15E_A5E5;
    let mut digits = String::with_capacity(40);
    for round in 0..3u64 {
        hash = (hash ^ (ix as u64) ^ (round << 32)).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        hash ^= hash >> 29;
        let _ = write!(digits, "{hash:016x}");
    }
    digits.truncate(40);
    digits
}

const COMMIT_TITLES: [&str; 6] = [
    "Add the reader and its tests",
    "Handle the empty case",
    "Rename the cache fields",
    "Fix the overflow on large inputs",
    "Address review comments",
    "Split the parser out of the loader",
];

/// A fixture pull request's commits, newest first: two to five, the newest
/// at its head, and a merge of the base branch among the longer ones.
pub fn commits(pr: &PullRequest) -> Vec<PrCommit> {
    let number = pr.key.number.0;
    let count = 2 + (number % 4) as usize;
    let newest = pr.updated_at - Duration::minutes(10);
    let sha = |ix: usize| -> SharedString {
        if ix == 0 {
            format!("{}{}", pr.sha, &commit_sha(number, 0)[pr.sha.len()..]).into()
        } else {
            commit_sha(number, ix).into()
        }
    };
    (0..count)
        .map(|ix| {
            let merge = count >= 4 && ix == 1;
            let title = if merge {
                format!("Merge branch '{}' into {}", pr.base, pr.head)
            } else {
                COMMIT_TITLES[(number as usize + ix) % COMMIT_TITLES.len()].to_owned()
            };
            let author = if ix == 2 {
                s(PEOPLE[number as usize % PEOPLE.len()])
            } else {
                pr.author.clone()
            };
            PrCommit {
                sha: sha(ix),
                title: title.into(),
                author,
                authored: newest - Duration::hours(3 * ix as i64),
                parent: Some(commit_sha(number, ix + 1).into()),
                merge,
            }
        })
        .collect()
}

/// A synthetic diff of one fixture commit: some of the pull request's
/// files, picked by the commit's id.
pub fn commit_diff(number: u64, commit: &PrCommit, files: &[ChangedFile]) -> diff::Diff {
    let seed = u64::from_str_radix(commit.sha.get(..16).unwrap_or("0"), 16).unwrap_or(number);
    let mut picked: Vec<&ChangedFile> = files
        .iter()
        .enumerate()
        .filter(|(ix, _)| (seed >> (ix % 64)) & 1 == 1)
        .map(|(_, file)| file)
        .collect();
    if picked.is_empty()
        && let Some(file) = files.get(seed as usize % files.len().max(1))
    {
        picked.push(file);
    }
    let mut diff = synthesize(&SynthConfig {
        seed: seed ^ number,
        max_files: picked.len(),
        target_rows: None,
        block_every: 0,
    });
    diff.files.truncate(picked.len());
    for (file, changed) in diff.files.iter_mut().zip(picked) {
        file.path = changed.path.to_string();
    }
    diff
}

const PEOPLE: [&str; 5] = ["mira", "tobias", "wen", "priya", "ansel"];

const REMARKS: [&str; 8] = [
    "This loop can overflow for large exponents. Consider `checked_mul`, or `saturating_mul` if a clamp is what callers expect.",
    "Nice simplification. Can you add a test for the empty case? The old code returned early and this path now walks the whole list.",
    "Will do, tracking it in a follow-up so this one stays small.",
    "Why drop the plain subtraction? It read fine, and the wrapping version hides the case where the counter goes backwards.",
    "Prefer `% 2` here, it reads better.",
    "Is the lock still needed now that the cache is per thread?\n\nIf not, removing it also drops the poisoning branch below.",
    "Good catch, fixed in the next push.",
    "Could this be a `match` on the enum instead of two `if let` arms? A new variant would then be a compile error here.",
];

fn person(n: usize) -> User {
    user(PEOPLE[n % PEOPLE.len()])
}

fn comment(id: String, n: usize, minutes_ago: i64, verdict: Option<Verdict>) -> Comment {
    Comment {
        id: CommentId::new(id.clone()),
        author: person(n),
        body: REMARKS[n % REMARKS.len()].to_owned(),
        created_at: Utc::now() - Duration::minutes(minutes_ago),
        edited_at: None,
        web_url: None,
        review: verdict.map(|_| ReviewId::new(id)),
        verdict,
        suggestions: Vec::new(),
    }
}

fn at(side: Side, line: u32) -> LineRef {
    LineRef { side, line }
}

/// The kinds of anchored thread the fixtures cycle through.
#[derive(Clone, Copy)]
enum Kind {
    Open,
    Resolved,
    Range,
    RemovedSide,
    /// Outdated, its line still in the diff.
    OutdatedShown,
    /// Outdated, its line gone: listed at the top of the file.
    OutdatedGone,
}

const KINDS: [Kind; 6] = [
    Kind::Open,
    Kind::Resolved,
    Kind::Range,
    Kind::RemovedSide,
    Kind::OutdatedShown,
    Kind::OutdatedGone,
];

/// Review threads on a synthetic diff: a thread every few hunks, cycling
/// through open, resolved, multi-line, removed-side and outdated ones, then
/// conversation comments and review verdicts, oldest first. Lines in the
/// forge model are 1-based.
pub fn threads(diff: &diff::Diff) -> Vec<Thread> {
    let hunks: usize = diff.files.iter().map(|f| f.hunks.len()).sum();
    let every = if hunks < 60 { 2 } else { 6 };
    let mut out = Vec::new();
    let mut seen = 0usize;
    for file in &diff.files {
        for hunk in &file.hunks {
            seen += 1;
            if !seen.is_multiple_of(every) || hunk.new_len() == 0 {
                continue;
            }
            let n = out.len();
            out.push(anchored(n, &file.path, hunk));
        }
    }
    let conversation = [
        (Some(Verdict::Commented), 0, 60 * 24 * 4),
        (None, 5, 60 * 24 * 2),
        (Some(Verdict::ChangesRequested), 3, 60 * 30),
        (None, 6, 60 * 20),
        (Some(Verdict::Approved), 1, 90),
    ];
    for (k, (verdict, n, minutes)) in conversation.into_iter().enumerate() {
        out.push(Thread {
            id: ThreadId::new(format!("conversation-{k}")),
            anchor: None,
            resolution: Resolution::NotResolvable,
            outdated: false,
            comments: vec![comment(format!("conversation-{k}"), n, minutes, verdict)],
        });
    }
    out.sort_by_key(|thread| thread.comments[0].created_at);
    out
}

/// The `n`-th anchored thread, on the last new line of `hunk` unless its
/// kind wants otherwise.
fn anchored(n: usize, path: &str, hunk: &diff::Hunk) -> Thread {
    let kind = KINDS[n % KINDS.len()];
    let last_new = hunk.new_start + hunk.new_len();
    let line = |side, line| Position {
        line: at(side, line),
        start: None,
        commit: None,
    };
    let (position, original, outdated) = match kind {
        Kind::Open | Kind::Resolved => (Some(line(Side::New, last_new)), None, false),
        Kind::Range => {
            let start = last_new.saturating_sub(2).max(hunk.new_start + 1);
            let range = Position {
                start: Some(at(Side::New, start)),
                ..line(Side::New, last_new)
            };
            (Some(range), None, false)
        }
        Kind::RemovedSide => match removed_line(hunk) {
            Some(old) => (Some(line(Side::Old, old)), None, false),
            None => (Some(line(Side::New, last_new)), None, false),
        },
        Kind::OutdatedShown => (None, Some(line(Side::New, last_new)), true),
        Kind::OutdatedGone => (None, Some(line(Side::New, last_new + 400)), true),
    };
    let replies = 1 + n % 3;
    let base = 60 * 24 * 3 - (n as i64) * 7;
    let mut comments: Vec<Comment> = (0..replies)
        .map(|k| comment(format!("c-{n}-{k}"), n + k, base - k as i64 * 30, None))
        .collect();
    // Every open thread's first comment suggests a change to its line.
    if matches!(kind, Kind::Open) {
        comments[0].body = SUGGESTION.to_owned();
    }
    Thread {
        id: ThreadId::new(format!("thread-{n}")),
        anchor: Some(Anchor {
            path: path.to_owned(),
            old_path: None,
            position,
            original,
        }),
        resolution: match kind {
            Kind::Resolved => Resolution::Resolved {
                by: Some(person(n + 1)),
            },
            _ => Resolution::Unresolved,
        },
        outdated,
        comments,
    }
}

const SUGGESTION: &str = "This reads better as an iterator, and the comment says why:\n\n```suggestion\nlet total: u32 = parts.iter().map(|p| p.len).sum();  \n\t// one long comment line that goes past the card so the confirmation has to wrap it: the quick brown fox jumps over the lazy dog, then does it again, and again, and once more\n```";

/// The first removed line of a hunk, 1-based on the old side.
fn removed_line(hunk: &diff::Hunk) -> Option<u32> {
    let mut old = hunk.old_start;
    for segment in &hunk.segments {
        if let Segment::Change { removed, .. } = *segment
            && removed > 0
        {
            return Some(old + 1);
        }
        old += segment.old_len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_counts_match_the_brief() {
        let fixtures = Fixtures::load();
        let counts: Vec<usize> = fixtures
            .inbox
            .sections
            .iter()
            .map(|s| s.items.len())
            .collect();
        assert_eq!(counts, [3, 5, 7, 5, 3]);
        assert_eq!(fixtures.inbox.prs.len(), 15);
    }

    #[test]
    fn fixtures_cover_every_ci_state() {
        let fixtures = Fixtures::load();
        let mut seen: Vec<CiState> = Vec::new();
        for pr in &fixtures.inbox.prs {
            seen.push(pr.ci);
            seen.extend(fixtures.detail(&pr.key).checks.iter().map(|c| c.state));
        }
        for state in [
            CiState::Pass,
            CiState::Fail,
            CiState::Running,
            CiState::Queued,
            CiState::Skipped,
            CiState::Canceled,
            CiState::None,
        ] {
            assert!(seen.contains(&state), "{state:?} is missing");
        }
    }

    #[test]
    fn seeded_viewed_files_match_pull_request_files() {
        let fixtures = Fixtures::load();
        let total: usize = fixtures
            .inbox
            .prs
            .iter()
            .map(|pr| {
                let files = fixtures.detail(&pr.key).files;
                fixtures.viewed(&pr.key, &files).count(&files)
            })
            .sum();
        assert_eq!(total, 7);
    }

    #[test]
    fn fixture_threads_cover_every_kind_and_place_on_the_diff() {
        use crate::threads::place;

        let fixtures = Fixtures::load();
        let pr = fixtures
            .inbox
            .prs
            .iter()
            .find(|pr| pr.number() == 41)
            .expect("fixture exists");
        let detail = fixtures.detail(&pr.key);
        let diff = synthetic_diff(41, &detail.files, None);
        let threads = threads(&diff);
        assert!(
            threads
                .windows(2)
                .all(|w| w[0].comments[0].created_at <= w[1].comments[0].created_at),
            "oldest first"
        );
        let anchored = threads.iter().filter(|t| t.anchor.is_some()).count();
        assert!(anchored >= KINDS.len(), "{anchored} anchored threads");
        let verdicts: Vec<_> = threads
            .iter()
            .filter_map(|t| t.comments[0].verdict)
            .collect();
        assert_eq!(
            verdicts,
            [
                Verdict::Commented,
                Verdict::ChangesRequested,
                Verdict::Approved
            ]
        );
        let placed = place(&threads, &diff);
        assert_eq!(placed.len(), anchored, "every anchored thread has a block");
        assert!(placed.iter().any(|p| p.outdated_group));
        assert!(placed.iter().any(|p| p.first_line.is_some()));
        assert!(placed.iter().any(|p| matches!(
            p.anchor,
            diff::Anchor::Line {
                side: diff::Side::Old,
                ..
            }
        )));
        assert!(threads.iter().any(Thread::is_resolved));
    }

    #[test]
    fn fixture_logs_have_a_group_per_step_and_the_error() {
        let fixtures = Fixtures::load();
        let pr = fixtures
            .inbox
            .prs
            .iter()
            .find(|pr| pr.number() == 1288)
            .expect("fixture exists");
        let runs = fixtures.detail(&pr.key).runs;
        let failed = runs[0]
            .jobs
            .iter()
            .find(|job| job.state == CiState::Fail)
            .expect("a failed job");
        let log = String::from_utf8(job_log(failed)).unwrap();
        assert_eq!(log.matches("##[group]").count(), failed.steps.len() - 1);
        assert!(log.contains("##[error]"));
        assert!(
            log.starts_with(
                &failed.steps[0]
                    .started_at
                    .unwrap()
                    .format("%Y-%m-%dT%H:%M:%S.")
                    .to_string()
            )
        );
    }

    #[test]
    fn the_stack_is_four_deep_from_any_member() {
        let fixtures = Fixtures::load();
        for number in 38..=41 {
            let pr = fixtures
                .inbox
                .prs
                .iter()
                .find(|pr| pr.number() == number)
                .expect("fixture exists");
            let stack = fixtures.stack(&pr.key);
            let numbers: Vec<u64> = stack.members.iter().map(|m| m.key.number.0).collect();
            assert_eq!(numbers, [38, 39, 40, 41]);
            assert_eq!(stack.base, "main");
        }
    }
}
