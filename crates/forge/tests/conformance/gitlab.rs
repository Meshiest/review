//! gitlab backend against recordings of `review-sandbox`.
//!
//! Record with `REVIEW_RECORD=1 cargo test -p forge --test conformance gitlab`
//! (live, with glab's token). The account also has work merge requests, so
//! assertions on the account-wide inbox sections look at the sandbox only.

use std::sync::Arc;
use std::time::Duration;

use forge::auth::{Credential, StaticCredential, Token};
use forge::gitlab::GitLab;
use forge::http::{HttpClient, HttpOptions, ManualClock, Method, Transport};
use forge::model::{
    CiState, DiffStat, ForgeKind, InboxSection, LineRef, PrNumber, PrState, PullRequest, RepoRef,
    Resolution, ReviewState, Side, StackSource, Thread,
};
use forge::record::{
    Cassette, Exchange, RECORD_ENV, RecordedBody, RecordedRequest, RecordedResponse,
    ReplayTransport, fixture_client, fixture_path,
};
use forge::{Error, Forge};
use serde_json::{Value, json};

const HOST: &str = "gitlab.com";

fn repo() -> RepoRef {
    RepoRef::new(HOST, "Lovelace", "review-sandbox")
}

fn recording() -> bool {
    std::env::var(RECORD_ENV).is_ok_and(|v| v == "1")
}

/// A backend over `gitlab/<name>.json`, and the replay to check at the end.
async fn backend(name: &str) -> (GitLab, Option<Arc<ReplayTransport>>) {
    if recording() {
        let http = fixture_client(ForgeKind::GitLab, HOST, name).unwrap();
        return (GitLab::connect(http).await.unwrap(), None);
    }
    let replay = Arc::new(ReplayTransport::load(&fixture_path(ForgeKind::GitLab, name)).unwrap());
    (replayed(replay.clone()).await, Some(replay))
}

async fn replayed(transport: Arc<dyn Transport>) -> GitLab {
    let options = HttpOptions {
        clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
        ..HttpOptions::for_kind(ForgeKind::GitLab)
    };
    let credentials = Arc::new(StaticCredential(Credential::bearer(Token::new("replay"))));
    let http = HttpClient::new(ForgeKind::GitLab, HOST, transport, credentials, options);
    GitLab::connect(http).await.unwrap()
}

/// Every recorded exchange was asked for.
fn finish(replay: Option<Arc<ReplayTransport>>) {
    if let Some(replay) = replay {
        assert_eq!(replay.unused(), Vec::<String>::new());
    }
}

fn in_sandbox(prs: Vec<PullRequest>) -> Vec<PullRequest> {
    prs.into_iter().filter(|pr| pr.repo == repo()).collect()
}

fn numbers(prs: &[PullRequest]) -> Vec<u64> {
    let mut numbers: Vec<u64> = prs.iter().map(|pr| pr.number.0).collect();
    numbers.sort_unstable();
    numbers
}

fn line(side: Side, line: u32) -> LineRef {
    LineRef { side, line }
}

#[tokio::test]
async fn whoami_is_the_signed_in_user() {
    let (gitlab, replay) = backend("whoami").await;
    let me = gitlab.whoami().await.unwrap();
    assert_eq!(me.login, "Lovelace");
    assert_eq!(me.id.unwrap().as_str(), "gid://gitlab/User/7654321");
    assert!(me.avatar_url.is_some());
    finish(replay);
}

#[tokio::test]
async fn inbox_sections_list_open_merge_requests() {
    let (gitlab, replay) = backend("inbox").await;

    let requested = gitlab.inbox(&InboxSection::ReviewRequested).await.unwrap();
    assert!(in_sandbox(requested).is_empty());

    let authored = in_sandbox(gitlab.inbox(&InboxSection::AuthoredByMe).await.unwrap());
    assert_eq!(numbers(&authored), [1, 2, 3, 4, 5, 8]);
    assert!(
        authored
            .windows(2)
            .all(|pair| pair[0].updated_at >= pair[1].updated_at),
        "most recently updated first"
    );
    assert!(authored.iter().all(|pr| pr.state.is_open()));

    let open = gitlab
        .inbox(&InboxSection::OpenInRepo(repo()))
        .await
        .unwrap();
    assert_eq!(numbers(&open), [1, 2, 3, 4, 5, 8]);
    let draft = open.iter().find(|pr| pr.number == PrNumber(8)).unwrap();
    assert_eq!(draft.state, PrState::Draft);
    assert!(open.iter().all(|pr| !pr.head.sha.as_str().is_empty()));

    // Inbox rows carry line counts and comment counts (6 notes in 5
    // discussions, 4 unresolved, on the threads PR).
    let row = |n: u64| open.iter().find(|pr| pr.number == PrNumber(n)).unwrap();
    assert_eq!(
        row(5).diff_stat,
        Some(DiffStat {
            additions: 72,
            deletions: 28,
            changed_files: 27
        })
    );
    assert_eq!(
        (row(4).comment_count, row(4).unresolved_threads),
        (Some(6), Some(4))
    );
    assert_eq!(
        (row(1).comment_count, row(1).unresolved_threads),
        (Some(0), Some(0))
    );

    // The free query is REST; its rows are loaded through GraphQL.
    let merged = gitlab
        .inbox(&InboxSection::Query(
            "scope=all&state=merged&source_branch=feature-merged".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(numbers(&merged), [6]);
    assert_eq!(merged[0].state, PrState::Merged);
    let closed = gitlab
        .inbox(&InboxSection::Query(
            "scope=all&state=closed&source_branch=feature-closed".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(numbers(&closed), [7]);
    assert_eq!(closed[0].state, PrState::Closed);
    finish(replay);
}

#[tokio::test]
async fn merge_requests_map_to_the_model() {
    let (gitlab, replay) = backend("pull_requests").await;
    let mut all = Vec::new();
    for number in 1..=8 {
        all.push(
            gitlab
                .pull_request(&repo(), PrNumber(number))
                .await
                .unwrap(),
        );
    }
    let expect = [
        (1, "stack-1", "main", PrState::Open),
        (2, "stack-2", "stack-1", PrState::Open),
        (3, "stack-3", "stack-2", PrState::Open),
        (4, "threads", "main", PrState::Open),
        (5, "big", "main", PrState::Open),
        (6, "feature-merged", "main", PrState::Merged),
        (7, "feature-closed", "main", PrState::Closed),
        (8, "draft-wip", "main", PrState::Draft),
    ];
    for (pr, (number, head, base, state)) in all.iter().zip(expect) {
        assert_eq!(pr.number, PrNumber(number));
        assert_eq!((pr.head.name.as_str(), pr.base.name.as_str()), (head, base));
        assert_eq!(pr.state, state, "!{number}");
        assert_eq!(pr.repo, repo());
        assert_eq!(pr.head.repo.as_ref(), Some(&repo()));
        assert_eq!(pr.author.login, "Lovelace");
        assert!(pr.id.as_str().starts_with("gid://gitlab/MergeRequest/"));
        assert_eq!(pr.head.sha.as_str().len(), 40);
        assert_eq!(pr.base.sha.as_str().len(), 40);
        assert_eq!(pr.merge_base.as_ref().unwrap().as_str().len(), 40);
        assert_eq!(pr.ci, Some(CiState::None), "the sandbox has no CI");
        assert!(pr.web_url.ends_with(&format!("/-/merge_requests/{number}")));
    }
    assert!(all[7].title.starts_with("Draft: "));

    let big = all[4].diff_stat.unwrap();
    assert_eq!(big.changed_files, 27);
    assert!(big.additions > 0 && big.deletions > 0);

    // Stack members sit on each other: !2 is based on !1's branch.
    assert_eq!(all[1].base.sha, all[0].head.sha);

    let missing = gitlab.pull_request(&repo(), PrNumber(99)).await;
    assert!(
        matches!(missing, Err(Error::NotFound { .. })),
        "{missing:?}"
    );
    finish(replay);
}

fn thread_with<'a>(threads: &'a [Thread], text: &str) -> &'a Thread {
    threads
        .iter()
        .find(|thread| thread.comments[0].body.contains(text))
        .unwrap_or_else(|| panic!("no thread starting with {text:?}"))
}

fn check_threads(threads: &[Thread]) {
    // Five inline threads; the "resolved all threads", "changed this line"
    // and "added 1 commit" system notes are gone.
    assert_eq!(threads.len(), 5, "{threads:#?}");
    assert!(threads.iter().all(|t| {
        t.anchor
            .as_ref()
            .is_some_and(|a| a.path == "src/threads_target.rs")
    }));
    assert!(threads.iter().all(|t| t.comments.iter().all(|c| {
        !c.body.contains("changed this line")
            && c.web_url.as_deref().is_some_and(|u| u.contains("#note_"))
    })));

    let resolved = thread_with(threads, "saturate or wrap");
    let Resolution::Resolved { by } = &resolved.resolution else {
        panic!("not resolved: {:?}", resolved.resolution);
    };
    assert_eq!(by.as_ref().unwrap().login, "Lovelace");
    assert!(!resolved.outdated);
    let position = resolved.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert_eq!(position.line, line(Side::New, 8));
    assert_eq!(position.start, None);
    assert_eq!(position.commit.as_ref().unwrap().as_str().len(), 40);

    let reply = thread_with(threads, "Nice simplification");
    assert_eq!(reply.resolution, Resolution::Unresolved);
    assert_eq!(reply.comments.len(), 2);
    assert!(reply.comments[1].body.contains("Will do"));
    assert!(reply.comments[0].created_at <= reply.comments[1].created_at);
    let position = reply.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert_eq!(position.line, line(Side::New, 16));

    let multi = thread_with(threads, "overflow for large exponents");
    let position = multi.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert_eq!(position.start, Some(line(Side::New, 29)));
    assert_eq!(position.line, line(Side::New, 31));

    let removed = thread_with(threads, "plain subtraction");
    let position = removed.anchor.as_ref().unwrap().position.as_ref().unwrap();
    assert_eq!(position.line, line(Side::Old, 8));

    let outdated = thread_with(threads, "% 2");
    assert!(outdated.outdated);
    let anchor = outdated.anchor.as_ref().unwrap();
    assert_eq!(anchor.position, None);
    let original = anchor.original.as_ref().unwrap();
    assert_eq!(original.line, line(Side::New, 24));
    assert!(original.commit.is_some());
    assert!(threads.iter().filter(|t| t.outdated).count() == 1);

    assert!(
        threads
            .windows(2)
            .all(|pair| pair[0].comments[0].created_at <= pair[1].comments[0].created_at),
        "oldest first"
    );
}

#[tokio::test]
async fn threads_carry_positions_resolution_and_outdated() {
    let (gitlab, replay) = backend("threads").await;
    let threads = gitlab.threads(&repo(), PrNumber(4)).await.unwrap();
    check_threads(&threads);

    // No discussion on a stacked merge request.
    assert!(
        gitlab
            .threads(&repo(), PrNumber(1))
            .await
            .unwrap()
            .is_empty()
    );
    finish(replay);
}

#[tokio::test]
async fn checks_are_empty_without_a_pipeline() {
    let (gitlab, replay) = backend("checks").await;
    assert!(
        gitlab
            .checks(&repo(), PrNumber(4))
            .await
            .unwrap()
            .is_empty()
    );
    finish(replay);
}

#[tokio::test]
async fn stacks_use_the_native_field_then_the_base_branch_rule() {
    let (gitlab, replay) = backend("stack").await;
    for number in [1, 2, 3] {
        let stack = gitlab.stack(&repo(), PrNumber(number)).await.unwrap();
        assert_eq!(stack.source, StackSource::Native);
        let linked: Vec<(u64, &str, &str)> = stack
            .links
            .iter()
            .map(|l| (l.number.0, l.head.as_str(), l.base.as_str()))
            .collect();
        assert_eq!(
            linked,
            [
                (1, "stack-1", "main"),
                (2, "stack-2", "stack-1"),
                (3, "stack-3", "stack-2"),
            ]
        );
        assert_eq!(
            stack.position_of(PrNumber(number)),
            Some(number as usize - 1)
        );
    }

    let alone = gitlab.stack(&repo(), PrNumber(4)).await.unwrap();
    assert_eq!(alone.source, StackSource::BaseBranch);
    assert_eq!(alone.links.len(), 1);
    assert_eq!(alone.links[0].number, PrNumber(4));
    finish(replay);
}

#[tokio::test]
async fn snapshot_is_one_query_plus_the_discussions() {
    let (gitlab, replay) = backend("snapshot").await;
    let snapshot = gitlab.snapshot(&repo(), PrNumber(4)).await.unwrap();
    assert_eq!(snapshot.pull_request.number, PrNumber(4));
    assert_eq!(snapshot.pull_request.state, PrState::Open);
    assert!(snapshot.checks.is_empty());
    check_threads(&snapshot.threads);
    finish(replay);
}

// ---------------------------------------------------------------------------
// Hand-built cassettes for what the sandbox cannot show: it has no CI and
// no approvals. These pin the request shapes and the mapping.

fn graphql_exchange(operation: &str, variables: Value, data: Value) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method: Method::Post,
            path: "/api/graphql".to_owned(),
            query: Vec::new(),
            operation: Some(operation.to_owned()),
            variables: Some(variables),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: RecordedBody::Json(json!({"data": data})),
        },
    }
}

fn rest_exchange(path: &str, query: &[(&str, &str)], next: Option<&str>, body: Value) -> Exchange {
    let mut query: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    query.sort();
    Exchange {
        request: RecordedRequest {
            method: Method::Get,
            path: path.to_owned(),
            query,
            operation: None,
            variables: None,
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: next
                .map(|page| ("x-next-page".to_owned(), page.to_owned()))
                .into_iter()
                .collect(),
            body: RecordedBody::Json(body),
        },
    }
}

#[tokio::test]
async fn a_pipeline_larger_than_a_page_is_read_from_rest() {
    let truncated = json!({"project": {"mergeRequest": {"headPipeline": {
        "id": "gid://gitlab/Ci::Pipeline/900",
        "status": "RUNNING",
        "stages": {"pageInfo": {"hasNextPage": false}, "nodes": [{
            "name": "test",
            "jobs": {"pageInfo": {"hasNextPage": true}, "nodes": []},
        }]},
    }}}});
    let job = |id: u64, name: &str, status: &str| {
        json!({"id": id, "name": name, "status": status, "stage": "test",
               "web_url": format!("https://gitlab.com/o/r/-/jobs/{id}"),
               "started_at": "2026-11-17T05:00:00Z", "finished_at": null})
    };
    let jobs_path = "/api/v4/projects/Lovelace%2Freview-sandbox/pipelines/900/jobs";
    let cassette = Cassette {
        exchanges: vec![
            graphql_exchange(
                "Checks",
                json!({"path": "Lovelace/review-sandbox", "iid": "4"}),
                truncated,
            ),
            rest_exchange(
                jobs_path,
                &[("per_page", "100")],
                Some("2"),
                json!([job(1, "build", "success"), job(2, "lint", "failed")]),
            ),
            rest_exchange(
                jobs_path,
                &[("page", "2"), ("per_page", "100")],
                None,
                json!([job(3, "e2e", "running"), job(4, "deploy", "manual")]),
            ),
        ],
    };
    let replay = Arc::new(ReplayTransport::new("inline", cassette));
    let gitlab = replayed(replay.clone()).await;

    let checks = gitlab.checks(&repo(), PrNumber(4)).await.unwrap();
    let seen: Vec<(&str, CiState)> = checks.iter().map(|c| (c.name.as_str(), c.state)).collect();
    assert_eq!(
        seen,
        [
            ("build", CiState::Pass),
            ("lint", CiState::Fail),
            ("e2e", CiState::Running),
            ("deploy", CiState::Skipped),
        ]
    );
    let build = &checks[0];
    assert_eq!(build.id.as_str(), "gid://gitlab/Ci::Build/1");
    assert_eq!(build.job.as_ref().unwrap().as_str(), "1");
    assert_eq!(build.run.as_ref().unwrap().as_str(), "900");
    assert_eq!(build.group.as_deref(), Some("test"));
    assert!(build.steps.is_empty());
    assert!(build.started_at.is_some() && build.completed_at.is_none());
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[tokio::test]
async fn graphql_jobs_map_to_checks() {
    let pipeline = json!({"project": {"mergeRequest": {"headPipeline": {
        "id": "gid://gitlab/Ci::Pipeline/901",
        "status": "FAILED",
        "stages": {"pageInfo": {"hasNextPage": false}, "nodes": [
            {"name": "build", "jobs": {"pageInfo": {"hasNextPage": false}, "nodes": [
                {"id": "gid://gitlab/Ci::Build/10", "name": "compile", "status": "SUCCESS",
                 "webPath": "/o/r/-/jobs/10", "startedAt": "2026-11-17T05:00:00Z",
                 "finishedAt": "2026-11-17T05:01:00Z"},
            ]}},
            {"name": "test", "jobs": {"pageInfo": {"hasNextPage": false}, "nodes": [
                {"id": "gid://gitlab/Ci::Build/11", "name": "unit", "status": "FAILED",
                 "webPath": "/o/r/-/jobs/11", "startedAt": null, "finishedAt": null},
                {"id": "gid://gitlab/Ci::Build/12", "name": "slow", "status": "CREATED",
                 "webPath": null, "startedAt": null, "finishedAt": null},
            ]}},
        ]},
    }}}});
    let cassette = Cassette {
        exchanges: vec![graphql_exchange(
            "Checks",
            json!({"path": "Lovelace/review-sandbox", "iid": "4"}),
            pipeline,
        )],
    };
    let replay = Arc::new(ReplayTransport::new("inline", cassette));
    let gitlab = replayed(replay.clone()).await;
    let checks = gitlab.checks(&repo(), PrNumber(4)).await.unwrap();
    let seen: Vec<(&str, &str, CiState)> = checks
        .iter()
        .map(|c| (c.name.as_str(), c.group.as_deref().unwrap(), c.state))
        .collect();
    assert_eq!(
        seen,
        [
            ("compile", "build", CiState::Pass),
            ("unit", "test", CiState::Fail),
            ("slow", "test", CiState::Queued),
        ]
    );
    assert_eq!(
        checks[0].url.as_deref(),
        Some("https://gitlab.com/o/r/-/jobs/10")
    );
    assert_eq!(checks[2].url, None);
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[tokio::test]
async fn ci_states_read_a_page_of_merge_requests_in_one_query() {
    let node = |iid: &str, status: Option<&str>| json!({"iid": iid, "headPipeline": status.map(|status| json!({"status": status}))});
    let asked = ["4", "5", "6", "7", "8", "9", "10", "11"];
    // Unordered, and !7 is gone: the project no longer has it.
    let data = json!({"project": {"mergeRequests": {"nodes": [
        node("11", Some("SUCCESS")),
        node("4", Some("CREATED")),
        node("5", Some("MANUAL")),
        node("6", None),
        node("8", Some("SCHEDULED")),
        node("9", Some("WAITING_FOR_RESOURCE")),
        node("10", Some("PREPARING")),
    ]}}});
    let cassette = Cassette {
        exchanges: vec![graphql_exchange(
            "CiStates",
            json!({"path": "Lovelace/review-sandbox", "iids": asked, "first": asked.len()}),
            data,
        )],
    };
    let replay = Arc::new(ReplayTransport::new("inline", cassette));
    let gitlab = replayed(replay.clone()).await;
    let numbers: Vec<PrNumber> = asked
        .iter()
        .map(|iid| PrNumber(iid.parse().unwrap()))
        .collect();
    let states = gitlab.ci_states(&repo(), &numbers).await.unwrap();
    assert_eq!(
        states,
        [
            CiState::Queued,
            CiState::Skipped,
            CiState::None,
            CiState::None,
            CiState::Queued,
            CiState::Queued,
            CiState::Queued,
            CiState::Pass,
        ],
        "a manual pipeline, a deleted one and a missing merge request all settle"
    );
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[tokio::test]
async fn approvals_become_verdict_threads() {
    let note = |id: u64, body: &str, at: &str| {
        json!({"id": id, "body": body, "system": true, "created_at": at,
               "author": {"id": 5, "username": "mira"}})
    };
    let discussion =
        |id: &str, notes: Vec<Value>| json!({"id": id, "individual_note": true, "notes": notes});
    let plain = json!({"id": "plain", "individual_note": true, "notes": [{
        "id": 3, "body": "Looks good overall.", "system": false,
        "created_at": "2026-11-17T05:01:00Z", "resolvable": false,
        "author": {"id": 5, "username": "mira"},
    }]});
    let discussions = json!([
        discussion(
            "a",
            vec![note(
                1,
                "approved this merge request",
                "2026-11-17T05:00:00Z"
            )]
        ),
        plain,
        discussion(
            "u",
            vec![note(
                2,
                "unapproved this merge request",
                "2026-11-17T05:02:00Z"
            )]
        ),
        discussion(
            "c",
            vec![note(4, "requested changes", "2026-11-17T05:03:00Z")]
        ),
        discussion(
            "n",
            vec![note(5, "changed the description", "2026-11-17T05:04:00Z")]
        ),
    ]);
    let head = json!({"project": {"mergeRequest": {
        "webUrl": "https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/4",
        "diffHeadSha": "b".repeat(40),
        "diffRefs": {"headSha": "b".repeat(40)},
    }}});
    let cassette = Cassette {
        exchanges: vec![
            graphql_exchange(
                "ThreadsHead",
                json!({"path": "Lovelace/review-sandbox", "iid": "4"}),
                head,
            ),
            rest_exchange(
                "/api/v4/projects/Lovelace%2Freview-sandbox/merge_requests/4/discussions",
                &[("per_page", "100")],
                None,
                discussions,
            ),
        ],
    };
    let replay = Arc::new(ReplayTransport::new("inline", cassette));
    let gitlab = replayed(replay.clone()).await;
    let threads = gitlab.threads(&repo(), PrNumber(4)).await.unwrap();

    let verdicts: Vec<Option<ReviewState>> =
        threads.iter().map(|t| t.comments[0].verdict).collect();
    assert_eq!(
        verdicts,
        [
            Some(ReviewState::Approved),
            None,
            Some(ReviewState::Dismissed),
            Some(ReviewState::ChangesRequested),
        ]
    );
    assert!(threads.iter().all(|t| t.anchor.is_none() && !t.outdated));
    assert!(
        threads
            .iter()
            .all(|t| t.resolution == Resolution::NotResolvable)
    );
    assert_eq!(threads[0].comments[0].body, "");
    assert_eq!(threads[1].comments[0].body, "Looks good overall.");
    assert_eq!(replay.unused(), Vec::<String>::new());
}
