//! Reviewers, assignees and labels against recordings of one live run per
//! forge (`tests/fixtures/<forge>/people.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example people --
//! --forge <github|gitlab|forgejo>` (it opens and closes one scratch pull
//! request on `review-sandbox`). These tests only replay.
//!
//! What a recording cannot hold is covered by a scripted transport: GitHub
//! and Forgejo refuse a review request for the pull request's author, and
//! the sandboxes have one account, so the accepted request, a team and the
//! picker's limit are checked against requests the test answers itself.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use forge::COLLABORATOR_LIMIT;
use forge::http::{HttpRequest, HttpResponse, Method, RetryMode, Transport};
use forge::model::{AccountKind, ForgeKind, PrNumber, ReviewState, User};
use forge::record::{Cassette, RecordedBody};
use serde_json::{Value, json};

use super::lifecycle::{Counting, backend, cassette_of, sandbox};
use super::people_steps::{self as steps, Status};

fn cassette(kind: ForgeKind) -> Cassette {
    cassette_of(kind, "people")
}

/// The scratch pull request the recording is about: the number its calls name.
fn scratch_number(cassette: &Cassette) -> PrNumber {
    let number = cassette
        .exchanges
        .iter()
        .find_map(|exchange| {
            let request = &exchange.request;
            let from_variables = request.variables.as_ref().and_then(|vars| {
                let number = vars.get("number").and_then(Value::as_u64);
                number.or_else(|| vars.get("iid")?.as_str()?.parse().ok())
            });
            let from_path = ["/pulls/", "/issues/", "/merge_requests/"]
                .iter()
                .find_map(|marker| {
                    let rest = request.path.split(marker).nth(1)?;
                    rest.split('/').next()?.parse().ok()
                });
            from_variables.or(from_path)
        })
        .expect("the recording names a pull request");
    PrNumber(number)
}

struct Replayed {
    outcomes: Vec<steps::Outcome>,
    transport: Arc<Counting>,
}

async fn replay(kind: ForgeKind) -> Replayed {
    let cassette = cassette(kind);
    let number = scratch_number(&cassette);
    let transport = Counting::named("people", cassette);
    let forge = backend(kind, transport.clone()).await;
    let outcomes = steps::run(forge.as_ref(), &sandbox(kind), number, |_| {}).await;
    Replayed {
        outcomes,
        transport,
    }
}

fn assert_passes(replayed: &Replayed, skipped: &[&str]) {
    let lines: Vec<String> = replayed.outcomes.iter().map(ToString::to_string).collect();
    assert!(replayed.outcomes.iter().all(|o| !o.failed()), "{lines:#?}");
    let skips: Vec<_> = replayed
        .outcomes
        .iter()
        .filter(|o| matches!(o.status, Status::Skip(_)))
        .map(|o| o.step)
        .collect();
    assert_eq!(skips, skipped, "{lines:#?}");
    assert_eq!(replayed.transport.replay.unused(), Vec::<String>::new());
    let sent = replayed.transport.sent();
    assert!(
        sent.iter()
            .filter(|s| s.write)
            .all(|s| s.retry == RetryMode::Never),
        "a write was sent as retryable: {sent:#?}"
    );
    assert!(
        sent.iter().any(|s| s.write),
        "the run sent no write at all: {sent:#?}"
    );
}

/// The non-GraphQL writes of a recording: method, path and body, in order.
fn rest_writes(cassette: &Cassette) -> Vec<(Method, String, Value)> {
    cassette
        .exchanges
        .iter()
        .filter(|e| e.request.method != Method::Get && e.request.operation.is_none())
        .map(|e| {
            (
                e.request.method,
                e.request.path.clone(),
                e.request.body.clone().unwrap_or(Value::Null),
            )
        })
        .collect()
}

const GITHUB_SKIPS: [&str; 3] = ["request-review", "unrequest-review", "team-reviewers"];

#[tokio::test]
async fn github_replays_with_every_step_passing_or_refused() {
    assert_passes(&replay(ForgeKind::GitHub).await, &GITHUB_SKIPS);
}

#[tokio::test]
async fn gitlab_replays_with_every_step_passing() {
    assert_passes(&replay(ForgeKind::GitLab).await, &["team-reviewers"]);
}

#[tokio::test]
async fn forgejo_replays_with_every_step_passing_or_refused() {
    assert_passes(&replay(ForgeKind::Forgejo).await, &GITHUB_SKIPS);
}

#[tokio::test]
async fn a_refused_review_request_says_why() {
    for kind in [ForgeKind::GitHub, ForgeKind::Forgejo] {
        let replayed = replay(kind).await;
        let step = replayed
            .outcomes
            .iter()
            .find(|o| o.step == "request-review")
            .unwrap();
        let Status::Skip(why) = &step.status else {
            panic!("{kind}: {step}");
        };
        assert!(why.contains("HTTP 422"), "{kind}: {why}");
    }
}

#[test]
fn github_sends_logins_names_and_the_one_label_it_changes() {
    let cassette = cassette(ForgeKind::GitHub);
    let n = scratch_number(&cassette);
    let base = format!("/repos/Lovelace/review-sandbox/issues/{n}");
    let writes = rest_writes(&cassette);
    let shapes: Vec<(Method, String)> = writes.iter().map(|(m, p, _)| (*m, p.clone())).collect();
    assert_eq!(
        shapes,
        [
            (Method::Post, format!("{base}/labels")),
            (Method::Post, format!("{base}/assignees")),
            (
                Method::Post,
                format!("/repos/Lovelace/review-sandbox/pulls/{n}/requested_reviewers")
            ),
            (Method::Delete, format!("{base}/assignees")),
            (Method::Delete, format!("{base}/labels/review-test")),
        ]
    );
    assert_eq!(writes[0].2, json!({"labels": ["review-test"]}));
    assert_eq!(writes[1].2, json!({"assignees": ["Lovelace"]}));
    assert_eq!(
        writes[2].2,
        json!({"reviewers": ["Lovelace"], "team_reviewers": []})
    );
    let first_collaborators = cassette
        .exchanges
        .iter()
        .find(|e| e.request.operation.as_deref() == Some("Collaborators"))
        .and_then(|e| e.request.variables.clone())
        .unwrap();
    assert_eq!(first_collaborators["first"], COLLABORATOR_LIMIT);
    assert_eq!(first_collaborators["query"], Value::Null);
}

#[test]
fn gitlab_puts_the_diffs_on_the_merge_request() {
    let cassette = cassette(ForgeKind::GitLab);
    let n = scratch_number(&cassette);
    let path = format!("/api/v4/projects/Lovelace%2Freview-sandbox/merge_requests/{n}");
    let writes = rest_writes(&cassette);
    assert!(
        writes
            .iter()
            .all(|(method, at, _)| *method == Method::Put && *at == path),
        "{writes:?}"
    );
    let bodies: Vec<&Value> = writes.iter().map(|(_, _, body)| body).collect();
    let id = bodies[1]["assignee_ids"][0].as_u64().unwrap();
    assert_ne!(id, 0);
    assert_eq!(
        bodies,
        [
            &json!({"add_labels": "review-test"}),
            &json!({"assignee_ids": [id]}),
            &json!({"reviewer_ids": [id]}),
            &json!({"assignee_ids": [0]}),
            &json!({"remove_labels": "review-test"}),
            &json!({"reviewer_ids": [0]}),
        ]
    );
}

#[test]
fn forgejo_patches_the_assignee_list_and_deletes_a_label_by_id() {
    let cassette = cassette(ForgeKind::Forgejo);
    let n = scratch_number(&cassette);
    let base = format!("/api/v1/repos/cora/review-sandbox/issues/{n}");
    let writes = rest_writes(&cassette);
    let shapes: Vec<(Method, String)> = writes
        .iter()
        .map(|(method, path, _)| (*method, path.clone()))
        .collect();
    let label_id = shapes
        .iter()
        .find_map(|(_, path)| path.strip_prefix(&format!("{base}/labels/")))
        .expect("a label delete")
        .to_owned();
    assert!(label_id.chars().all(|c| c.is_ascii_digit()), "{label_id}");
    assert_eq!(
        shapes,
        [
            (Method::Post, format!("{base}/labels")),
            (Method::Patch, base.clone()),
            (
                Method::Post,
                format!("/api/v1/repos/cora/review-sandbox/pulls/{n}/requested_reviewers")
            ),
            (Method::Patch, base.clone()),
            (Method::Delete, format!("{base}/labels/{label_id}")),
        ]
    );
    assert_eq!(writes[0].2, json!({"labels": ["review-test"]}));
    assert_eq!(writes[1].2, json!({"assignees": ["cora"]}));
    assert_eq!(writes[3].2, json!({"assignees": []}));
}

// ---- a scripted transport for what the sandboxes cannot show ---------------------

type Respond = Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>;

struct Scripted {
    respond: Respond,
    sent: Mutex<Vec<HttpRequest>>,
}

impl Scripted {
    fn new(respond: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            respond: Box::new(respond),
            sent: Mutex::default(),
        })
    }

    fn sent(&self) -> Vec<HttpRequest> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for Scripted {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        self.sent.lock().unwrap().push(request.clone());
        Ok((self.respond)(request))
    }
}

fn json_response(status: u16, headers: &[(&str, String)], body: &Value) -> HttpResponse {
    let mut all = vec![("content-type".to_owned(), "application/json".to_owned())];
    all.extend(headers.iter().map(|(k, v)| ((*k).to_owned(), v.clone())));
    HttpResponse {
        status,
        headers: all,
        body: serde_json::to_vec(body).unwrap(),
        cached: false,
    }
}

/// The body of the first recorded response to a request `matches` picks.
fn recorded_body(
    cassette: &Cassette,
    matches: impl Fn(&forge::record::RecordedRequest) -> bool,
) -> Value {
    let exchange = cassette
        .exchanges
        .iter()
        .find(|e| matches(&e.request))
        .expect("a matching exchange");
    match &exchange.response.body {
        RecordedBody::Json(value) => value.clone(),
        RecordedBody::Empty | RecordedBody::Text(_) => panic!("not JSON"),
    }
}

fn team(login: &str) -> User {
    User {
        kind: AccountKind::Team,
        ..User::new(login)
    }
}

#[tokio::test]
async fn github_asks_for_users_by_login_and_teams_by_slug_and_withdraws_the_same_way() {
    let recorded = cassette(ForgeKind::GitHub);
    let mut read = recorded_body(&recorded, |r| r.operation.as_deref() == Some("PullRequest"));
    *read
        .pointer_mut("/data/repository/pullRequest/reviewRequests/nodes")
        .unwrap() = json!([
        {"requestedReviewer": {"__typename": "User", "id": "U_1", "login": "ann",
                               "name": "Ann", "avatarUrl": "https://example.invalid/a"}},
        {"requestedReviewer": {"__typename": "Team", "id": "T_1", "slug": "core",
                               "name": "Core", "avatarUrl": null,
                               "organization": {"login": "acme"}}},
    ]);
    let transport = Scripted::new(move |request| {
        if request.url.path().ends_with("/graphql") {
            json_response(200, &[], &read)
        } else {
            json_response(201, &[], &json!({}))
        }
    });
    let forge = backend(ForgeKind::GitHub, transport.clone()).await;
    let repo = sandbox(ForgeKind::GitHub);
    let n = scratch_number(&recorded);

    let ann = User::new("ann");
    let pr = forge
        .request_reviewers(&repo, n, std::slice::from_ref(&ann), &[team("acme/core")])
        .await
        .unwrap();
    let rows: Vec<_> = pr
        .reviewers
        .iter()
        .map(|r| (r.user.login.as_str(), r.state, r.is_team()))
        .collect();
    assert_eq!(
        rows,
        [
            ("ann", ReviewState::Requested, false),
            ("acme/core", ReviewState::Requested, true)
        ]
    );
    forge
        .remove_reviewers(&repo, n, &[ann], &[team("acme/core")])
        .await
        .unwrap();

    let writes: Vec<HttpRequest> = transport
        .sent()
        .into_iter()
        .filter(|r| r.method != Method::Get && !r.url.path().ends_with("/graphql"))
        .collect();
    let seen: Vec<(Method, &str, RetryMode)> = writes
        .iter()
        .map(|r| (r.method, r.url.path(), r.retry))
        .collect();
    let path = format!("/repos/Lovelace/review-sandbox/pulls/{n}/requested_reviewers");
    assert_eq!(
        seen,
        [
            (Method::Post, path.as_str(), RetryMode::Never),
            (Method::Delete, path.as_str(), RetryMode::Never)
        ]
    );
    for write in &writes {
        assert_eq!(
            write.body,
            Some(json!({"reviewers": ["ann"], "team_reviewers": ["core"]}))
        );
    }
}

#[tokio::test]
async fn nothing_is_sent_for_an_empty_reviewer_request() {
    let recorded = cassette(ForgeKind::GitHub);
    let read = recorded_body(&recorded, |r| r.operation.as_deref() == Some("PullRequest"));
    let transport = Scripted::new(move |_| json_response(200, &[], &read));
    let forge = backend(ForgeKind::GitHub, transport.clone()).await;
    forge
        .request_reviewers(
            &sandbox(ForgeKind::GitHub),
            scratch_number(&recorded),
            &[],
            &[],
        )
        .await
        .unwrap();
    assert!(
        transport
            .sent()
            .iter()
            .all(|r| r.url.path().ends_with("/graphql")),
        "a write was sent"
    );
}

#[tokio::test]
async fn gitlab_has_no_review_requests_to_groups() {
    let recorded = cassette(ForgeKind::GitLab);
    let transport = Scripted::new(|_| json_response(404, &[], &json!({})));
    let forge = backend(ForgeKind::GitLab, transport.clone()).await;
    let err = forge
        .request_reviewers(
            &sandbox(ForgeKind::GitLab),
            scratch_number(&recorded),
            &[],
            &[team("group")],
        )
        .await
        .unwrap_err();
    assert!(matches!(err, forge::Error::Unsupported { .. }), "{err}");
    assert!(!forge.capabilities().team_reviewers);
    assert!(transport.sent().is_empty());
}

#[tokio::test]
async fn forgejo_asks_for_teams_by_name_and_lists_at_most_a_hundred_users() {
    let recorded = cassette(ForgeKind::Forgejo);
    let n = scratch_number(&recorded);
    let mut pull = recorded_body(&recorded, |r| {
        r.method == Method::Get && r.path.ends_with(&format!("/pulls/{n}"))
    });
    pull["requested_reviewers"] = json!([{"id": 7, "login": "ann", "full_name": "Ann"}]);
    pull["requested_reviewers_teams"] =
        json!([{"name": "core", "organization": {"username": "acme"}}]);
    let transport = Scripted::new(move |request| {
        let path = request.url.path();
        if path.ends_with("/version") {
            return json_response(200, &[], &json!({"version": "17.0.1"}));
        }
        if path.ends_with("/assignees") {
            let page: usize = request
                .url
                .query_pairs()
                .find(|(k, _)| k == "page")
                .and_then(|(_, v)| v.parse().ok())
                .unwrap_or(1);
            let rows: Vec<Value> = (50 * (page - 1)..(50 * page).min(130))
                .map(|i| json!({"id": i + 1, "login": format!("user{i:03}")}))
                .collect();
            return json_response(200, &[("x-total-count", "130".to_owned())], &json!(rows));
        }
        if path.ends_with(&format!("/pulls/{n}")) && request.method == Method::Get {
            return json_response(200, &[], &pull);
        }
        if path.ends_with("/reviews") {
            return json_response(200, &[], &json!([]));
        }
        if request.method == Method::Get {
            return json_response(404, &[], &json!({"message": "not found"}));
        }
        json_response(201, &[], &json!([]))
    });
    let forge = backend(ForgeKind::Forgejo, transport.clone()).await;
    let repo = sandbox(ForgeKind::Forgejo);

    let pr = forge
        .request_reviewers(&repo, n, &[User::new("ann")], &[team("acme/core")])
        .await
        .unwrap();
    let rows: Vec<_> = pr
        .reviewers
        .iter()
        .map(|r| (r.user.login.as_str(), r.is_team()))
        .collect();
    assert_eq!(rows, [("ann", false), ("acme/core", true)]);
    let post = transport
        .sent()
        .into_iter()
        .find(|r| r.method == Method::Post)
        .unwrap();
    assert_eq!(
        post.body,
        Some(json!({"reviewers": ["ann"], "team_reviewers": ["core"]}))
    );
    assert_eq!(post.retry, RetryMode::Never);

    let all = forge.collaborators(&repo, "").await.unwrap();
    assert_eq!(all.len(), COLLABORATOR_LIMIT);
    let some = forge.collaborators(&repo, "USER12").await.unwrap();
    let logins: Vec<&str> = some.iter().map(|u| u.login.as_str()).collect();
    assert_eq!(logins.len(), 10, "{logins:?}");
    assert!(logins.iter().all(|l| l.starts_with("user12")));
    let before = transport.sent().len();
    forge.collaborators(&repo, "user12").await.unwrap();
    assert_eq!(transport.sent().len(), before, "the list was not cached");
}
