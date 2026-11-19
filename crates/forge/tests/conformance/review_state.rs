//! Review state on every forge, from hand-built recordings: the sandboxes
//! hold no approvals, and a stale or re-requested approval needs a push or a
//! second request after the first review.
//!
//! What must hold everywhere: an approval given before the last push is
//! "approved before last push", someone asked again after reviewing is
//! "re-review requested", and neither makes the pull request "approved".

use std::sync::Arc;
use std::time::Duration;

use forge::Forge;
use forge::auth::{Credential, Scheme, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::gitlab::GitLab;
use forge::http::{HttpClient, HttpOptions, ManualClock, Method};
use forge::model::{
    ForgeKind, PrNumber, PullRequest, RepoRef, ReviewDecision, ReviewState, ReviewStatus, User,
};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
};
use serde_json::{Value, json};

/// What one reviewer row must read as.
struct Expect {
    state: ReviewState,
    requested_again: bool,
    stale: bool,
    status: ReviewStatus,
    decision: ReviewDecision,
}

fn assert_review(pr: &PullRequest, login: &str, expect: &Expect, case: &str) {
    let rows: Vec<&str> = pr.reviewers.iter().map(|r| r.user.login.as_str()).collect();
    let row = pr
        .reviewer(&User::new(login))
        .unwrap_or_else(|| panic!("{case}: no row for {login} in {rows:?}"));
    assert_eq!(
        (row.state, row.requested_again, row.stale),
        (expect.state, expect.requested_again, expect.stale),
        "{case}: the row"
    );
    assert_eq!(row.status(), expect.status, "{case}: the status");
    assert_eq!(
        pr.viewer_status(&User::new(login)),
        Some(expect.status),
        "{case}: the viewer's status"
    );
    assert_eq!(pr.review_decision, expect.decision, "{case}: the decision");
    assert_eq!(
        rows.iter().filter(|l| **l == login).count(),
        1,
        "{case}: one row"
    );
}

fn requested(again: bool, status: ReviewStatus) -> Expect {
    Expect {
        state: ReviewState::Requested,
        requested_again: again,
        stale: false,
        status,
        decision: ReviewDecision::ReviewRequired,
    }
}

fn approved() -> Expect {
    Expect {
        state: ReviewState::Approved,
        requested_again: false,
        stale: false,
        status: ReviewStatus::Approved,
        decision: ReviewDecision::Approved,
    }
}

fn stale() -> Expect {
    Expect {
        stale: true,
        status: ReviewStatus::StaleApproval,
        decision: ReviewDecision::StaleApproval,
        ..approved()
    }
}

fn dismissed() -> Expect {
    Expect {
        state: ReviewState::Dismissed,
        requested_again: false,
        stale: false,
        status: ReviewStatus::Dismissed,
        decision: ReviewDecision::ReviewRequired,
    }
}

fn exchange(
    method: Method,
    path: &str,
    query: &[(&str, &str)],
    operation: Option<(&str, Value)>,
    body: Value,
) -> Exchange {
    let mut query: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    query.sort();
    let (operation, variables, body_key, reply) = match operation {
        Some((name, variables)) => (
            Some(name.to_owned()),
            Some(variables),
            None,
            json!({"data": body}),
        ),
        None => (None, None, None, body),
    };
    Exchange {
        request: RecordedRequest {
            method,
            path: path.to_owned(),
            query,
            operation,
            variables,
            body: body_key,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: RecordedBody::Json(reply),
        },
    }
}

fn http(kind: ForgeKind, host: &str, scheme: Scheme, exchanges: Vec<Exchange>) -> HttpClient {
    let replay = Arc::new(ReplayTransport::new("review_state", Cassette { exchanges }));
    HttpClient::new(
        kind,
        host,
        replay,
        Arc::new(StaticCredential(Credential {
            token: Token::new("replay"),
            scheme,
        })),
        HttpOptions {
            clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
            ..HttpOptions::for_kind(kind)
        },
    )
}

// ---- GitLab ---------------------------------------------------------------

const GITLAB: &str = "gitlab.com";

fn gitlab_repo() -> RepoRef {
    RepoRef::new(GITLAB, "Lovelace", "review-sandbox")
}

/// A reviewer as GraphQL lists one: `reviewState`, and the time of the
/// approval on record if any.
fn gitlab_reviewer(login: &str, state: &str, approved_at: Option<&str>) -> Value {
    json!({"id": format!("gid://gitlab/User/{login}"), "username": login, "name": login,
    "avatarUrl": null, "bot": false,
    "mergeRequestInteraction": {
        "reviewState": state,
        "approved": approved_at.is_some(),
        "approvedAt": approved_at,
    }})
}

fn gitlab_user(login: &str) -> Value {
    json!({"id": format!("gid://gitlab/User/{login}"), "username": login, "name": login,
           "avatarUrl": null, "bot": false})
}

/// The merge request GraphQL answers with. `diffs` are the creation times of
/// its diff versions, newest first; the first is the last push.
fn gitlab_mr(reviewers: Vec<Value>, approved_by: &[&str], diffs: &[&str]) -> Value {
    json!({"project": {"mergeRequest": {
        "id": "gid://gitlab/MergeRequest/10", "iid": "10", "title": "t", "description": "",
        "draft": false, "state": "opened", "sourceBranch": "topic", "targetBranch": "main",
        "diffHeadSha": "b".repeat(40),
        "diffRefs": {"baseSha": "a".repeat(40), "startSha": "a".repeat(40), "headSha": "b".repeat(40)},
        "sourceProject": {"fullPath": "Lovelace/review-sandbox"},
        "targetProject": {"fullPath": "Lovelace/review-sandbox"},
        "createdAt": "2026-11-10T09:00:00Z", "updatedAt": "2026-11-17T12:00:00Z",
        "webUrl": "https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/10",
        "author": gitlab_user("author"),
        "reviewers": {"nodes": reviewers},
        "approvedBy": {"nodes": approved_by.iter().map(|l| gitlab_user(l)).collect::<Vec<_>>()},
        "mergeRequestDiffs": {"nodes": diffs.iter().map(|at| json!({"createdAt": at})).collect::<Vec<_>>()},
        "labels": {"nodes": []}, "assignees": {"nodes": []},
        "headPipeline": null,
    }}})
}

async fn gitlab_pull_request(data: Value) -> PullRequest {
    let exchanges = vec![exchange(
        Method::Post,
        "/api/graphql",
        &[],
        Some((
            "PullRequest",
            json!({"path": "Lovelace/review-sandbox", "iid": "10"}),
        )),
        data,
    )];
    let http = http(ForgeKind::GitLab, GITLAB, Scheme::Bearer, exchanges);
    let gitlab = GitLab::connect(http).await.unwrap();
    gitlab
        .pull_request(&gitlab_repo(), PrNumber(10))
        .await
        .unwrap()
}

const APPROVED_AT: &str = "2026-11-17T10:00:00Z";
const BEFORE: &str = "2026-11-17T09:00:00Z";
const AFTER: &str = "2026-11-17T11:00:00Z";

#[tokio::test]
async fn gitlab_review_states() {
    let approver = |state: &str, at: Option<&str>| vec![gitlab_reviewer("kit", state, at)];
    // reviewer rows, approvedBy, diff versions (newest first), expectation
    type Case<'a> = (&'a str, Vec<Value>, &'a [&'a str], &'a [&'a str], Expect);
    let cases: Vec<Case> = vec![
        (
            "a fresh approval",
            approver("APPROVED", Some(APPROVED_AT)),
            &["kit"],
            &[BEFORE],
            approved(),
        ),
        (
            "an approval older than the last push",
            approver("APPROVED", Some(APPROVED_AT)),
            &["kit"],
            &[AFTER, BEFORE],
            stale(),
        ),
        (
            "asked again after approving (approvedBy still holds the approval)",
            approver("UNREVIEWED", Some(APPROVED_AT)),
            &["kit"],
            &[AFTER, BEFORE],
            requested(true, ReviewStatus::ReReviewRequested),
        ),
        (
            "asked again, approval known only from approvedBy",
            approver("UNREVIEWED", None),
            &["kit"],
            &[BEFORE],
            requested(true, ReviewStatus::ReReviewRequested),
        ),
        (
            "asked for the first time",
            approver("UNREVIEWED", None),
            &[],
            &[BEFORE],
            requested(false, ReviewStatus::ReviewRequested),
        ),
        (
            "approved, then unapproved",
            approver("UNAPPROVED", None),
            &[],
            &[BEFORE],
            dismissed(),
        ),
    ];
    for (case, reviewers, approved_by, diffs, expect) in cases {
        let pr = gitlab_pull_request(gitlab_mr(reviewers, approved_by, diffs)).await;
        assert_review(&pr, "kit", &expect, case);
    }
}

#[tokio::test]
async fn gitlab_maps_every_review_state_and_an_approver_who_was_never_asked() {
    let reviewers = vec![
        gitlab_reviewer("a", "UNREVIEWED", None),
        gitlab_reviewer("b", "REVIEW_STARTED", None),
        gitlab_reviewer("c", "REVIEWED", None),
        gitlab_reviewer("d", "REQUESTED_CHANGES", None),
        gitlab_reviewer("e", "APPROVED", Some(APPROVED_AT)),
        gitlab_reviewer("f", "UNAPPROVED", None),
    ];
    let pr = gitlab_pull_request(gitlab_mr(reviewers, &["e", "walk-in"], &[BEFORE])).await;
    let states: Vec<(&str, ReviewState)> = pr
        .reviewers
        .iter()
        .map(|r| (r.user.login.as_str(), r.state))
        .collect();
    assert_eq!(
        states,
        [
            ("a", ReviewState::Requested),
            ("b", ReviewState::Pending),
            ("c", ReviewState::Commented),
            ("d", ReviewState::ChangesRequested),
            ("e", ReviewState::Approved),
            ("f", ReviewState::Dismissed),
            ("walk-in", ReviewState::Approved),
        ]
    );
    assert_eq!(pr.review_decision, ReviewDecision::ChangesRequested);

    // Without a timestamp the approval cannot be called stale.
    let walk_in = pr.reviewer(&User::new("walk-in")).unwrap();
    assert!(!walk_in.stale && walk_in.is_current_approval());
}

#[tokio::test]
async fn gitlab_decision_is_not_approved_while_the_only_approver_is_asked_again() {
    // The user's case: approved, the author pushed and asked again. GitLab's
    // `approved` and `approvedBy` still say so; the reviewer row does not.
    let pr = gitlab_pull_request(gitlab_mr(
        vec![gitlab_reviewer("kit", "UNREVIEWED", Some(APPROVED_AT))],
        &["kit"],
        &[AFTER, BEFORE],
    ))
    .await;
    assert_eq!(pr.review_decision, ReviewDecision::ReviewRequired);
    assert!(pr.reviewers.iter().all(|r| !r.is_current_approval()));
}

// ---- GitHub ---------------------------------------------------------------

mod github {
    use super::*;
    use crate::github::{on_pr, pr_json, scripted};

    fn actor(login: &str) -> Value {
        json!({"__typename": "User", "login": login, "avatarUrl": null,
               "id": format!("U_{login}"), "name": null})
    }

    /// `pr_json` with its own reviewers swapped for these (head is `b`*40).
    fn node(decision: Option<&str>, reviews: Vec<Value>, requests: Vec<Value>) -> Value {
        let mut node = pr_json(7, "topic", "main");
        node["reviewDecision"] = json!(decision);
        node["latestReviews"] = json!({"nodes": reviews});
        node["reviewRequests"] = json!({"nodes": requests});
        node
    }

    fn review(login: &str, state: &str, commit: &str) -> Value {
        json!({"state": state, "author": actor(login), "commit": {"oid": commit.repeat(40)}})
    }

    fn request(login: &str) -> Value {
        json!({"requestedReviewer": {"__typename": "User", "id": format!("U_{login}"),
               "login": login, "name": null, "avatarUrl": null}})
    }

    async fn read(node: Value) -> PullRequest {
        let github = scripted(move |_, _| on_pr(node.clone())).await;
        github
            .pull_request(&crate::common::repo(), PrNumber(7))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn review_states() {
        let cases: Vec<(&str, Value, Expect)> = vec![
            (
                "a fresh approval",
                node(
                    Some("APPROVED"),
                    vec![review("kit", "APPROVED", "b")],
                    vec![],
                ),
                approved(),
            ),
            (
                "an approval of an earlier commit",
                node(
                    Some("APPROVED"),
                    vec![review("kit", "APPROVED", "a")],
                    vec![],
                ),
                stale(),
            ),
            (
                "the same with no required-review rule",
                node(None, vec![review("kit", "APPROVED", "a")], vec![]),
                stale(),
            ),
            (
                "asked again after approving the head",
                node(
                    Some("APPROVED"),
                    vec![review("kit", "APPROVED", "b")],
                    vec![request("kit")],
                ),
                requested(true, ReviewStatus::ReReviewRequested),
            ),
            (
                "asked again after approving an earlier commit",
                node(
                    None,
                    vec![review("kit", "APPROVED", "a")],
                    vec![request("kit")],
                ),
                requested(true, ReviewStatus::ReReviewRequested),
            ),
            (
                "asked for the first time",
                node(Some("REVIEW_REQUIRED"), vec![], vec![request("kit")]),
                requested(false, ReviewStatus::ReviewRequested),
            ),
            (
                "an approval that was dismissed",
                node(
                    Some("REVIEW_REQUIRED"),
                    vec![review("kit", "DISMISSED", "b")],
                    vec![],
                ),
                dismissed(),
            ),
        ];
        for (case, node, expect) in cases {
            let pr = read(node).await;
            assert_review(&pr, "kit", &expect, case);
        }
    }

    #[tokio::test]
    async fn a_fresh_approval_beside_a_stale_one_still_approves() {
        let pr = read(node(
            Some("APPROVED"),
            vec![
                review("kit", "APPROVED", "a"),
                review("lee", "APPROVED", "b"),
            ],
            vec![],
        ))
        .await;
        assert_eq!(pr.review_decision, ReviewDecision::Approved);
        assert_eq!(
            pr.viewer_status(&User::new("kit")),
            Some(ReviewStatus::StaleApproval)
        );
        assert_eq!(
            pr.viewer_status(&User::new("lee")),
            Some(ReviewStatus::Approved)
        );
    }
}

// ---- Forgejo --------------------------------------------------------------

mod forgejo {
    use super::*;

    const HOST: &str = "git.example.invalid";
    const PULLS: &str = "/api/v1/repos/cora/review-sandbox/pulls/7";

    fn api_user(login: &str) -> Value {
        json!({"id": 1, "login": login, "full_name": ""})
    }

    fn api_review(id: u64, login: &str, state: &str, stale: bool, dismissed: bool) -> Value {
        json!({"id": id, "user": api_user(login), "state": state, "stale": stale,
               "dismissed": dismissed, "commit_id": "abc",
               "submitted_at": "2026-11-17T10:00:00Z"})
    }

    async fn read(requested: Vec<Value>, reviews: Vec<Value>) -> PullRequest {
        let get = |path: &str, query: &[(&str, &str)], body: Value| {
            exchange(Method::Get, path, query, None, body)
        };
        let exchanges = vec![
            get(
                "/api/v1/version",
                &[],
                json!({"version": "17.0.1+gitea-1.22.0"}),
            ),
            get(
                PULLS,
                &[],
                json!({"id": 7, "number": 7, "title": "t", "state": "open",
                       "user": api_user("author"),
                       "head": {"ref": "topic", "sha": "abc"}, "base": {"ref": "main", "sha": "def"},
                       "requested_reviewers": requested}),
            ),
            get(
                &format!("{PULLS}/reviews"),
                &[("limit", "50"), ("page", "1")],
                json!(reviews),
            ),
            get(
                "/api/v1/repos/cora/review-sandbox/commits/abc/status",
                &[],
                json!({"state": "", "total_count": 0}),
            ),
        ];
        let http = http(ForgeKind::Forgejo, HOST, Scheme::Token, exchanges);
        let forgejo = Forgejo::connect(http).await.unwrap();
        forgejo
            .pull_request(&RepoRef::new(HOST, "cora", "review-sandbox"), PrNumber(7))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn review_states() {
        let cases: Vec<(&str, Vec<Value>, Vec<Value>, Expect)> = vec![
            (
                "a fresh approval",
                vec![],
                vec![api_review(1, "kit", "APPROVED", false, false)],
                approved(),
            ),
            (
                "an approval the forge calls stale",
                vec![],
                vec![api_review(1, "kit", "APPROVED", true, false)],
                stale(),
            ),
            (
                "a request newer than the approval",
                vec![api_user("kit")],
                vec![
                    api_review(1, "kit", "APPROVED", true, false),
                    api_review(2, "kit", "REQUEST_REVIEW", false, false),
                ],
                requested(true, ReviewStatus::ReReviewRequested),
            ),
            (
                "still listed as requested after an approval",
                vec![api_user("kit")],
                vec![api_review(1, "kit", "APPROVED", false, false)],
                requested(true, ReviewStatus::ReReviewRequested),
            ),
            (
                "asked for the first time",
                vec![api_user("kit")],
                vec![api_review(1, "kit", "REQUEST_REVIEW", false, false)],
                requested(false, ReviewStatus::ReviewRequested),
            ),
            (
                "an approval that was dismissed",
                vec![],
                vec![api_review(1, "kit", "APPROVED", false, true)],
                dismissed(),
            ),
        ];
        for (case, requested, reviews, expect) in cases {
            let pr = read(requested, reviews).await;
            assert_review(&pr, "kit", &expect, case);
        }
    }
}
