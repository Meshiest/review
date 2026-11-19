//! github backend against recordings of `review-sandbox`.
//!
//! Record: `REVIEW_RECORD=1 cargo test -p forge --test conformance github`.
//! The `scripted` tests cover what the sandbox cannot show (CI, paging,
//! review summaries, native stacks) with hand-written responses.

use std::ops::Deref;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge::auth::{Credential, StaticCredential, Token};
use forge::github::{GitHub, inbox_query};
use forge::http::{HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Transport};
use forge::model::{
    AccountKind, CiState, DiffStat, InboxSection, PrNumber, PrState, PullRequest, Resolution,
    ReviewDecision, ReviewState, Side, StackSource, Thread, ViewedState,
};
use forge::record::{RECORD_ENV, ReplayTransport, fixture_client, fixture_path};
use forge::{Error, Forge};
use serde_json::{Value, json};

use super::common::repo;

const HOST: &str = "github.com";

/// A GitHub backend replaying (or, with `REVIEW_RECORD=1`, recording) one
/// cassette. `done` checks every recorded request was used.
struct Fixture {
    github: GitHub,
    replay: Option<Arc<ReplayTransport>>,
}

impl Deref for Fixture {
    type Target = GitHub;

    fn deref(&self) -> &GitHub {
        &self.github
    }
}

fn options() -> HttpOptions {
    HttpOptions {
        clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
        ..HttpOptions::for_kind(forge::model::ForgeKind::GitHub)
    }
}

pub(super) fn client(transport: Arc<dyn Transport>) -> HttpClient {
    HttpClient::new(
        forge::model::ForgeKind::GitHub,
        HOST,
        transport,
        Arc::new(StaticCredential(Credential::bearer(Token::new("replay")))),
        options(),
    )
}

impl Fixture {
    async fn open(name: &str) -> Self {
        let kind = forge::model::ForgeKind::GitHub;
        if std::env::var(RECORD_ENV).is_ok_and(|v| v == "1") {
            let http = fixture_client(kind, HOST, name).unwrap();
            return Self {
                github: GitHub::connect(http).await.unwrap(),
                replay: None,
            };
        }
        let replay = Arc::new(ReplayTransport::load(&fixture_path(kind, name)).unwrap());
        Self {
            github: GitHub::connect(client(replay.clone())).await.unwrap(),
            replay: Some(replay),
        }
    }

    fn done(self) {
        if let Some(replay) = self.replay {
            assert_eq!(replay.unused(), Vec::<String>::new());
        }
    }
}

fn numbers(prs: &[PullRequest]) -> Vec<u64> {
    let mut numbers: Vec<u64> = prs.iter().map(|pr| pr.number.0).collect();
    numbers.sort_unstable();
    numbers
}

fn sandbox_query(rest: &str) -> InboxSection {
    InboxSection::Query(format!("repo:Lovelace/review-sandbox {rest}"))
}

#[tokio::test]
async fn whoami_and_inbox() {
    let github = Fixture::open("inbox").await;
    let me = github.whoami().await.unwrap();
    assert_eq!(me.login, "Lovelace");
    assert_eq!(me.kind, AccountKind::User);
    assert!(me.id.is_some());

    let open = github
        .inbox(&InboxSection::OpenInRepo(repo()))
        .await
        .unwrap();
    assert_eq!(numbers(&open), [1, 2, 3, 4, 5, 8]);
    assert!(open.iter().all(|pr| pr.state.is_open()));
    assert!(open.iter().all(|pr| pr.repo == repo()));
    assert!(open.windows(2).all(|w| w[0].updated_at >= w[1].updated_at));
    let draft = open.iter().find(|pr| pr.number == PrNumber(8)).unwrap();
    assert_eq!(draft.state, PrState::Draft);

    // Inbox rows carry what the list shows: line counts, and the comments of
    // the threads PR (6 review comments in 5 threads, 4 unresolved).
    let stat = |n: u64| open.iter().find(|pr| pr.number == PrNumber(n)).unwrap();
    assert_eq!(
        stat(5).diff_stat,
        Some(DiffStat {
            additions: 72,
            deletions: 28,
            changed_files: 27
        })
    );
    assert_eq!(
        (stat(4).comment_count, stat(4).unresolved_threads),
        (Some(6), Some(4))
    );
    assert_eq!(
        (stat(1).comment_count, stat(1).unresolved_threads),
        (Some(0), Some(0))
    );

    // The sandbox's pull requests are all mine; nobody asked me to review any.
    let authored = github
        .inbox(&sandbox_query("is:open author:@me"))
        .await
        .unwrap();
    assert_eq!(numbers(&authored), [1, 2, 3, 4, 5, 8]);
    let requested = github
        .inbox(&sandbox_query("is:open review-requested:@me"))
        .await
        .unwrap();
    assert!(requested.is_empty());

    let closed = github
        .inbox(&sandbox_query("is:closed in:title note"))
        .await
        .unwrap();
    assert_eq!(numbers(&closed), [6, 7]);
    github.done();
}

#[test]
fn inbox_sections_search_for_open_pull_requests() {
    assert_eq!(
        inbox_query(&InboxSection::ReviewRequested),
        "is:pr is:open review-requested:@me sort:updated-desc"
    );
    assert_eq!(
        inbox_query(&InboxSection::AuthoredByMe),
        "is:pr is:open author:@me sort:updated-desc"
    );
    assert_eq!(
        inbox_query(&InboxSection::OpenInRepo(repo())),
        "is:pr is:open repo:Lovelace/review-sandbox sort:updated-desc"
    );
}

#[tokio::test]
async fn pull_request_states_and_fields() {
    let sandbox = repo();
    let github = Fixture::open("pull_requests").await;
    let get = |n: u64| github.pull_request(&sandbox, PrNumber(n));

    let one = get(1).await.unwrap();
    assert_eq!(one.repo, repo());
    assert_eq!(one.title, "Stack 1/3: bump version, add triple");
    assert_eq!(one.state, PrState::Open);
    assert_eq!(one.author.login, "Lovelace");
    assert_eq!(
        (one.base.name.as_str(), one.head.name.as_str()),
        ("main", "stack-1")
    );
    assert_eq!(one.head.repo, Some(repo()));
    assert_eq!(one.base.sha.as_str().len(), 40);
    assert_eq!(one.head.sha.as_str().len(), 40);
    assert_eq!(one.review_decision, ReviewDecision::None);
    assert!(one.reviewers.is_empty());
    assert_eq!(one.ci, Some(CiState::None));
    assert_eq!(
        one.web_url,
        "https://github.com/Lovelace/review-sandbox/pull/1"
    );
    assert!(one.merge_base.is_none());
    assert!(one.diff_stat.is_some_and(|stat| stat.changed_files == 2));

    let two = get(2).await.unwrap();
    assert_eq!(two.base.name, "stack-1");
    let three = get(3).await.unwrap();
    assert_eq!(three.base.name, "stack-2");

    let big = get(5).await.unwrap();
    assert_eq!(big.diff_stat.map(|stat| stat.changed_files), Some(27));

    let merged = get(6).await.unwrap();
    assert_eq!(merged.state, PrState::Merged);
    let closed = get(7).await.unwrap();
    assert_eq!(closed.state, PrState::Closed);
    let draft = get(8).await.unwrap();
    assert_eq!(draft.state, PrState::Draft);
    assert_eq!(draft.head.name, "draft-wip");

    assert!(matches!(get(9999).await, Err(Error::NotFound { .. })));
    github.done();
}

#[tokio::test]
async fn no_ci_means_no_checks() {
    let sandbox = repo();
    let github = Fixture::open("checks").await;
    assert!(
        github
            .checks(&sandbox, PrNumber(1))
            .await
            .unwrap()
            .is_empty()
    );
    github.done();
}

#[tokio::test]
async fn the_stack_follows_base_branches() {
    let sandbox = repo();
    let github = Fixture::open("stack").await;
    let numbers_of = async |n: u64| {
        let stack = github.stack(&sandbox, PrNumber(n)).await.unwrap();
        assert_eq!(stack.source, StackSource::BaseBranch);
        let numbers: Vec<u64> = stack.links.iter().map(|link| link.number.0).collect();
        (stack, numbers)
    };
    for n in [1, 2, 3] {
        let (stack, numbers) = numbers_of(n).await;
        assert_eq!(numbers, [1, 2, 3], "stack seen from #{n}");
        assert_eq!(stack.position_of(PrNumber(n)), Some(n as usize - 1));
    }
    let (stack, _) = numbers_of(2).await;
    let heads: Vec<&str> = stack.links.iter().map(|l| l.head.as_str()).collect();
    let bases: Vec<&str> = stack.links.iter().map(|l| l.base.as_str()).collect();
    assert_eq!(heads, ["stack-1", "stack-2", "stack-3"]);
    assert_eq!(bases, ["main", "stack-1", "stack-2"]);
    assert!(stack.links.iter().all(|l| l.state == PrState::Open));

    let (alone, numbers) = numbers_of(4).await;
    assert_eq!(numbers, [4]);
    assert_eq!(alone.position_of(PrNumber(4)), Some(0));
    github.done();
}

fn thread_at(threads: &[Thread], line: u32, side: Side) -> &Thread {
    threads
        .iter()
        .find(|thread| {
            thread
                .anchor
                .as_ref()
                .and_then(|a| a.best_position())
                .is_some_and(|p| p.line.line == line && p.line.side == side)
        })
        .unwrap_or_else(|| panic!("no thread at {side:?} line {line}"))
}

fn check_pr4_threads(threads: &[Thread]) {
    assert_eq!(threads.len(), 5);
    assert!(threads.iter().all(|t| !t.comments.is_empty()));
    assert!(
        threads
            .windows(2)
            .all(|w| w[0].comments[0].created_at <= w[1].comments[0].created_at)
    );
    for thread in threads {
        let anchor = thread.anchor.as_ref().unwrap();
        assert_eq!(anchor.path, "src/threads_target.rs");
        assert!(anchor.old_path.is_none());
    }

    let resolved = thread_at(threads, 8, Side::New);
    assert!(resolved.is_resolved());
    assert!(!resolved.outdated);
    assert!(matches!(
        &resolved.resolution,
        Resolution::Resolved { by: Some(by) } if by.login == "Lovelace"
    ));
    assert_eq!(resolved.comments.len(), 1);
    assert!(
        resolved.comments[0]
            .body
            .starts_with("Should this saturate")
    );
    assert_eq!(resolved.comments[0].author.login, "Lovelace");
    assert_eq!(resolved.comments[0].verdict, None);
    assert!(resolved.comments[0].review.is_some());
    assert!(
        resolved.comments[0]
            .web_url
            .as_deref()
            .is_some_and(|u| u.contains("#discussion_r"))
    );
    let position = resolved.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert!(position.start.is_none());
    assert_eq!(position.commit.as_ref().map(|c| c.as_str().len()), Some(40));

    let replied = thread_at(threads, 16, Side::New);
    assert_eq!(replied.resolution, Resolution::Unresolved);
    assert_eq!(replied.comments.len(), 2);
    assert!(replied.comments[0].body.contains("add a test for `b == 0`"));
    assert!(replied.comments[1].body.starts_with("Will do"));
    assert!(replied.anchor.as_ref().unwrap().original.is_none());

    let multi = thread_at(threads, 31, Side::New);
    let range = multi.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert_eq!(range.start.map(|s| (s.side, s.line)), Some((Side::New, 29)));
    assert_eq!(multi.resolution, Resolution::Unresolved);

    let removed = thread_at(threads, 8, Side::Old);
    assert_eq!(removed.comments[0].body, "Why drop the plain subtraction?");
    assert!(!removed.outdated);

    let outdated = thread_at(threads, 24, Side::New);
    assert!(outdated.outdated);
    assert_eq!(outdated.resolution, Resolution::Unresolved);
    let anchor = outdated.anchor.as_ref().unwrap();
    assert!(anchor.position.is_none());
    let original = anchor.original.as_ref().unwrap();
    assert_eq!((original.line.side, original.line.line), (Side::New, 24));
    assert_eq!(original.commit.as_ref().map(|c| c.as_str().len()), Some(40));
    assert!(outdated.comments[0].body.contains("% 2"));
}

#[tokio::test]
async fn pr4_threads() {
    let sandbox = repo();
    let github = Fixture::open("threads").await;
    let threads = github.threads(&sandbox, PrNumber(4)).await.unwrap();
    check_pr4_threads(&threads);
    github.done();
}

#[tokio::test]
async fn a_snapshot_equals_the_separate_reads() {
    let sandbox = repo();
    let snapshot_source = Fixture::open("snapshot").await;
    let snapshot = snapshot_source
        .snapshot(&sandbox, PrNumber(4))
        .await
        .unwrap();
    snapshot_source.done();

    let separate = Fixture::open("separate").await;
    let (pull_request, threads, checks) = tokio::try_join!(
        separate.pull_request(&sandbox, PrNumber(4)),
        separate.threads(&sandbox, PrNumber(4)),
        separate.checks(&sandbox, PrNumber(4)),
    )
    .unwrap();
    separate.done();

    assert_eq!(snapshot.pull_request, pull_request);
    assert_eq!(snapshot.threads, threads);
    assert_eq!(snapshot.checks, checks);
    assert_eq!(snapshot.pull_request.number, PrNumber(4));
    check_pr4_threads(&snapshot.threads);
}

#[tokio::test]
async fn the_big_pull_request_lists_its_27_files() {
    let sandbox = repo();
    let github = Fixture::open("files").await;
    let files = github.viewed_files(&sandbox, PrNumber(5)).await.unwrap();
    assert_eq!(files.len(), 27);
    assert!(files.iter().all(|f| f.state == ViewedState::Unviewed));
    let has = |path: &str| files.iter().any(|f| f.path == path);
    assert!(has("docs/new_name.md"), "the rename shows its new path");
    assert!(!has("docs/old_name.md"));
    assert!(has("docs/obsolete.md"), "a deletion keeps its path");
    assert!(has("scripts/run.sh"), "the mode change");
    assert!(has("assets/logo.png"));
    assert!(has("data/crlf.txt"));
    assert!(has("data/no_newline.txt"));
    let mut paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    paths.sort_unstable();
    paths.dedup();
    assert_eq!(paths.len(), 27);
    github.done();
}

// ---- scripted responses ---------------------------------------------------

/// Answers each GraphQL request from a function of (operation, variables).
type Answer = Box<dyn Fn(&str, &Value) -> Value + Send + Sync>;

struct Script(Answer);

#[async_trait]
impl Transport for Script {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        let body = request.body.as_ref().unwrap();
        let operation = body["operationName"].as_str().unwrap();
        let data = (self.0)(operation, &body["variables"]);
        Ok(HttpResponse {
            status: 200,
            headers: Vec::new(),
            body: serde_json::to_vec(&json!({"data": data})).unwrap(),
            cached: false,
        })
    }
}

pub(super) async fn scripted(
    answer: impl Fn(&str, &Value) -> Value + Send + Sync + 'static,
) -> GitHub {
    GitHub::connect(client(Arc::new(Script(Box::new(answer)))))
        .await
        .unwrap()
}

pub(super) fn page(nodes: Vec<Value>, next: Option<&str>) -> Value {
    json!({
        "pageInfo": {"hasNextPage": next.is_some(), "endCursor": next},
        "nodes": nodes,
    })
}

pub(super) fn on_pr(fields: Value) -> Value {
    json!({"repository": {"pullRequest": fields}})
}

fn actor(login: &str) -> Value {
    json!({"__typename": "User", "login": login, "avatarUrl": null, "id": format!("U_{login}"), "name": null})
}

fn review_comment(id: &str, minute: u32, review: &str, state: &str) -> Value {
    json!({
        "id": id, "author": actor("mira"), "body": format!("body of {id}"),
        "createdAt": format!("2026-11-16T12:{minute:02}:00Z"), "lastEditedAt": null, "url": null,
        "originalCommit": {"oid": "a".repeat(40)}, "commit": {"oid": "b".repeat(40)},
        "pullRequestReview": {"id": review, "state": state},
    })
}

fn review_thread(id: &str, comments: Value) -> Value {
    json!({
        "id": id, "isResolved": false, "isOutdated": false, "resolvedBy": null,
        "path": "src/lib.rs", "line": 5, "startLine": null, "originalLine": 5,
        "originalStartLine": null, "diffSide": "RIGHT", "startDiffSide": null,
        "comments": comments,
    })
}

#[tokio::test]
async fn threads_page_to_the_end_and_include_the_conversation() {
    let sandbox = repo();
    let github = scripted(|operation, variables| match (operation, variables["after"].as_str()) {
        ("PullRequestThreads", _) => on_pr(json!({
            "comments": page(vec![json!({
                "id": "IC_1", "author": null, "body": "ping", "createdAt": "2026-11-16T12:30:00Z",
                "lastEditedAt": "2026-11-16T12:31:00Z", "url": "https://x/ic1",
            })], Some("c1")),
            "reviews": page(vec![
                json!({"id": "R_1", "state": "APPROVED", "body": "Looks good", "author": actor("mira"),
                       "createdAt": "2026-11-16T12:10:00Z", "lastEditedAt": null, "url": null}),
                json!({"id": "R_2", "state": "COMMENTED", "body": "", "author": actor("mira"),
                       "createdAt": "2026-11-16T12:11:00Z", "lastEditedAt": null, "url": null}),
                json!({"id": "R_3", "state": "PENDING", "body": "draft", "author": actor("me"),
                       "createdAt": "2026-11-16T12:12:00Z", "lastEditedAt": null, "url": null}),
                json!({"id": "R_4", "state": "CHANGES_REQUESTED", "body": "", "author": actor("kit"),
                       "createdAt": "2026-11-16T12:13:00Z", "lastEditedAt": null, "url": null}),
                json!({"id": "R_5", "state": "DISMISSED", "body": " ", "author": actor("kit"),
                       "createdAt": "2026-11-16T12:14:00Z", "lastEditedAt": null, "url": null}),
                json!({"id": "R_6", "state": "APPROVED", "body": "", "author": actor("kit"),
                       "createdAt": "2026-11-16T12:15:00Z", "lastEditedAt": null, "url": null}),
            ], None),
            "reviewThreads": page(vec![
                review_thread("T_1", page(vec![review_comment("C_1", 1, "R_2", "COMMENTED")], Some("k1"))),
            ], Some("t1")),
        })),
        ("IssueCommentsPage", Some("c1")) => on_pr(json!({"comments": page(vec![json!({
            "id": "IC_2", "author": actor("mira"), "body": "pong", "createdAt": "2026-11-16T12:40:00Z",
            "lastEditedAt": null, "url": null,
        })], None)})),
        ("ReviewThreadsPage", Some("t1")) => on_pr(json!({"reviewThreads": page(vec![
            review_thread("T_2", page(vec![review_comment("C_3", 20, "R_9", "PENDING")], None)),
            review_thread("T_3", page(vec![review_comment("C_4", 21, "R_8", "COMMENTED"),
                                           review_comment("C_5", 22, "R_9", "PENDING")], None)),
        ], None)})),
        ("ThreadCommentsPage", Some("k1")) => {
            assert_eq!(variables["id"], "T_1");
            json!({"node": {"comments": page(vec![review_comment("C_2", 2, "R_7", "COMMENTED")], None)}})
        }
        other => panic!("unexpected request {other:?}"),
    })
    .await;
    let threads = github.threads(&sandbox, PrNumber(1)).await.unwrap();
    let shape: Vec<(&str, usize)> = threads
        .iter()
        .map(|t| (t.id.as_str(), t.comments.len()))
        .collect();
    // R_1 (12:10), T_1 (12:01) first by time; the pending comment is dropped,
    // the thread holding only a pending comment is dropped with it. Verdicts
    // without text stay; the empty COMMENTED review R_2 does not.
    assert_eq!(
        shape,
        [
            ("T_1", 2),
            ("R_1", 1),
            ("R_4", 1),
            ("R_5", 1),
            ("R_6", 1),
            ("T_3", 1),
            ("IC_1", 1),
            ("IC_2", 1)
        ]
    );
    let verdicts: Vec<Option<ReviewState>> = threads[2..5]
        .iter()
        .map(|t| t.comments[0].verdict)
        .collect();
    assert_eq!(
        verdicts,
        [
            Some(ReviewState::ChangesRequested),
            Some(ReviewState::Dismissed),
            Some(ReviewState::Approved)
        ]
    );
    assert!(threads[2..5].iter().all(|t| t.anchor.is_none()));
    let review = &threads[1];
    assert!(review.anchor.is_none());
    assert_eq!(review.resolution, Resolution::NotResolvable);
    assert_eq!(review.comments[0].verdict, Some(ReviewState::Approved));
    assert_eq!(
        review.comments[0].review.as_ref().map(|r| r.as_str()),
        Some("R_1")
    );
    let ghost = &threads[6].comments[0];
    assert_eq!(ghost.author.login, "ghost");
    assert!(ghost.edited_at.is_some());
}

fn run_json(id: &str, database_id: u64, name: &str, workflow: Option<(&str, u64)>) -> Value {
    json!({
        "__typename": "CheckRun", "id": id, "databaseId": database_id, "name": name,
        "status": "COMPLETED", "conclusion": "SUCCESS", "detailsUrl": format!("https://x/{id}"),
        "startedAt": "2026-11-16T12:00:00Z", "completedAt": "2026-11-16T12:05:00Z",
        "steps": page(vec![json!({"number": 1, "name": "Set up job", "status": "COMPLETED",
            "conclusion": "SUCCESS", "startedAt": null, "completedAt": null})], Some("s1")),
        "checkSuite": {"workflowRun": workflow.map(|(name, run)| json!({
            "databaseId": run, "url": "https://x/run", "workflow": {"name": name}}))},
    })
}

fn rollup(contexts: Value) -> Value {
    json!({"commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE", "contexts": contexts}}}]}})
}

#[tokio::test]
async fn checks_map_runs_steps_and_statuses() {
    let sandbox = repo();
    let github = scripted(|operation, variables| match (operation, variables["after"].as_str()) {
        ("PullRequestChecks", _) => on_pr(rollup(page(
            vec![
                run_json("CR_1", 133_349_881_449, "build", Some(("CI", 900))),
                json!({"__typename": "StatusContext", "id": "SC_1", "context": "deploy/preview",
                       "state": "PENDING", "targetUrl": "https://x/deploy"}),
            ],
            Some("p1"),
        ))),
        ("ChecksPage", Some("p1")) => on_pr(rollup(page(
            vec![
                // The same job re-run: the newer attempt replaces it.
                run_json("CR_2", 133_349_881_500, "build", Some(("CI", 901))),
                {
                    let mut app = run_json("CR_3", 7, "lint", None);
                    app["conclusion"] = json!("FAILURE");
                    app
                },
                {
                    let mut running = run_json("CR_4", 8, "e2e", Some(("CI", 900)));
                    running["status"] = json!("IN_PROGRESS");
                    running["conclusion"] = Value::Null;
                    running
                },
            ],
            None,
        ))),
        ("CheckStepsPage", Some("s1")) => {
            let step = |number: u32, conclusion: &str| json!({"number": number, "name": format!("step {number}"),
                "status": "COMPLETED", "conclusion": conclusion, "startedAt": null, "completedAt": null});
            json!({"node": {"steps": page(vec![step(2, "FAILURE")], None)}})
        }
        other => panic!("unexpected request {other:?}"),
    })
    .await;
    let checks = github.checks(&sandbox, PrNumber(1)).await.unwrap();
    let rows: Vec<(&str, Option<&str>, CiState)> = checks
        .iter()
        .map(|c| (c.name.as_str(), c.group.as_deref(), c.state))
        .collect();
    assert_eq!(
        rows,
        [
            ("build", Some("CI"), CiState::Pass),
            ("deploy/preview", None, CiState::Queued),
            ("lint", None, CiState::Fail),
            ("e2e", Some("CI"), CiState::Running),
        ]
    );
    let build = &checks[0];
    assert_eq!(build.id.as_str(), "CR_2");
    assert_eq!(build.job.as_ref().map(|j| j.as_str()), Some("133349881500"));
    assert_eq!(build.run.as_ref().map(|r| r.as_str()), Some("901"));
    let steps: Vec<(u32, CiState)> = build.steps.iter().map(|s| (s.number, s.state)).collect();
    assert_eq!(steps, [(1, CiState::Pass), (2, CiState::Fail)]);
    assert!(checks[1].job.is_none() && checks[1].run.is_none());
    assert!(
        checks[2].job.is_none(),
        "a non-Actions check app has no job id"
    );
}

#[tokio::test]
async fn viewed_files_page_and_map_states() {
    let sandbox = repo();
    let github = scripted(|operation, variables| {
        assert_eq!(operation, "PullRequestFiles");
        let file = |path: &str, state: &str| json!({"path": path, "viewerViewedState": state});
        match variables["after"].as_str() {
            None => on_pr(json!({"files": page(vec![file("a.rs", "VIEWED"), file("b.rs", "UNVIEWED")], Some("f1"))})),
            Some("f1") => on_pr(json!({"files": page(vec![file("c.rs", "DISMISSED")], None)})),
            Some(other) => panic!("cursor {other}"),
        }
    })
    .await;
    let files = github.viewed_files(&sandbox, PrNumber(1)).await.unwrap();
    let got: Vec<(&str, ViewedState)> = files.iter().map(|f| (f.path.as_str(), f.state)).collect();
    assert_eq!(
        got,
        [
            ("a.rs", ViewedState::Viewed),
            ("b.rs", ViewedState::Unviewed),
            ("c.rs", ViewedState::Dismissed),
        ]
    );
}

pub(super) fn pr_json(number: u64, head: &str, base: &str) -> Value {
    json!({
        "id": format!("PR_{number}"), "number": number, "title": format!("Change {number}"),
        "body": "", "author": actor("mira"), "state": "OPEN", "isDraft": false,
        "createdAt": "2026-11-16T12:00:00Z", "updatedAt": "2026-11-16T12:05:00Z",
        "baseRefName": base, "baseRefOid": "a".repeat(40), "headRefName": head, "headRefOid": "b".repeat(40),
        "headRepository": {"nameWithOwner": "Lovelace/review-sandbox"},
        "baseRepository": {"nameWithOwner": "Lovelace/review-sandbox"},
        "reviewDecision": null,
        "reviewRequests": {"nodes": [
            {"requestedReviewer": {"__typename": "User", "id": "U_1", "login": "mira", "name": "Mira", "avatarUrl": null}},
            {"requestedReviewer": {"__typename": "Team", "id": "T_1", "slug": "core", "name": "Core", "avatarUrl": null,
                                   "organization": {"login": "acme"}}},
        ]},
        "latestReviews": {"nodes": [{"state": "COMMENTED", "author": actor("mira")},
                                    {"state": "APPROVED", "author": actor("kit")}]},
        "labels": {"nodes": [{"name": "stack", "color": "ededed"}]},
        "additions": 3, "deletions": 1, "changedFiles": 2, "url": format!("https://x/pull/{number}"),
        "commits": {"nodes": [{"commit": {"oid": "b".repeat(40), "statusCheckRollup": {"state": "PENDING"}}}]},
    })
}

#[tokio::test]
async fn pull_request_reviewers_labels_and_rollup() {
    let sandbox = repo();
    let github = scripted(|_, _| on_pr(pr_json(7, "topic", "main"))).await;
    let pr = github.pull_request(&sandbox, PrNumber(7)).await.unwrap();
    let reviewers: Vec<(&str, ReviewState, AccountKind)> = pr
        .reviewers
        .iter()
        .map(|r| (r.user.login.as_str(), r.state, r.user.kind))
        .collect();
    assert_eq!(
        reviewers,
        [
            ("mira", ReviewState::Requested, AccountKind::User),
            ("kit", ReviewState::Approved, AccountKind::User),
            ("acme/core", ReviewState::Requested, AccountKind::Team),
        ]
    );
    assert_eq!(pr.review_decision, ReviewDecision::Approved);
    assert_eq!(pr.ci, Some(CiState::Running));
    assert_eq!(pr.labels[0].name, "stack");
}

#[tokio::test]
async fn a_native_stack_wins_over_the_base_branch_rule() {
    let sandbox = repo();
    let github = scripted(|operation, variables| match operation {
        "PullRequestStack" => {
            let mut node = pr_json(11, "feat-b", "feat-a");
            node["stack"] = json!({"id": "S_1", "entries": page(vec![json!({
                "position": 2,
                "pullRequest": {"number": 11, "title": "B", "state": "OPEN", "isDraft": true,
                                "headRefName": "feat-b", "baseRefName": "feat-a"},
            })], Some("e1"))});
            on_pr(node)
        }
        "StackEntriesPage" => {
            assert_eq!(
                (variables["id"].as_str(), variables["after"].as_str()),
                (Some("S_1"), Some("e1"))
            );
            json!({"node": {"entries": page(vec![
                json!({"position": 1, "pullRequest": {"number": 10, "title": "A", "state": "MERGED",
                       "isDraft": false, "headRefName": "feat-a", "baseRefName": "main"}}),
                json!({"position": 3, "pullRequest": null}),
            ], None)}})
        }
        other => panic!("unexpected {other}"),
    })
    .await;
    let stack = github.stack(&sandbox, PrNumber(11)).await.unwrap();
    assert_eq!(stack.source, StackSource::Native);
    let links: Vec<(u64, PrState)> = stack.links.iter().map(|l| (l.number.0, l.state)).collect();
    assert_eq!(links, [(10, PrState::Merged), (11, PrState::Draft)]);
}
