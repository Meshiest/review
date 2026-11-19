//! GitLab writes against a recording of one live round trip
//! (`tests/fixtures/gitlab/writes.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example round_trip --
//! --forge gitlab` (it opens and closes a scratch merge request on
//! `review-sandbox`), then replace the avatar hashes with zeros
//! (`gravatar.com/avatar/<64 hex>`), as `recorded_fixtures_hold_no_secrets`
//! checks. These tests only replay: `ReplayTransport` has no network, and a
//! request without a recording is a `ReplayMiss`.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge::auth::{Credential, StaticCredential, Token};
use forge::gitlab::GitLab;
use forge::http::{
    HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, RetryMode, Transport,
};
use forge::model::{
    CommentAnchor, CommentId, ForgeKind, JobId, LineRef, PendingPlace, PrNumber, RepoRef, ReviewId,
    ReviewState, RunId, Side, SuggestionId, ThreadId, Verdict,
};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
    fixture_path,
};
use forge::{Error, Forge};
use serde_json::{Value, json};

use super::round_trip_steps::{self, Status};

const HOST: &str = "gitlab.com";

fn repo() -> RepoRef {
    RepoRef::new(HOST, "Lovelace", "review-sandbox")
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

fn cassette() -> Cassette {
    let path = fixture_path(ForgeKind::GitLab, "writes");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The scratch merge request the recording was made on.
fn recorded_number(cassette: &Cassette) -> PrNumber {
    let iid = cassette
        .exchanges
        .iter()
        .find_map(|e| e.request.variables.as_ref()?.get("iid")?.as_str())
        .expect("the recording reads a merge request");
    PrNumber(iid.parse().unwrap())
}

async fn replay() -> (GitLab, Arc<ReplayTransport>) {
    let path = fixture_path(ForgeKind::GitLab, "writes");
    let replay = Arc::new(ReplayTransport::load(&path).unwrap());
    let gitlab = replayed(replay.clone())
        .await
        .with_settle_interval(Duration::ZERO);
    (gitlab, replay)
}

/// Method and the part of the path after the merge request, of every REST
/// write, in order.
fn writes(cassette: &Cassette) -> Vec<(Method, String)> {
    cassette
        .exchanges
        .iter()
        .filter(|e| e.request.method != Method::Get)
        .filter(|e| e.request.operation.is_none())
        .map(|e| {
            let path = &e.request.path;
            let tail = path.split_once("/merge_requests/").map_or_else(
                || path.clone(),
                |(_, rest)| {
                    rest.split_once('/')
                        .map_or(String::new(), |(_, t)| t.to_owned())
                },
            );
            // Discussion ids differ per run; keep the shape.
            let tail = match tail.split('/').collect::<Vec<_>>().as_slice() {
                ["discussions", _, rest @ ..] => format!("discussions/:id/{}", rest.join("/")),
                _ => tail,
            };
            (e.request.method, tail.trim_end_matches('/').to_owned())
        })
        .collect()
}

/// The JSON bodies of the writes whose path ends with `tail`.
fn sent(cassette: &Cassette, method: Method, tail: &str) -> Vec<Value> {
    cassette
        .exchanges
        .iter()
        .filter(|e| e.request.method == method && e.request.path.ends_with(tail))
        .filter_map(|e| e.request.body.clone())
        .collect()
}

#[tokio::test]
async fn the_round_trip_replays_with_every_step_passing() {
    let (gitlab, replay) = replay().await;
    let number = recorded_number(&cassette());
    let outcomes = round_trip_steps::run(&gitlab, &repo(), number, |_| {}).await;
    let lines: Vec<String> = outcomes.iter().map(ToString::to_string).collect();
    assert!(outcomes.iter().all(|o| !o.failed()), "{lines:#?}");
    let skipped: Vec<_> = outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Skip(_)))
        .map(|o| o.step)
        .collect();
    assert_eq!(skipped, ["viewed"], "{lines:#?}");
    assert_eq!(replay.unused(), Vec::<String>::new());
}

#[test]
fn writes_are_the_rest_calls_in_round_trip_order() {
    use Method::{Delete, Post, Put};
    let draft = || (Post, "draft_notes".to_owned());
    let publish = || (Post, "draft_notes/bulk_publish".to_owned());
    let delete = || (Delete, "draft_notes".to_owned());
    let calls: Vec<(Method, String)> = writes(&cassette())
        .into_iter()
        .map(|(method, tail)| match tail.strip_prefix("draft_notes/") {
            Some(id) if method == Delete && id.chars().all(|c| c.is_ascii_digit()) => delete(),
            _ => (method, tail),
        })
        .collect();
    assert_eq!(
        calls,
        [
            draft(),
            draft(),
            delete(),
            delete(),
            draft(),
            draft(),
            draft(),
            publish(),
            (Post, "discussions/:id/notes".to_owned()),
            (Put, "discussions/:id/".trim_end_matches('/').to_owned()),
            (Put, "discussions/:id/".trim_end_matches('/').to_owned()),
            draft(),
            publish(),
            (Post, "notes".to_owned()),
            publish(),
            (Post, "approve".to_owned()),
            publish(),
        ]
    );
}

#[test]
fn drafts_send_line_range_removed_side_and_file_positions() {
    let cassette = cassette();
    let drafts = sent(&cassette, Method::Post, "/draft_notes");
    let shape = |draft: &Value| {
        let position = &draft["position"];
        for sha in ["base_sha", "start_sha", "head_sha"] {
            assert_eq!(position[sha].as_str().map(str::len), Some(40), "{sha}");
        }
        let mut position = position.clone();
        let object = position.as_object_mut().unwrap();
        for key in ["base_sha", "start_sha", "head_sha"] {
            object.remove(key);
        }
        position
    };
    let path = json!({"old_path": "docs/intro.md", "new_path": "docs/intro.md"});
    let text = |extra: Value| {
        let mut position = path.clone();
        position["position_type"] = json!("text");
        position
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        position
    };
    let shapes: Vec<Value> = drafts.iter().map(shape).collect();
    let mut file = path.clone();
    file["position_type"] = json!("file");
    assert_eq!(
        shapes,
        [
            text(json!({"new_line": 3})),
            file,
            text(json!({"new_line": 3})),
            text(json!({"new_line": 5, "line_range": {
                "start": {"type": "new", "new_line": 3},
                "end": {"type": "new", "new_line": 5},
            }})),
            text(json!({"old_line": 3})),
            text(json!({"new_line": 4})),
        ]
    );
    assert!(drafts.iter().all(|d| d["note"].is_string()));
}

#[test]
fn verdicts_publish_with_a_reviewer_state_except_approve() {
    let cassette = cassette();
    let published = sent(&cassette, Method::Post, "/draft_notes/bulk_publish");
    assert_eq!(
        published,
        [
            json!({"note": "Round trip review.", "reviewer_state": "reviewed"}),
            json!({"reviewer_state": "reviewed"}),
            json!({"note": "Round trip approval."}),
            json!({"note": "Round trip: changes requested.", "reviewer_state": "requested_changes"}),
        ]
    );
    assert_eq!(sent(&cassette, Method::Post, "/approve"), [json!({})]);
}

#[tokio::test]
async fn writes_return_the_mapped_objects() {
    let cassette = cassette();
    let (gitlab, _) = replay().await;
    let sandbox = repo();
    let number = recorded_number(&cassette);

    let review = ReviewId::new(format!("{sandbox}!{number}"));
    let started = gitlab
        .start_review(&sandbox, number, &forge::model::Sha::new("a".repeat(40)))
        .await
        .unwrap();
    assert_eq!(started.review.id, review);
    assert_eq!(started.review.state, ReviewState::Pending);
    assert_eq!(started.review.author.login, "Lovelace");
    assert!(started.comments.is_empty());

    let transport = counting(vec![
        cassette
            .exchanges
            .iter()
            .find(|e| e.request.operation.as_deref() == Some("Whoami"))
            .expect("the recording asks who the viewer is")
            .clone(),
        notes_listing(number, json!([])),
        exchange(
            Method::Post,
            &format!("{PROJECT}/merge_requests/{number}/draft_notes/bulk_publish"),
            Some(json!({"note": "Round trip review.", "reviewer_state": "reviewed"})),
            204,
        ),
        drafts_listing(number, json!([])),
        notes_listing(number, json!([system_note(5, "left review comments")])),
    ]);
    let submitted = replayed(transport)
        .await
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

    let thread = ThreadId::new(
        cassette
            .exchanges
            .iter()
            .find(|e| e.request.method == Method::Put)
            .and_then(|e| e.request.path.rsplit('/').next())
            .unwrap(),
    );
    let reply = gitlab
        .reply(&sandbox, number, &thread, "Round trip reply.")
        .await
        .unwrap();
    assert_eq!(reply.body, "Round trip reply.");
    assert_eq!(reply.author.login, "Lovelace");
    assert!(reply.verdict.is_none());

    let resolved = gitlab
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

    let comment = gitlab
        .add_comment(&sandbox, number, "Round trip conversation comment.")
        .await
        .unwrap();
    assert_eq!(comment.body, "Round trip conversation comment.");
    assert!(comment.web_url.is_some_and(|url| url.contains("#note_")));
}

#[tokio::test]
async fn queued_comments_come_back_from_the_listing_in_the_composers_terms() {
    let (gitlab, _) = replay().await;
    let number = recorded_number(&cassette());
    let pending = gitlab
        .pending_review(&repo(), number)
        .await
        .unwrap()
        .expect("the first listing has the two drafts");
    assert_eq!(pending.review.state, ReviewState::Pending);
    assert_eq!(pending.review.author.login, "Lovelace");
    let places: Vec<_> = pending.comments.iter().map(|c| c.place.clone()).collect();
    let path = "docs/intro.md".to_owned();
    assert_eq!(
        places,
        [
            PendingPlace::New(CommentAnchor::Line {
                path: path.clone(),
                line: LineRef {
                    side: Side::New,
                    line: 3
                },
                start: None,
            }),
            PendingPlace::New(CommentAnchor::File { path }),
        ]
    );
    assert!(pending.comments.iter().all(|c| c.thread.is_none()));
    assert_eq!(
        pending.review.commit.as_ref().map(|sha| sha.as_str().len()),
        Some(40)
    );
}

#[tokio::test]
async fn an_unrecorded_write_is_a_miss_not_a_send() {
    let (gitlab, _) = replay().await;
    let number = recorded_number(&cassette());
    let err = gitlab
        .add_comment(&repo(), number, "never recorded")
        .await
        .unwrap_err();
    let Error::ReplayMiss { request, .. } = err else {
        panic!("expected a replay miss, got {err:?}");
    };
    assert!(
        request.contains("POST") && request.contains("/notes"),
        "{request}"
    );
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

fn exchange(method: Method, path: &str, body: Option<Value>, status: u16) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method,
            path: path.to_owned(),
            query: Vec::new(),
            operation: None,
            variables: None,
            body,
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

const PROJECT: &str = "/api/v4/projects/Lovelace%2Freview-sandbox";

fn counting(exchanges: Vec<Exchange>) -> Arc<Counting> {
    Arc::new(Counting {
        replay: ReplayTransport::new("inline", Cassette { exchanges }),
        sent: AtomicUsize::new(0),
        retries: Mutex::default(),
    })
}

#[tokio::test]
async fn retries_suggestions_and_deletes_hit_their_endpoints() {
    let transport = counting(vec![
        exchange(
            Method::Post,
            &format!("{PROJECT}/jobs/123/retry"),
            Some(json!({})),
            201,
        ),
        exchange(
            Method::Post,
            &format!("{PROJECT}/pipelines/456/retry"),
            Some(json!({})),
            201,
        ),
        exchange(Method::Put, "/api/v4/suggestions/7/apply", None, 200),
        exchange(
            Method::Put,
            "/api/v4/suggestions/batch_apply",
            Some(json!({"ids": [8, 9]})),
            200,
        ),
        exchange(
            Method::Delete,
            &format!("{PROJECT}/merge_requests/3/draft_notes/55"),
            None,
            204,
        ),
    ]);
    let gitlab = replayed(transport.clone()).await;
    let sandbox = repo();
    gitlab
        .retry_job(&sandbox, &JobId::new("123"))
        .await
        .unwrap();
    gitlab
        .retry_failed_jobs(&sandbox, &RunId::new("456"))
        .await
        .unwrap();
    let ids = |ids: &[&str]| {
        ids.iter()
            .copied()
            .map(SuggestionId::new)
            .collect::<Vec<_>>()
    };
    let number = PrNumber(3);
    gitlab
        .apply_suggestions(&sandbox, number, &ids(&["7"]))
        .await
        .unwrap();
    gitlab
        .apply_suggestions(&sandbox, number, &ids(&["gid://gitlab/Suggestion/8", "9"]))
        .await
        .unwrap();
    gitlab
        .apply_suggestions(&sandbox, number, &[])
        .await
        .unwrap();
    gitlab
        .delete_review_comment(&sandbox, number, &CommentId::new("55"))
        .await
        .unwrap();
    assert_eq!(transport.sent.load(Ordering::SeqCst), 5);
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn a_5xx_on_a_write_is_returned_and_never_resent() {
    let transport = counting(vec![
        exchange(
            Method::Post,
            &format!("{PROJECT}/pipelines/456/retry"),
            Some(json!({})),
            502,
        ),
        exchange(
            Method::Put,
            &format!("{PROJECT}/merge_requests/3/discussions/abc"),
            Some(json!({"resolved": true})),
            503,
        ),
        exchange(
            Method::Post,
            &format!("{PROJECT}/merge_requests/3/notes"),
            Some(json!({"body": "hi"})),
            500,
        ),
        exchange(
            Method::Delete,
            &format!("{PROJECT}/merge_requests/3/draft_notes/9"),
            None,
            504,
        ),
    ]);
    let gitlab = replayed(transport.clone()).await;
    let sandbox = repo();
    let number = PrNumber(3);
    let errors = [
        gitlab
            .retry_failed_jobs(&sandbox, &RunId::new("456"))
            .await
            .unwrap_err(),
        gitlab
            .set_resolved(&sandbox, number, &ThreadId::new("abc"), true)
            .await
            .unwrap_err(),
        gitlab
            .add_comment(&sandbox, number, "hi")
            .await
            .unwrap_err(),
        gitlab
            .delete_review_comment(&sandbox, number, &CommentId::new("9"))
            .await
            .unwrap_err(),
    ];
    for (err, status) in errors.iter().zip([502, 503, 500, 504]) {
        assert!(
            matches!(err, Error::Status { status: s, .. } if *s == status),
            "{err:?}"
        );
    }
    assert_eq!(
        transport.sent.load(Ordering::SeqCst),
        4,
        "nothing is resent"
    );
    assert!(
        transport
            .retries
            .lock()
            .unwrap()
            .iter()
            .all(|mode| *mode == RetryMode::Never)
    );
}

/// `GET .../draft_notes` answering `drafts`.
fn drafts_listing(number: PrNumber, drafts: Value) -> Exchange {
    let mut listing = exchange(
        Method::Get,
        &format!("{PROJECT}/merge_requests/{number}/draft_notes"),
        None,
        200,
    );
    listing.request.query = vec![("per_page".to_owned(), "100".to_owned())];
    listing.response.body = RecordedBody::Json(drafts);
    listing
}

/// `GET .../notes` (newest first) answering `notes`.
fn notes_listing(number: PrNumber, notes: Value) -> Exchange {
    let mut listing = exchange(
        Method::Get,
        &format!("{PROJECT}/merge_requests/{number}/notes"),
        None,
        200,
    );
    listing.request.query = vec![
        ("order_by".to_owned(), "created_at".to_owned()),
        ("per_page".to_owned(), "20".to_owned()),
        ("sort".to_owned(), "desc".to_owned()),
    ];
    listing.response.body = RecordedBody::Json(notes);
    listing
}

/// A system note by the recorded viewer.
fn system_note(id: u64, body: &str) -> Value {
    json!({"id": id, "body": body, "system": true, "created_at": "2026-11-17T12:00:00Z",
        "author": {"id": 1, "username": "Lovelace", "name": "M", "avatar_url": null}})
}

/// A verdict submit against an inline cassette: who am I, the notes before,
/// the publish, the drafts left, then one look at the notes per `polls` entry.
fn verdict_transport(reviewer_state: &str, before: Value, polls: Vec<Value>) -> Arc<Counting> {
    let number = PrNumber(3);
    let whoami = cassette()
        .exchanges
        .iter()
        .find(|e| e.request.operation.as_deref() == Some("Whoami"))
        .expect("the recording asks who the viewer is")
        .clone();
    let mut exchanges = vec![
        whoami,
        notes_listing(number, before),
        exchange(
            Method::Post,
            &format!("{PROJECT}/merge_requests/3/draft_notes/bulk_publish"),
            Some(json!({"reviewer_state": reviewer_state})),
            204,
        ),
        drafts_listing(number, json!([])),
    ];
    exchanges.extend(polls.into_iter().map(|notes| notes_listing(number, notes)));
    counting(exchanges)
}

async fn submit(
    transport: &Arc<Counting>,
    verdict: Verdict,
) -> forge::Result<forge::model::Review> {
    replayed(transport.clone())
        .await
        .with_settle_interval(Duration::ZERO)
        .submit_review(&repo(), PrNumber(3), &ReviewId::new("r"), verdict, "")
        .await
}

#[tokio::test]
async fn a_verdict_waits_for_its_note_that_shows_on_the_third_look() {
    // An older "requested changes" note is there from the start and must not count.
    let old = json!([system_note(10, "requested changes")]);
    let both = json!([
        system_note(11, "requested changes"),
        system_note(10, "requested changes")
    ]);
    let transport = verdict_transport(
        "requested_changes",
        old.clone(),
        vec![old.clone(), old, both],
    );
    let review = submit(&transport, Verdict::RequestChanges).await.unwrap();
    assert_eq!(review.state, ReviewState::ChangesRequested);
    assert!(transport.replay.unused().is_empty());
    // Whoami, notes before, publish, drafts, then exactly three looks.
    assert_eq!(transport.sent.load(Ordering::SeqCst), 7);
}

#[tokio::test]
async fn a_verdict_note_that_never_comes_is_a_clear_timeout() {
    let old = json!([system_note(10, "approved this merge request")]);
    let transport = verdict_transport("requested_changes", old.clone(), vec![old]);
    let err = submit(&transport, Verdict::RequestChanges)
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::NotReady { .. }), "{err:?}");
    assert!(err.to_string().contains("requested changes"), "{err}");
    // Four sends before the looks, then the whole bound of twenty.
    assert_eq!(transport.sent.load(Ordering::SeqCst), 24);
}

#[tokio::test]
async fn a_comment_review_gives_up_quietly_when_gitlab_writes_no_note() {
    let transport = verdict_transport("reviewed", json!([]), vec![json!([])]);
    let review = submit(&transport, Verdict::Comment).await.unwrap();
    assert_eq!(review.state, ReviewState::Commented);
    assert_eq!(transport.sent.load(Ordering::SeqCst), 24);
}

#[tokio::test]
async fn a_draft_the_forge_could_not_place_fails_the_submit() {
    let draft = |id: u64| {
        json!({"id": id, "note": "x", "discussion_id": null, "position": {"position_type": "text",
            "head_sha": "h", "old_path": "a", "new_path": "a", "new_line": 1}})
    };
    let transport = counting(vec![
        notes_listing(PrNumber(3), json!([])),
        exchange(
            Method::Post,
            &format!("{PROJECT}/merge_requests/3/draft_notes/bulk_publish"),
            Some(json!({"reviewer_state": "reviewed"})),
            204,
        ),
        drafts_listing(PrNumber(3), json!([draft(1)])),
    ]);
    let gitlab = replayed(transport).await;
    let err = gitlab
        .submit_review(
            &repo(),
            PrNumber(3),
            &ReviewId::new("r"),
            Verdict::Comment,
            "",
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&err, Error::Decode { reason, .. } if reason.contains("1 draft notes were not published")),
        "{err:?}"
    );
}

/// `GET .../diffs` answering `files`.
fn diffs_listing(number: PrNumber, files: Value) -> Exchange {
    let mut listing = exchange(
        Method::Get,
        &format!("{PROJECT}/merge_requests/{number}/diffs"),
        None,
        200,
    );
    listing.request.query = vec![("per_page".to_owned(), "100".to_owned())];
    listing.response.body = RecordedBody::Json(files);
    listing
}

fn diff_refs_answer(number: PrNumber) -> Exchange {
    Exchange {
        request: RecordedRequest {
            method: Method::Post,
            path: "/api/graphql".to_owned(),
            query: Vec::new(),
            operation: Some("DiffRefs".to_owned()),
            variables: Some(json!({"path": "Lovelace/review-sandbox", "iid": number.to_string()})),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: RecordedBody::Json(json!({"data": {"project": {"mergeRequest": {
                "diffRefs": {"baseSha": "b", "startSha": "s", "headSha": "h"}}}}})),
        },
    }
}

fn context_line() -> forge::model::NewComment {
    forge::model::NewComment {
        place: PendingPlace::New(CommentAnchor::Line {
            path: "docs/intro.md".to_owned(),
            line: LineRef {
                side: Side::New,
                line: 2,
            },
            start: None,
        }),
        body: "context".to_owned(),
    }
}

#[tokio::test]
async fn a_fresh_merge_request_s_empty_diff_is_asked_again_until_it_shows() {
    let number = PrNumber(3);
    let file = json!([{"old_path": "docs/intro.md", "new_path": "docs/intro.md",
        "diff": "@@ -1,3 +1,3 @@\n a\n b\n-c\n+C\n"}]);
    let mut created = exchange(
        Method::Post,
        &format!("{PROJECT}/merge_requests/3/draft_notes"),
        Some(json!({"note": "context", "position": {
            "base_sha": "b", "start_sha": "s", "head_sha": "h",
            "old_path": "docs/intro.md", "new_path": "docs/intro.md",
            "position_type": "text", "old_line": 2, "new_line": 2}})),
        201,
    );
    created.response.body = RecordedBody::Json(json!({"id": 77, "note": "context",
        "discussion_id": null, "position": null}));
    let transport = counting(vec![
        diff_refs_answer(number),
        diffs_listing(number, json!([])),
        diffs_listing(number, json!([])),
        diffs_listing(number, file),
        created,
    ]);
    let gitlab = replayed(transport.clone())
        .await
        .with_settle_interval(Duration::ZERO);
    let pending = gitlab
        .add_review_comment(&repo(), number, &ReviewId::new("r"), &context_line())
        .await
        .unwrap();
    assert_eq!(pending.id, CommentId::new("77"));
    assert_eq!(pending.created_at, None, "draft notes carry no time");
    assert!(transport.replay.unused().is_empty());
}

#[tokio::test]
async fn a_diff_that_stays_empty_is_a_clear_error_and_nothing_is_written() {
    let number = PrNumber(3);
    let transport = counting(vec![
        diff_refs_answer(number),
        diffs_listing(number, json!([])),
    ]);
    let gitlab = replayed(transport.clone())
        .await
        .with_settle_interval(Duration::ZERO);
    let err = gitlab
        .add_review_comment(&repo(), number, &ReviewId::new("r"), &context_line())
        .await
        .unwrap_err();
    assert!(matches!(&err, Error::NotReady { .. }), "{err:?}");
    assert!(err.to_string().contains("merge request !3"), "{err}");
    // The first look and three more after backoff, then no draft note.
    assert_eq!(transport.sent.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn editing_a_draft_note_puts_its_new_text() {
    let mut edited = exchange(
        Method::Put,
        &format!("{PROJECT}/merge_requests/3/draft_notes/55"),
        Some(json!({"note": "better"})),
        200,
    );
    edited.response.body = RecordedBody::Json(json!({"id": 55, "note": "better",
        "discussion_id": null, "position": null}));
    let transport = counting(vec![edited]);
    let gitlab = replayed(transport.clone()).await;
    let before = forge::model::PendingComment {
        id: CommentId::new("55"),
        place: context_line().place,
        thread: None,
        body: "good".to_owned(),
        created_at: None,
    };
    let after = gitlab
        .edit_review_comment(&repo(), PrNumber(3), &before, "better")
        .await
        .unwrap();
    assert_eq!(after.body, "better");
    assert_eq!(after.place, before.place);
    assert!(transport.replay.unused().is_empty());
}
