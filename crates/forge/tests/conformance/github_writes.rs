//! GitHub writes against a recording of one live round trip
//! (`tests/fixtures/github/writes.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example round_trip --
//! --forge github` (it opens and closes a scratch pull request on
//! `review-sandbox`). These tests only replay: `ReplayTransport` has no
//! network, and a request without a recording is a `ReplayMiss`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge::github::GitHub;
use forge::http::{HttpRequest, HttpResponse, Method, RetryMode, Transport};
use forge::model::{
    CommentAnchor, ForgeKind, JobId, LineRef, PendingPlace, PrNumber, RepoRef, ReviewId,
    ReviewState, RunId, Side, ThreadId, Verdict, ViewedState,
};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
    fixture_path,
};
use forge::{Error, Forge};
use serde_json::{Value, json};

use super::common::repo;
use super::github::client;
use super::round_trip_steps::{self, Status};

fn cassette() -> Cassette {
    let path = fixture_path(ForgeKind::GitHub, "writes");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The scratch pull request the recording was made on.
fn recorded_number(cassette: &Cassette) -> PrNumber {
    let number = cassette
        .exchanges
        .iter()
        .find_map(|e| e.request.variables.as_ref()?.get("number")?.as_u64())
        .expect("the recording reads a pull request");
    PrNumber(number)
}

/// The variables of every recorded request of one operation, in order.
fn sent(cassette: &Cassette, operation: &str) -> Vec<Value> {
    cassette
        .exchanges
        .iter()
        .filter(|e| e.request.operation.as_deref() == Some(operation))
        .filter_map(|e| e.request.variables.clone())
        .collect()
}

async fn replay() -> (GitHub, Arc<ReplayTransport>) {
    let path = fixture_path(ForgeKind::GitHub, "writes");
    let replay = Arc::new(ReplayTransport::load(&path).unwrap());
    let github = GitHub::connect(client(replay.clone())).await.unwrap();
    (github, replay)
}

#[tokio::test]
async fn the_round_trip_replays_with_every_step_passing() {
    let (github, replay) = replay().await;
    let number = recorded_number(&cassette());
    let outcomes = round_trip_steps::run(&github, &repo(), number, |_| {}).await;
    let lines: Vec<String> = outcomes.iter().map(ToString::to_string).collect();
    assert!(outcomes.iter().all(|o| !o.failed()), "{lines:#?}");
    let skipped: Vec<_> = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Skip(_)))
        .map(|o| o.step)
        .collect();
    assert_eq!(skipped, ["approve", "request-changes"], "{lines:#?}");
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[test]
fn writes_are_the_named_mutations_in_round_trip_order() {
    let cassette = cassette();
    let mutations: Vec<&str> = cassette
        .exchanges
        .iter()
        .filter_map(|e| e.request.operation.as_deref())
        .filter(|op| {
            !matches!(
                *op,
                "PullRequest"
                    | "PullRequestId"
                    | "PendingReview"
                    | "PullRequestThreads"
                    | "PullRequestFiles"
            )
        })
        .collect();
    assert_eq!(
        mutations,
        [
            "AddPullRequestReview",
            "AddPullRequestReviewThread",
            "AddPullRequestReviewThread",
            "DeletePullRequestReviewComment",
            "DeletePullRequestReview",
            "AddPullRequestReview",
            "AddPullRequestReviewThread",
            "AddPullRequestReviewThread",
            "AddPullRequestReviewThread",
            "SubmitPullRequestReview",
            "AddPullRequestReviewThreadReply",
            "ResolveReviewThread",
            "UnresolveReviewThread",
            "AddPullRequestReview",
            "AddPullRequestReviewThread",
            "SubmitPullRequestReview",
            "AddComment",
            "MarkFileAsViewed",
            "UnmarkFileAsViewed",
        ]
    );
    assert!(
        cassette
            .exchanges
            .iter()
            .all(|e| e.request.method == Method::Post && e.request.path == "/graphql")
    );
}

#[test]
fn anchored_comments_send_line_side_and_range() {
    let cassette = cassette();
    let threads = sent(&cassette, "AddPullRequestReviewThread");
    let shape = |vars: &Value| {
        let mut vars = vars.clone();
        let object = vars.as_object_mut().unwrap();
        for id in ["pullRequestReviewId", "body"] {
            assert!(object.remove(id).is_some_and(|v| v.is_string()), "{id}");
        }
        vars
    };
    let shapes: Vec<Value> = threads.iter().map(shape).collect();
    let path = "docs/intro.md";
    assert_eq!(
        shapes,
        [
            json!({"path": path, "subjectType": "LINE", "line": 3, "side": "RIGHT"}),
            json!({"path": path, "subjectType": "FILE"}),
            json!({"path": path, "subjectType": "LINE", "line": 3, "side": "RIGHT"}),
            json!({"path": path, "subjectType": "LINE", "line": 5, "side": "RIGHT",
                   "startLine": 3, "startSide": "RIGHT"}),
            json!({"path": path, "subjectType": "LINE", "line": 3, "side": "LEFT"}),
            json!({"path": path, "subjectType": "LINE", "line": 4, "side": "RIGHT"}),
        ]
    );
    let started = sent(&cassette, "AddPullRequestReview");
    assert!(started.iter().all(|vars| {
        vars["commitOID"]
            .as_str()
            .is_some_and(|oid| oid.len() == 40)
            && vars["pullRequestId"]
                .as_str()
                .is_some_and(|id| id.starts_with("PR_"))
            && vars.get("event").is_none()
    }));
    let submitted: Vec<Value> = sent(&cassette, "SubmitPullRequestReview")
        .iter()
        .map(|v| json!([v["event"], v["body"]]))
        .collect();
    assert_eq!(
        submitted,
        [
            json!(["COMMENT", "Round trip review."]),
            json!(["COMMENT", ""])
        ]
    );
    // A reply sent at once names no review; resolve takes the same thread.
    let reply = &sent(&cassette, "AddPullRequestReviewThreadReply")[0];
    assert!(reply.get("pullRequestReviewId").is_none());
    let thread = &reply["pullRequestReviewThreadId"];
    assert_eq!(
        &sent(&cassette, "ResolveReviewThread")[0]["threadId"],
        thread
    );
    assert_eq!(
        &sent(&cassette, "UnresolveReviewThread")[0]["threadId"],
        thread
    );
    for op in ["MarkFileAsViewed", "UnmarkFileAsViewed"] {
        assert_eq!(sent(&cassette, op)[0]["path"], path, "{op}");
    }
}

#[tokio::test]
async fn writes_return_the_mapped_objects() {
    let cassette = cassette();
    let (github, _) = replay().await;
    let sandbox = repo();
    let number = recorded_number(&cassette);

    let submit = &sent(&cassette, "SubmitPullRequestReview")[0];
    let review = ReviewId::new(submit["pullRequestReviewId"].as_str().unwrap());
    let submitted = github
        .submit_review(
            &sandbox,
            number,
            &review,
            Verdict::Comment,
            "Round trip review.",
        )
        .await
        .unwrap();
    assert_eq!(submitted.id, review);
    assert_eq!(submitted.state, ReviewState::Commented);
    assert_eq!(submitted.author.login, "Lovelace");
    assert_eq!(submitted.body, "Round trip review.");
    assert!(submitted.submitted_at.is_some() && submitted.web_url.is_some());
    let head = sent(&cassette, "AddPullRequestReview")[0]["commitOID"].clone();
    assert_eq!(
        submitted.commit.as_ref().map(|sha| sha.as_str()),
        head.as_str()
    );

    let reply = &sent(&cassette, "AddPullRequestReviewThreadReply")[0];
    let thread = ThreadId::new(reply["pullRequestReviewThreadId"].as_str().unwrap());
    let resolved = github
        .set_resolved(&sandbox, number, &thread, true)
        .await
        .unwrap();
    assert!(resolved.is_resolved());
    assert_eq!(resolved.id, thread);
    let position = resolved.anchor.unwrap().position.unwrap();
    assert_eq!(
        (position.line, position.start),
        (
            LineRef {
                side: Side::New,
                line: 3
            },
            None
        )
    );
    assert_eq!(resolved.comments.len(), 2);

    let comment = github
        .add_comment(&sandbox, number, "Round trip conversation comment.")
        .await
        .unwrap();
    assert_eq!(comment.body, "Round trip conversation comment.");
    assert_eq!(comment.author.login, "Lovelace");
    assert!(comment.review.is_none() && comment.verdict.is_none());

    let viewed = github
        .set_viewed(&sandbox, number, "docs/intro.md", true)
        .await
        .unwrap();
    assert_eq!(viewed.state, ViewedState::Viewed);

    let pending = github.pending_review(&sandbox, number).await.unwrap();
    let pending = pending.expect("the first listing has the two drafts");
    assert_eq!(pending.review.state, ReviewState::Pending);
    let anchors: Vec<_> = pending
        .comments
        .iter()
        .map(|c| (c.place.clone(), c.thread.is_some()))
        .collect();
    assert_eq!(
        anchors,
        [
            (
                PendingPlace::New(CommentAnchor::Line {
                    path: "docs/intro.md".to_owned(),
                    line: LineRef {
                        side: Side::New,
                        line: 3
                    },
                    start: None,
                }),
                true
            ),
            (
                PendingPlace::New(CommentAnchor::File {
                    path: "docs/intro.md".to_owned()
                }),
                true
            ),
        ]
    );
}

#[tokio::test]
async fn an_unrecorded_write_is_a_miss_not_a_send() {
    let (github, _) = replay().await;
    let number = recorded_number(&cassette());
    let err = github
        .add_comment(&repo(), number, "never recorded")
        .await
        .unwrap_err();
    let Error::ReplayMiss { request, .. } = err else {
        panic!("expected a replay miss, got {err:?}");
    };
    assert!(request.contains("operation=AddComment"), "{request}");
}

/// Answers from an inline cassette and counts what it was sent.
struct Counting {
    replay: ReplayTransport,
    sent: AtomicUsize,
    retries: Mutex<Vec<RetryMode>>,
}

#[async_trait]
impl Transport for Counting {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        self.sent.fetch_add(1, Ordering::SeqCst);
        self.retries.lock().unwrap().push(request.retry);
        self.replay.send(request).await
    }
}

fn rest_exchange(path: &str, status: u16) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method: Method::Post,
            path: path.to_owned(),
            query: Vec::new(),
            operation: None,
            variables: None,
            body: Some(json!({})),
        },
        response: RecordedResponse {
            status,
            headers: Default::default(),
            body: if status < 300 {
                RecordedBody::Empty
            } else {
                RecordedBody::Json(json!({"message": "server error"}))
            },
        },
    }
}

#[tokio::test]
async fn reruns_post_to_actions_and_are_never_retried() {
    let transport = Arc::new(Counting {
        replay: ReplayTransport::new(
            "inline",
            Cassette {
                exchanges: vec![
                    rest_exchange("/repos/Lovelace/review-sandbox/actions/jobs/123/rerun", 201),
                    rest_exchange(
                        "/repos/Lovelace/review-sandbox/actions/runs/456/rerun-failed-jobs",
                        502,
                    ),
                ],
            },
        ),
        sent: AtomicUsize::new(0),
        retries: Mutex::default(),
    });
    let github = GitHub::connect(client(transport.clone())).await.unwrap();
    let sandbox: RepoRef = repo();
    github
        .retry_job(&sandbox, &JobId::new("123"))
        .await
        .unwrap();
    let err = github
        .retry_failed_jobs(&sandbox, &RunId::new("456"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Status { status: 502, .. }), "{err:?}");
    assert_eq!(
        transport.sent.load(Ordering::SeqCst),
        2,
        "a 502 is not resent"
    );
    assert!(
        transport
            .retries
            .lock()
            .unwrap()
            .iter()
            .all(|mode| *mode == RetryMode::Never)
    );
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn editing_a_queued_comment_updates_its_body_in_place() {
    let exchange = Exchange {
        request: RecordedRequest {
            method: Method::Post,
            path: "/graphql".to_owned(),
            query: Vec::new(),
            operation: Some("UpdatePullRequestReviewComment".to_owned()),
            variables: Some(json!({"pullRequestReviewCommentId": "PRRC_1", "body": "better"})),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: RecordedBody::Json(json!({"data": {"updatePullRequestReviewComment": {
                "pullRequestReviewComment": {
                    "id": "PRRC_1", "author": {"__typename": "User", "login": "Lovelace",
                        "avatarUrl": null, "id": "U_1", "name": null},
                    "body": "better", "createdAt": "2026-11-17T11:55:43Z", "lastEditedAt": null,
                    "url": null, "originalCommit": null, "commit": null,
                    "pullRequestReview": {"id": "PRR_1", "state": "PENDING"}}}}})),
        },
    };
    let replay = Arc::new(ReplayTransport::new(
        "inline",
        Cassette {
            exchanges: vec![exchange],
        },
    ));
    let github = GitHub::connect(client(replay.clone())).await.unwrap();
    let before = forge::model::PendingComment {
        id: forge::model::CommentId::new("PRRC_1"),
        place: PendingPlace::Reply(ThreadId::new("PRRT_1")),
        thread: Some(ThreadId::new("PRRT_1")),
        body: "good".to_owned(),
        created_at: None,
    };
    let after = github
        .edit_review_comment(&repo(), PrNumber(4), &before, "better")
        .await
        .unwrap();
    assert_eq!(after.body, "better");
    assert_eq!(
        (after.place, after.thread),
        (before.place, before.thread),
        "the place is the one it was queued at"
    );
    assert!(after.created_at.is_some());
    assert!(replay.unused().is_empty());
}
