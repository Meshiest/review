//! Forgejo writes against a recording of one live round trip
//! (`tests/fixtures/forgejo/writes.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example round_trip --
//! --forge forgejo` (it opens and closes a scratch pull request on
//! `review-sandbox`). These tests only replay: `ReplayTransport` has no
//! network, and a request without a recording is a `ReplayMiss`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge::auth::{Credential, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::http::{
    HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, RetryMode, Transport,
};
use forge::model::{
    CommentAnchor, ForgeKind, JobId, LineRef, NewComment, PendingPlace, PrNumber, RepoRef,
    ReviewId, ReviewState, RunId, Side, ThreadId, Verdict,
};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
    fixture_path,
};
use forge::{Error, Forge};
use serde_json::{Value, json};

use super::round_trip_steps::{self, Status};

const HOST: &str = "forgejo.example.com";
const PATH: &str = "docs/intro.md";

fn sandbox() -> RepoRef {
    RepoRef::new(HOST, "cora", "review-sandbox")
}

fn client(transport: Arc<dyn Transport>) -> HttpClient {
    HttpClient::new(
        ForgeKind::Forgejo,
        HOST,
        transport,
        Arc::new(StaticCredential(Credential {
            token: Token::new("replay"),
            scheme: forge::auth::Scheme::Token,
        })),
        HttpOptions {
            clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
            ..HttpOptions::for_kind(ForgeKind::Forgejo)
        },
    )
}

fn cassette() -> Cassette {
    let path = fixture_path(ForgeKind::Forgejo, "writes");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn replay() -> (Forgejo, Arc<ReplayTransport>) {
    let path = fixture_path(ForgeKind::Forgejo, "writes");
    let replay = Arc::new(ReplayTransport::load(&path).unwrap());
    let forgejo = Forgejo::connect(client(replay.clone())).await.unwrap();
    (forgejo, replay)
}

/// The scratch pull request the recording was made on.
fn recorded_number(cassette: &Cassette) -> PrNumber {
    let number = cassette
        .exchanges
        .iter()
        .find_map(|e| {
            let rest = e.request.path.split("/pulls/").nth(1)?;
            rest.split('/').next()?.parse().ok()
        })
        .expect("the recording reads a pull request");
    PrNumber(number)
}

/// The recorded exchange of the first write to `suffix` whose body has
/// `body == text`.
fn recorded_write<'c>(cassette: &'c Cassette, suffix: &str, text: &str) -> &'c Exchange {
    cassette
        .exchanges
        .iter()
        .find(|e| {
            e.request.method == Method::Post
                && e.request.path.ends_with(suffix)
                && e.request
                    .body
                    .as_ref()
                    .is_some_and(|body| body["body"] == text)
        })
        .unwrap_or_else(|| panic!("no recorded write to {suffix} with {text:?}"))
}

/// The id after `reviews/` in a recorded path.
fn review_id_of(path: &str) -> String {
    let rest = path.split("/reviews/").nth(1).unwrap();
    rest.split('/').next().unwrap().to_owned()
}

/// The recorded requests that are not reads, as `(method, path after the
/// pull request, body)`.
fn writes(cassette: &Cassette) -> Vec<(Method, String, Value)> {
    cassette
        .exchanges
        .iter()
        .map(|e| &e.request)
        .filter(|r| r.method != Method::Get)
        .map(|r| {
            let (_, after) = r.path.split_once("/review-sandbox/").unwrap();
            // Drop `pulls/{n}/` or `issues/{n}/`.
            let rest = after.split('/').skip(2).collect::<Vec<_>>().join("/");
            (r.method, rest, r.body.clone().unwrap_or(Value::Null))
        })
        .collect()
}

#[tokio::test]
async fn the_round_trip_replays_with_every_step_passing() {
    let (forgejo, replay) = replay().await;
    let number = recorded_number(&cassette());
    let outcomes = round_trip_steps::run(&forgejo, &sandbox(), number, |_| {}).await;
    let lines: Vec<String> = outcomes.iter().map(ToString::to_string).collect();
    assert!(outcomes.iter().all(|o| !o.failed()), "{lines:#?}");
    let skipped: Vec<_> = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Skip(_)))
        .map(|o| o.step)
        .collect();
    assert_eq!(
        skipped,
        ["resolve", "viewed", "approve", "request-changes"],
        "{lines:#?}"
    );
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[test]
fn writes_are_the_review_endpoints_in_round_trip_order() {
    let sent: Vec<(Method, String)> = writes(&cassette())
        .into_iter()
        .map(|(method, rest, _)| {
            // Review and comment ids differ per recording.
            let shape: Vec<&str> = rest
                .split('/')
                .map(|part| {
                    if part.chars().all(|c| c.is_ascii_digit()) {
                        "{id}"
                    } else {
                        part
                    }
                })
                .collect();
            (method, shape.join("/"))
        })
        .collect();
    let post = |path: &str| (Method::Post, path.to_owned());
    let delete = |path: &str| (Method::Delete, path.to_owned());
    assert_eq!(
        sent,
        [
            post("reviews"),
            post("reviews/{id}/comments"),
            post("reviews/{id}/comments"),
            delete("reviews/{id}/comments/{id}"),
            delete("reviews/{id}"),
            post("reviews"),
            post("reviews/{id}/comments"),
            post("reviews/{id}/comments"),
            post("reviews/{id}/comments"),
            post("reviews/{id}"),
            post("reviews"),
            post("reviews"),
            post("reviews/{id}/comments"),
            post("reviews/{id}"),
            post("comments"),
        ]
    );
}

#[test]
fn comments_send_one_sided_positions_and_a_line_count() {
    let comments: Vec<Value> = writes(&cassette())
        .into_iter()
        .filter(|(method, rest, _)| *method == Method::Post && rest.ends_with("/comments"))
        .filter(|(_, rest, _)| rest.starts_with("reviews/"))
        .map(|(_, _, mut body)| {
            assert!(body["body"].is_string());
            body.as_object_mut().unwrap().remove("body");
            body
        })
        .collect();
    let at = |new_position: u32, old_position: u32, extra: u32| {
        json!({"path": PATH, "new_position": new_position,
               "old_position": old_position, "extra_lines_count": extra})
    };
    assert_eq!(
        comments,
        [
            at(3, 0, 0),
            at(0, 3, 0),
            at(3, 0, 0),
            at(3, 0, 2),
            at(0, 3, 0),
            at(4, 0, 0),
        ]
    );
}

#[test]
fn reviews_start_with_a_body_the_server_accepts_and_submit_with_an_event() {
    let writes = writes(&cassette());
    let starts: Vec<&Value> = writes
        .iter()
        .filter(|(m, rest, body)| {
            *m == Method::Post && rest == "reviews" && body.get("comments").is_none()
        })
        .map(|(_, _, body)| body)
        .collect();
    assert_eq!(starts.len(), 3);
    for start in starts {
        let object = start.as_object().unwrap();
        // No `event`: the server reads an explicit PENDING as a submit that
        // needs a body. The body is a zero-width space.
        assert!(!object.contains_key("event"), "{start}");
        assert_eq!(object["body"], "\u{200b}");
        assert_eq!(object["commit_id"].as_str().unwrap().len(), 40);
    }
    let submits: Vec<Value> = writes
        .iter()
        .filter(|(m, rest, _)| {
            *m == Method::Post && rest.starts_with("reviews/") && !rest.contains("comments")
        })
        .map(|(_, _, body)| body.clone())
        .collect();
    assert_eq!(
        submits,
        [
            json!({"event": "COMMENT", "body": "Round trip review."}),
            json!({"event": "COMMENT", "body": "\u{200b}"}),
        ]
    );
    // A reply is one review with one comment, submitted at once.
    let replies: Vec<&Value> = writes
        .iter()
        .filter(|(_, rest, body)| rest == "reviews" && body.get("comments").is_some())
        .map(|(_, _, body)| body)
        .collect();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["event"], "COMMENT");
    assert_eq!(replies[0]["comments"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn writes_return_the_mapped_objects() {
    let cassette = cassette();
    let (forgejo, _) = replay().await;
    let repo = sandbox();
    let number = recorded_number(&cassette);

    let head = cassette
        .exchanges
        .iter()
        .find_map(|e| e.request.body.as_ref()?.get("commit_id")?.as_str())
        .unwrap()
        .to_owned();
    let range_body = "Round trip: three lines.";
    let range_write = recorded_write(&cassette, "/comments", range_body);
    let review_id = review_id_of(&range_write.request.path);

    // As the round trip does, read the pull request before starting.
    forgejo.pull_request(&repo, number).await.unwrap();
    let started = forgejo
        .start_review(&repo, number, &forge::model::Sha::new(&head))
        .await
        .unwrap();
    assert_eq!(started.review.state, ReviewState::Pending);
    assert!(started.comments.is_empty() && started.review.submitted_at.is_none());
    assert_eq!(
        started.review.body, "",
        "the placeholder body is not a summary"
    );
    assert_eq!(started.review.author.login, "cora");

    let pending = forgejo
        .pending_review(&repo, number)
        .await
        .unwrap()
        .expect("the first listing has the two drafts");
    let anchors: Vec<_> = pending.comments.iter().map(|c| c.place.clone()).collect();
    assert_eq!(
        anchors,
        [
            PendingPlace::New(CommentAnchor::Line {
                path: PATH.to_owned(),
                line: LineRef {
                    side: Side::New,
                    line: 3
                },
                start: None,
            }),
            PendingPlace::New(CommentAnchor::Line {
                path: PATH.to_owned(),
                line: LineRef {
                    side: Side::Old,
                    line: 3
                },
                start: None,
            }),
        ]
    );
    assert!(pending.comments.iter().all(|c| c.thread.is_none()));

    let range = forgejo
        .add_review_comment(
            &repo,
            number,
            &ReviewId::new(&review_id),
            &NewComment {
                place: PendingPlace::New(CommentAnchor::Line {
                    path: PATH.to_owned(),
                    line: LineRef {
                        side: Side::New,
                        line: 5,
                    },
                    start: Some(LineRef {
                        side: Side::New,
                        line: 3,
                    }),
                }),
                body: range_body.to_owned(),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        range.place,
        PendingPlace::New(CommentAnchor::Line {
            path: PATH.to_owned(),
            line: LineRef {
                side: Side::New,
                line: 5
            },
            start: Some(LineRef {
                side: Side::New,
                line: 3
            }),
        })
    );

    let submitted = forgejo
        .submit_review(
            &repo,
            number,
            &ReviewId::new(&review_id),
            Verdict::Comment,
            "Round trip review.",
        )
        .await
        .unwrap();
    assert_eq!(submitted.id, ReviewId::new(&review_id));
    assert_eq!(submitted.state, ReviewState::Commented);
    assert_eq!(submitted.author.login, "cora");
    assert_eq!(submitted.body, "Round trip review.");
    assert!(submitted.submitted_at.is_some() && submitted.web_url.is_some());
    assert_eq!(
        submitted.commit.as_ref().map(|sha| sha.as_str()),
        Some(head.as_str())
    );

    let comment = forgejo
        .add_comment(&repo, number, "Round trip conversation comment.")
        .await
        .unwrap();
    assert_eq!(comment.body, "Round trip conversation comment.");
    assert_eq!(comment.author.login, "cora");
    assert!(comment.review.is_none() && comment.verdict.is_none());
}

#[tokio::test]
async fn what_v17_has_no_endpoint_for_stays_unsupported() {
    let (forgejo, replay) = replay().await;
    let repo = sandbox();
    let number = PrNumber(1);
    let file = NewComment {
        place: PendingPlace::New(CommentAnchor::File {
            path: PATH.to_owned(),
        }),
        body: "file".to_owned(),
    };
    let unsupported = |err: Error| matches!(err, Error::Unsupported { .. });
    assert!(unsupported(
        forgejo
            .add_review_comment(&repo, number, &ReviewId::new("1"), &file)
            .await
            .unwrap_err()
    ));
    assert!(unsupported(
        forgejo
            .set_resolved(&repo, number, &ThreadId::new("1"), true)
            .await
            .unwrap_err()
    ));
    assert!(unsupported(
        forgejo
            .set_viewed(&repo, number, PATH, true)
            .await
            .unwrap_err()
    ));
    assert!(matches!(
        forgejo.apply_suggestions(&repo, number, &[]).await,
        Err(Error::ApplyLocally { .. })
    ));
    assert!(!forgejo.capabilities().own_pull_request_verdicts);
    assert!(forgejo.capabilities().multi_line_comments);
    drop(replay);
}

#[tokio::test]
async fn an_unrecorded_write_is_a_miss_not_a_send() {
    let (forgejo, _) = replay().await;
    let number = recorded_number(&cassette());
    let err = forgejo
        .add_comment(&sandbox(), number, "never recorded")
        .await
        .unwrap_err();
    let Error::ReplayMiss { request, .. } = err else {
        panic!("expected a replay miss, got {err:?}");
    };
    assert!(request.contains("/issues/"), "{request}");
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

fn exchange(
    method: Method,
    path: &str,
    body: Option<Value>,
    status: u16,
    reply: Value,
) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method,
            path: format!("/api/v1/{path}"),
            query: Vec::new(),
            operation: None,
            variables: None,
            body,
        },
        response: RecordedResponse {
            status,
            headers: Default::default(),
            body: if reply.is_null() {
                RecordedBody::Empty
            } else {
                RecordedBody::Json(reply)
            },
        },
    }
}

fn version(version: &str) -> Exchange {
    exchange(
        Method::Get,
        "version",
        None,
        200,
        json!({"version": version}),
    )
}

async fn counting(exchanges: Vec<Exchange>) -> (Forgejo, Arc<Counting>) {
    let transport = Arc::new(Counting {
        replay: ReplayTransport::new("inline", Cassette { exchanges }),
        sent: AtomicUsize::new(0),
        retries: Mutex::default(),
    });
    let forgejo = Forgejo::connect(client(transport.clone())).await.unwrap();
    (forgejo, transport)
}

#[tokio::test]
async fn a_5xx_on_a_write_is_returned_and_never_resent() {
    let comment = "repos/cora/review-sandbox/issues/7/comments";
    let (forgejo, transport) = counting(vec![
        version("17.0.1+gitea-1.22.0"),
        exchange(
            Method::Post,
            comment,
            Some(json!({"body": "hello"})),
            502,
            json!({"message": "bad gateway"}),
        ),
    ])
    .await;
    let err = forgejo
        .add_comment(&sandbox(), PrNumber(7), "hello")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Status { status: 502, .. }), "{err:?}");
    // The version probe, then exactly one write.
    assert_eq!(
        transport.sent.load(Ordering::SeqCst),
        2,
        "a 502 is not resent"
    );
    let retries = transport.retries.lock().unwrap();
    assert_eq!(retries.last(), Some(&RetryMode::Never));
    drop(retries);
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn your_own_pull_request_refuses_a_verdict_and_the_error_comes_back() {
    let review = "repos/cora/review-sandbox/pulls/7/reviews/5";
    let (forgejo, transport) = counting(vec![
        version("17.0.1+gitea-1.22.0"),
        exchange(
            Method::Post,
            review,
            Some(json!({"event": "APPROVED", "body": "ok"})),
            422,
            json!({"message": "approve your own pull is not allowed"}),
        ),
        exchange(
            Method::Post,
            review,
            Some(json!({"event": "REQUEST_CHANGES", "body": "no"})),
            422,
            json!({"message": "reject your own pull is not allowed"}),
        ),
    ])
    .await;
    for (verdict, body, message) in [
        (Verdict::Approve, "ok", "approve your own pull"),
        (Verdict::RequestChanges, "no", "reject your own pull"),
    ] {
        let err = forgejo
            .submit_review(&sandbox(), PrNumber(7), &ReviewId::new("5"), verdict, body)
            .await
            .unwrap_err();
        let Error::Status {
            status, message: m, ..
        } = err
        else {
            panic!("expected a status error, got {err:?}");
        };
        assert_eq!(status, 422);
        assert!(m.contains(message), "{m}");
    }
    assert_eq!(transport.sent.load(Ordering::SeqCst), 3);
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn reruns_post_to_actions_and_are_never_retried() {
    let repo = "repos/cora/review-sandbox/actions";
    let jobs = json!([
        {"id": 10, "name": "build", "status": "success"},
        {"id": 11, "name": "test", "status": "failure"},
        {"id": 12, "name": "lint", "status": "failure"},
    ]);
    let (forgejo, transport) = counting(vec![
        version("17.0.1+gitea-1.22.0"),
        exchange(
            Method::Post,
            &format!("{repo}/jobs/123/rerun"),
            Some(json!({})),
            204,
            Value::Null,
        ),
        exchange(
            Method::Post,
            &format!("{repo}/jobs/124/rerun"),
            Some(json!({})),
            502,
            json!({"message": "bad gateway"}),
        ),
        exchange(
            Method::Get,
            &format!("{repo}/runs/456/jobs"),
            None,
            200,
            jobs,
        ),
        exchange(
            Method::Post,
            &format!("{repo}/jobs/11/rerun"),
            Some(json!({})),
            204,
            Value::Null,
        ),
        // `lint` was already restarted as a dependent of `test`.
        exchange(
            Method::Post,
            &format!("{repo}/jobs/12/rerun"),
            Some(json!({})),
            400,
            json!({"message": "job is not completed"}),
        ),
    ])
    .await;
    let sandbox = sandbox();
    forgejo
        .retry_job(&sandbox, &JobId::new("123"))
        .await
        .unwrap();
    let err = forgejo
        .retry_job(&sandbox, &JobId::new("124"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Status { status: 502, .. }), "{err:?}");
    forgejo
        .retry_failed_jobs(&sandbox, &RunId::new("456"))
        .await
        .unwrap();
    assert_eq!(
        transport.sent.load(Ordering::SeqCst),
        6,
        "a 502 is not resent"
    );
    let retries = transport.retries.lock().unwrap();
    let posts_never = retries
        .iter()
        .filter(|mode| **mode == RetryMode::Never)
        .count();
    assert_eq!(posts_never, 4, "every POST is RetryMode::Never");
    drop(retries);
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn rerun_needs_v17() {
    let (forgejo, transport) = counting(vec![version("16.0.7+gitea-1.22.0")]).await;
    let err = forgejo
        .retry_job(&sandbox(), &JobId::new("1"))
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    assert!(
        !forgejo.capabilities().multi_line_comments,
        "ranges need v17"
    );
    assert_eq!(transport.sent.load(Ordering::SeqCst), 1);
}

fn listing(path: &str, rows: Value) -> Exchange {
    let mut listing = exchange(Method::Get, path, None, 200, rows);
    listing.request.query = vec![
        ("limit".to_owned(), "50".to_owned()),
        ("page".to_owned(), "1".to_owned()),
    ];
    listing
}

#[tokio::test]
async fn a_reply_is_a_one_comment_review_on_the_threads_line_and_range() {
    let pulls = "repos/cora/review-sandbox/pulls/7";
    let root = json!({"id": 20, "path": "a.rs", "position": 4, "extra_lines_count": 2,
        "pull_request_review_id": 1, "commit_id": "abc", "body": "root",
        "created_at": "2026-01-01T00:00:00Z"});
    let reply = json!({"id": 21, "path": "a.rs", "position": 4, "extra_lines_count": 2,
        "pull_request_review_id": 2, "commit_id": "abc", "body": "re",
        "created_at": "2026-01-01T00:01:00Z", "user": {"id": 1, "login": "cora"}});
    let (forgejo, transport) = counting(vec![
        version("17.0.1+gitea-1.22.0"),
        exchange(
            Method::Get,
            pulls,
            None,
            200,
            json!({"number": 7, "head": {"sha": "abc"}, "base": {"ref": "main"}}),
        ),
        listing(
            &format!("{pulls}/reviews"),
            json!([{"id": 1, "state": "COMMENT", "comments_count": 1}]),
        ),
        listing("repos/cora/review-sandbox/issues/7/comments", json!([])),
        listing(&format!("{pulls}/reviews/1/comments"), json!([root])),
        exchange(
            Method::Post,
            &format!("{pulls}/reviews"),
            Some(json!({"event": "COMMENT", "body": "", "comments": [{
                "path": "a.rs", "body": "re", "new_position": 4,
                "old_position": 0, "extra_lines_count": 2}]})),
            200,
            json!({"id": 2, "state": "COMMENT"}),
        ),
        listing(&format!("{pulls}/reviews/2/comments"), json!([reply])),
    ])
    .await;
    let comment = forgejo
        .reply(&sandbox(), PrNumber(7), &ThreadId::new("20"), "re")
        .await
        .unwrap();
    assert_eq!(comment.id.as_str(), "21");
    assert_eq!(comment.body, "re");
    assert_eq!(comment.review, Some(ReviewId::new("2")));
    assert!(transport.replay.unused().is_empty());
    let err = forgejo
        .reply(&sandbox(), PrNumber(7), &ThreadId::new("999"), "re")
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound { .. }), "{err:?}");
}
