//! The pull request lifecycle against recordings of one live run per forge
//! (`tests/fixtures/<forge>/lifecycle.json`).
//!
//! Record: `REVIEW_RECORD=1 cargo run -p forge --example lifecycle --
//! --forge <github|gitlab|forgejo>` (it opens, merges and closes scratch pull
//! requests on `review-sandbox`). These tests only replay: `ReplayTransport`
//! has no network, and a request without a recording is a `ReplayMiss`.
//!
//! What a recording cannot hold is covered by editing one: auto-merge (which
//! can merge at once, so it is not run live) and a head branch in a fork (the
//! sandbox has none).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use forge::auth::{Credential, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::github::GitHub;
use forge::gitlab::GitLab;
use forge::http::{
    HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, RetryMode, Transport,
};
use forge::model::{ForgeKind, MergeMethod, MergeOptions, PrNumber, PrState, RepoRef, Sha};
use forge::record::{Cassette, Exchange, RecordedBody, ReplayTransport, fixture_path};
use forge::{Error, Forge};
use serde_json::{Value, json};

use super::lifecycle_steps::{self as steps, Hooks, Scratch, Status};

pub(super) fn sandbox(kind: ForgeKind) -> RepoRef {
    match kind {
        ForgeKind::GitHub => RepoRef::new("github.com", "Lovelace", "review-sandbox"),
        ForgeKind::GitLab => RepoRef::new("gitlab.com", "Lovelace", "review-sandbox"),
        ForgeKind::Forgejo => RepoRef::new("forgejo.example.com", "cora", "review-sandbox"),
    }
}

fn cassette(kind: ForgeKind) -> Cassette {
    cassette_of(kind, "lifecycle")
}

pub(super) fn cassette_of(kind: ForgeKind, name: &str) -> Cassette {
    let path = fixture_path(kind, name);
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// What one replayed request looked like.
#[derive(Clone, Debug)]
pub(super) struct Sent {
    pub(super) method: Method,
    pub(super) path: String,
    pub(super) write: bool,
    pub(super) retry: RetryMode,
}

/// Replays a cassette and keeps what it was sent.
pub(super) struct Counting {
    pub(super) replay: ReplayTransport,
    sent: Mutex<Vec<Sent>>,
}

impl Counting {
    fn new(cassette: Cassette) -> Arc<Self> {
        Self::named("lifecycle", cassette)
    }

    pub(super) fn named(name: &str, cassette: Cassette) -> Arc<Self> {
        Arc::new(Self {
            replay: ReplayTransport::new(name, cassette),
            sent: Mutex::default(),
        })
    }

    pub(super) fn sent(&self) -> Vec<Sent> {
        self.sent.lock().unwrap().clone()
    }
}

#[async_trait]
impl Transport for Counting {
    async fn send(&self, request: &HttpRequest) -> forge::Result<HttpResponse> {
        let mutation = request
            .body
            .as_ref()
            .and_then(|body| body.get("query")?.as_str())
            .is_some_and(|query| query.starts_with("mutation "));
        let graphql_query = request
            .body
            .as_ref()
            .is_some_and(|b| b.get("query").is_some());
        self.sent.lock().unwrap().push(Sent {
            method: request.method,
            path: request.url.path().to_owned(),
            write: mutation || (request.method != Method::Get && !graphql_query),
            retry: request.retry,
        });
        self.replay.send(request).await
    }
}

fn options(kind: ForgeKind) -> HttpOptions {
    HttpOptions {
        clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
        ..HttpOptions::for_kind(kind)
    }
}

pub(super) fn client(kind: ForgeKind, transport: Arc<dyn Transport>) -> HttpClient {
    let token = Token::new("replay");
    let credential = match kind {
        ForgeKind::Forgejo => Credential {
            token,
            scheme: forge::auth::Scheme::Token,
        },
        ForgeKind::GitHub | ForgeKind::GitLab => Credential::bearer(token),
    };
    HttpClient::new(
        kind,
        sandbox(kind).host,
        transport,
        Arc::new(StaticCredential(credential)),
        options(kind),
    )
}

pub(super) async fn backend(kind: ForgeKind, transport: Arc<dyn Transport>) -> Arc<dyn Forge> {
    let http = client(kind, transport);
    match kind {
        ForgeKind::GitHub => Arc::new(GitHub::connect(http).await.unwrap()),
        ForgeKind::GitLab => Arc::new(
            GitLab::connect(http)
                .await
                .unwrap()
                .with_settle_interval(Duration::ZERO),
        ),
        ForgeKind::Forgejo => Arc::new(Forgejo::connect(http).await.unwrap()),
    }
}

/// The pull request numbers a recording names, in order of first appearance:
/// the one that was merged, then the one that was closed.
fn scratch_numbers(cassette: &Cassette) -> Scratch {
    let mut seen: Vec<u64> = Vec::new();
    for exchange in &cassette.exchanges {
        let request = &exchange.request;
        let from_variables = request.variables.as_ref().and_then(|vars| {
            let number = vars.get("number").and_then(Value::as_u64);
            number.or_else(|| vars.get("iid")?.as_str()?.parse().ok())
        });
        let from_path = ["/pulls/", "/merge_requests/"].iter().find_map(|marker| {
            let rest = request.path.split(marker).nth(1)?;
            rest.split('/').next()?.parse().ok()
        });
        if let Some(number) = from_variables.or(from_path)
            && !seen.contains(&number)
        {
            seen.push(number);
        }
    }
    assert!(
        seen.len() >= 2,
        "the recording names two pull requests: {seen:?}"
    );
    Scratch {
        merged: PrNumber(seen[0]),
        closed: PrNumber(seen[1]),
    }
}

struct Replayed {
    outcomes: Vec<steps::Outcome>,
    transport: Arc<Counting>,
}

async fn replay(kind: ForgeKind) -> Replayed {
    let cassette = cassette(kind);
    let transport = Counting::new(cassette.clone());
    let forge = backend(kind, transport.clone()).await;
    let gone = |_: &str| Ok(false);
    let hooks = Hooks {
        branch_exists: &gone,
        settle: Duration::ZERO,
    };
    let outcomes = steps::run(
        forge.as_ref(),
        &sandbox(kind),
        scratch_numbers(&cassette),
        &hooks,
        |_| {},
    )
    .await;
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

fn head_sha(cassette: &Cassette) -> String {
    // The merge that was not the stale one names the real head.
    let mut shas: Vec<String> = cassette
        .exchanges
        .iter()
        .filter_map(|e| {
            let request = &e.request;
            let value = request.variables.as_ref().or(request.body.as_ref())?;
            let sha = value
                .get("expectedHeadOid")
                .or_else(|| value.get("sha"))
                .or_else(|| value.get("head_commit_id"))?;
            sha.as_str().map(str::to_owned)
        })
        .collect();
    shas.retain(|sha| sha.chars().any(|c| c != '0'));
    shas.pop().expect("a merge names the head it expects")
}

fn squash(sha: &str) -> MergeOptions {
    MergeOptions {
        commit_title: Some(steps::MERGE_TITLE.to_owned()),
        commit_message: Some(steps::MERGE_BODY.to_owned()),
        delete_branch: true,
        ..MergeOptions::new(MergeMethod::Squash, Sha::new(sha))
    }
}

/// Every exchange whose request is `f`'s, changed.
fn edit(cassette: &mut Cassette, mut f: impl FnMut(&mut Exchange)) {
    cassette.exchanges.iter_mut().for_each(&mut f);
}

fn response_json(exchange: &mut Exchange) -> Option<&mut Value> {
    match &mut exchange.response.body {
        RecordedBody::Json(value) => Some(value),
        RecordedBody::Empty | RecordedBody::Text(_) => None,
    }
}

// ---- GitHub -------------------------------------------------------------------

#[tokio::test]
async fn github_replays_with_every_step_passing() {
    let replayed = replay(ForgeKind::GitHub).await;
    assert_passes(&replayed, &["auto-merge"]);
}

#[test]
fn github_sends_the_named_mutations_in_order() {
    let cassette = cassette(ForgeKind::GitHub);
    let mutations = [
        "ConvertPullRequestToDraft",
        "MarkPullRequestReadyForReview",
        "MergePullRequest",
        "ClosePullRequest",
        "ReopenPullRequest",
    ];
    let sent: Vec<(&str, &Value)> = cassette
        .exchanges
        .iter()
        .filter_map(|e| {
            Some((
                e.request.operation.as_deref()?,
                e.request.variables.as_ref()?,
            ))
        })
        .filter(|(op, _)| mutations.contains(op))
        .collect();
    let ops: Vec<&str> = sent.iter().map(|(op, _)| *op).collect();
    assert_eq!(
        ops,
        [
            "ConvertPullRequestToDraft",
            "MarkPullRequestReadyForReview",
            "MergePullRequest",
            "MergePullRequest",
            "ClosePullRequest",
            "ReopenPullRequest",
            "ClosePullRequest",
        ]
    );
    let (_, stale) = sent[2];
    assert_eq!(stale["expectedHeadOid"], "0".repeat(40));
    assert_eq!(stale["mergeMethod"], "MERGE");
    let (_, real) = sent[3];
    assert_eq!(real["mergeMethod"], "SQUASH");
    assert_eq!(real["commitHeadline"], steps::MERGE_TITLE);
    assert_eq!(real["commitBody"], steps::MERGE_BODY);
    assert_eq!(real["expectedHeadOid"].as_str().unwrap().len(), 40);
    assert!(real["pullRequestId"].as_str().unwrap().starts_with("PR_"));
    let deletes: Vec<&str> = cassette
        .exchanges
        .iter()
        .filter(|e| e.request.method == Method::Delete)
        .map(|e| e.request.path.as_str())
        .collect();
    assert_eq!(deletes.len(), 1, "{deletes:?}");
    assert!(
        deletes[0].starts_with("/repos/Lovelace/review-sandbox/git/refs/heads/lifecycle/github-"),
        "{deletes:?}"
    );
}

/// The recorded merge, with the pull request's head branch in `head`.
fn github_merge_with_head_repo(head: &str) -> Cassette {
    let mut cassette = cassette(ForgeKind::GitHub);
    edit(&mut cassette, |exchange| {
        if exchange.request.operation.as_deref() != Some("MergePullRequest") {
            return;
        }
        let Some(value) = response_json(exchange) else {
            return;
        };
        if let Some(repo) = value.pointer_mut("/data/mergePullRequest/pullRequest/headRepository") {
            *repo = json!({"nameWithOwner": head});
        }
    });
    cassette
}

async fn github_merge(cassette: Cassette, delete: bool) -> (PrState, Vec<Sent>) {
    let number = scratch_numbers(&cassette).merged;
    let sha = head_sha(&cassette);
    let transport = Counting::new(cassette);
    let forge = backend(ForgeKind::GitHub, transport.clone()).await;
    let options = MergeOptions {
        delete_branch: delete,
        ..squash(&sha)
    };
    let merged = forge
        .merge(&sandbox(ForgeKind::GitHub), number, &options)
        .await
        .unwrap();
    (merged.state, transport.sent())
}

#[tokio::test]
async fn github_deletes_only_a_branch_of_the_same_repository_and_only_when_asked() {
    let deletes = |sent: &[Sent]| sent.iter().filter(|s| s.method == Method::Delete).count();
    let (state, sent) = github_merge(cassette(ForgeKind::GitHub), true).await;
    assert_eq!(state, PrState::Merged);
    assert_eq!(deletes(&sent), 1);
    let delete = sent.iter().find(|s| s.method == Method::Delete).unwrap();
    assert_eq!(delete.retry, RetryMode::Never);

    let (_, sent) = github_merge(cassette(ForgeKind::GitHub), false).await;
    assert_eq!(deletes(&sent), 0, "not asked");

    let (_, sent) = github_merge(github_merge_with_head_repo("someone/fork"), true).await;
    assert_eq!(deletes(&sent), 0, "a fork's branch is not ours");
}

#[tokio::test]
async fn github_auto_merge_sends_the_same_input_as_a_merge() {
    let recorded = cassette(ForgeKind::GitHub);
    let number = scratch_numbers(&recorded).merged;
    let sha = head_sha(&recorded);
    let merge = recorded
        .exchanges
        .iter()
        .rfind(|e| e.request.operation.as_deref() == Some("MergePullRequest"))
        .unwrap()
        .clone();
    let id = recorded
        .exchanges
        .iter()
        .find(|e| e.request.operation.as_deref() == Some("PullRequestId"))
        .unwrap()
        .clone();
    let renamed = |operation: &str, field: &str, from: &Exchange, variables: Value| {
        let mut exchange = from.clone();
        exchange.request.operation = Some(operation.to_owned());
        exchange.request.variables = Some(variables);
        let value = response_json(&mut exchange).unwrap();
        let payload = value["data"]["mergePullRequest"].take();
        value["data"] = json!({ field: payload });
        exchange
    };
    let enable = renamed(
        "EnablePullRequestAutoMerge",
        "enablePullRequestAutoMerge",
        &merge,
        merge.request.variables.clone().unwrap(),
    );
    let pull_request_id = merge.request.variables.as_ref().unwrap()["pullRequestId"].clone();
    let disable = renamed(
        "DisablePullRequestAutoMerge",
        "disablePullRequestAutoMerge",
        &merge,
        json!({ "pullRequestId": pull_request_id }),
    );
    let transport = Counting::new(Cassette {
        exchanges: vec![id, enable, disable],
    });
    let forge = backend(ForgeKind::GitHub, transport.clone()).await;
    let repo = sandbox(ForgeKind::GitHub);
    forge
        .set_auto_merge(&repo, number, Some(&squash(&sha)))
        .await
        .unwrap();
    forge.set_auto_merge(&repo, number, None).await.unwrap();
    assert_eq!(transport.replay.unused(), Vec::<String>::new());
    assert!(
        transport
            .sent()
            .iter()
            .filter(|s| s.write)
            .all(|s| s.retry == RetryMode::Never)
    );
}

// ---- GitLab -------------------------------------------------------------------

#[tokio::test]
async fn gitlab_replays_with_every_step_passing() {
    let replayed = replay(ForgeKind::GitLab).await;
    assert_passes(&replayed, &["auto-merge"]);
}

fn rest_writes(cassette: &Cassette) -> Vec<(Method, String, Value)> {
    cassette
        .exchanges
        .iter()
        .map(|e| &e.request)
        .filter(|r| r.method != Method::Get && r.operation.is_none())
        .map(|r| {
            let tail = r.path.split_once("/merge_requests/").unwrap().1;
            let tail = tail
                .split_once('/')
                .map_or(String::new(), |(_, t)| t.to_owned());
            (r.method, tail, r.body.clone().unwrap_or(Value::Null))
        })
        .collect()
}

#[test]
fn gitlab_sends_the_draft_mutation_and_the_merge_calls_in_order() {
    let cassette = cassette(ForgeKind::GitLab);
    let drafts: Vec<&Value> = cassette
        .exchanges
        .iter()
        .filter(|e| e.request.operation.as_deref() == Some("MergeRequestSetDraft"))
        .map(|e| &e.request.variables.as_ref().unwrap()["draft"])
        .collect();
    assert_eq!(drafts, [&json!(true), &json!(false)]);

    let writes = rest_writes(&cassette);
    let calls: Vec<(Method, &str)> = writes.iter().map(|(m, t, _)| (*m, t.as_str())).collect();
    assert_eq!(
        calls,
        [
            (Method::Put, "merge"),
            (Method::Put, "merge"),
            (Method::Put, ""),
            (Method::Put, ""),
            (Method::Put, ""),
        ]
    );
    let zeros = "0".repeat(40);
    assert_eq!(
        writes[0].2,
        json!({"sha": zeros, "squash": false, "should_remove_source_branch": false})
    );
    let real = &writes[1].2;
    assert_eq!(real["squash"], true);
    assert_eq!(real["should_remove_source_branch"], true);
    let message = format!("{}\n\n{}", steps::MERGE_TITLE, steps::MERGE_BODY);
    assert_eq!(real["squash_commit_message"], message);
    assert_eq!(real["merge_commit_message"], message);
    let states: Vec<&Value> = writes[2..]
        .iter()
        .map(|(_, _, body)| &body["state_event"])
        .collect();
    assert_eq!(states, [&json!("close"), &json!("reopen"), &json!("close")]);
}

/// The recording with the merge request's source project changed to `source`
/// and, when it is not the target, the recorded merge call asking not to
/// delete the branch (what the backend must send then).
fn gitlab_with_source(source: &str) -> Cassette {
    let mut cassette = cassette(ForgeKind::GitLab);
    edit(&mut cassette, |exchange| {
        if let Some(value) = response_json(exchange)
            && let Some(path) =
                value.pointer_mut("/data/project/mergeRequest/sourceProject/fullPath")
        {
            *path = json!(source);
        }
        if exchange.request.method == Method::Put
            && exchange.request.path.ends_with("/merge")
            && let Some(body) = exchange.request.body.as_mut()
            && body["squash"] == true
        {
            body["should_remove_source_branch"] = json!(false);
        }
    });
    cassette
}

async fn gitlab_merge(cassette: Cassette, delete: bool) {
    let number = scratch_numbers(&cassette).merged;
    let sha = head_sha(&cassette);
    let transport = Counting::new(cassette);
    let forge = backend(ForgeKind::GitLab, transport.clone()).await;
    let options = MergeOptions {
        delete_branch: delete,
        ..squash(&sha)
    };
    forge
        .merge(&sandbox(ForgeKind::GitLab), number, &options)
        .await
        .unwrap();
}

#[tokio::test]
async fn gitlab_asks_to_remove_the_branch_only_for_the_same_project_and_when_asked() {
    // Not asked: the recorded call says `true`, so this one must miss.
    let mut recorded = cassette(ForgeKind::GitLab);
    edit(&mut recorded, |exchange| {
        if exchange.request.method == Method::Put
            && exchange.request.path.ends_with("/merge")
            && let Some(body) = exchange.request.body.as_mut()
            && body["squash"] == true
        {
            body["should_remove_source_branch"] = json!(false);
        }
    });
    gitlab_merge(recorded, false).await;
    // A fork: asked, but the call says `false`.
    gitlab_merge(gitlab_with_source("someone/fork"), true).await;
    // The same project and asked: the unedited recording.
    gitlab_merge(cassette(ForgeKind::GitLab), true).await;
}

#[tokio::test]
async fn gitlab_refuses_a_method_the_project_does_not_make() {
    let recorded = cassette(ForgeKind::GitLab);
    let number = scratch_numbers(&recorded).merged;
    let transport = Counting::new(recorded);
    let forge = backend(ForgeKind::GitLab, transport.clone()).await;
    let rebase = MergeOptions::new(MergeMethod::Rebase, Sha::new("a".repeat(40)));
    let err = forge
        .merge(&sandbox(ForgeKind::GitLab), number, &rebase)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
    assert!(
        !transport.sent().iter().any(|s| s.write),
        "nothing was written"
    );
}

#[tokio::test]
async fn gitlab_auto_merge_sets_the_flag_and_cancels_it() {
    let recorded = cassette(ForgeKind::GitLab);
    let number = scratch_numbers(&recorded).merged;
    let sha = head_sha(&recorded);
    let mut enable = recorded
        .exchanges
        .iter()
        .rfind(|e| e.request.method == Method::Put && e.request.path.ends_with("/merge"))
        .unwrap()
        .clone();
    enable.request.body.as_mut().unwrap()["auto_merge"] = json!(true);
    let mut cancel = enable.clone();
    cancel.request.method = Method::Post;
    let merge_path = cancel
        .request
        .path
        .strip_suffix("/merge")
        .unwrap()
        .to_owned();
    cancel.request.path = format!("{merge_path}/cancel_merge_when_pipeline_succeeds");
    cancel.request.body = None;
    let mut exchanges: Vec<Exchange> = recorded
        .exchanges
        .iter()
        .filter(|e| {
            e.request.method == Method::Get && e.request.path.ends_with("review-sandbox")
                || e.request.operation.as_deref() == Some("PullRequest")
        })
        .cloned()
        .collect();
    exchanges.extend([enable, cancel]);
    let transport = Counting::new(Cassette { exchanges });
    let forge = backend(ForgeKind::GitLab, transport.clone()).await;
    let repo = sandbox(ForgeKind::GitLab);
    forge
        .set_auto_merge(&repo, number, Some(&squash(&sha)))
        .await
        .unwrap();
    forge.set_auto_merge(&repo, number, None).await.unwrap();
    let paths: Vec<String> = transport
        .sent()
        .into_iter()
        .filter(|s| s.write)
        .map(|s| s.path)
        .collect();
    assert!(paths[0].ends_with("/merge"), "{paths:?}");
    assert!(
        paths[1].ends_with("/cancel_merge_when_pipeline_succeeds"),
        "{paths:?}"
    );
}

// ---- Forgejo ------------------------------------------------------------------

#[tokio::test]
async fn forgejo_replays_with_every_step_passing() {
    let replayed = replay(ForgeKind::Forgejo).await;
    assert_passes(&replayed, &["auto-merge"]);
}

#[test]
fn forgejo_sends_the_title_prefix_the_merge_and_the_state_calls_in_order() {
    let cassette = cassette(ForgeKind::Forgejo);
    let writes: Vec<(Method, &str, &Value)> = cassette
        .exchanges
        .iter()
        .map(|e| &e.request)
        .filter(|r| r.method != Method::Get)
        .map(|r| {
            let tail = r.path.split_once("/review-sandbox/pulls/").unwrap().1;
            let tail = tail.split_once('/').map_or("", |(_, t)| t);
            (r.method, tail, r.body.as_ref().unwrap())
        })
        .collect();
    let calls: Vec<(Method, &str)> = writes.iter().map(|(m, t, _)| (*m, *t)).collect();
    assert_eq!(
        calls,
        [
            (Method::Patch, ""),
            (Method::Patch, ""),
            (Method::Post, "merge"),
            (Method::Post, "merge"),
            (Method::Patch, ""),
            (Method::Patch, ""),
            (Method::Patch, ""),
        ]
    );
    let title = writes[0].2["title"].as_str().unwrap();
    assert!(title.starts_with("WIP: "), "{title}");
    assert_eq!(writes[1].2["title"], title.trim_start_matches("WIP: "));
    let zeros = "0".repeat(40);
    assert_eq!(
        writes[2].2,
        &json!({"Do": "merge", "head_commit_id": zeros, "delete_branch_after_merge": false})
    );
    let real = writes[3].2;
    assert_eq!(real["Do"], "squash");
    assert_eq!(real["MergeTitleField"], steps::MERGE_TITLE);
    assert_eq!(real["MergeMessageField"], steps::MERGE_BODY);
    assert_eq!(real["delete_branch_after_merge"], true);
    let states: Vec<&Value> = writes[4..]
        .iter()
        .map(|(_, _, body)| &body["state"])
        .collect();
    assert_eq!(states, [&json!("closed"), &json!("open"), &json!("closed")]);
}

#[tokio::test]
async fn forgejo_deletes_only_a_branch_of_the_same_repository_and_only_when_asked() {
    let recorded = cassette(ForgeKind::Forgejo);
    let number = scratch_numbers(&recorded).merged;
    let sha = head_sha(&recorded);
    let delete_flag = |exchange: &mut Exchange, flag: bool| {
        if exchange.request.method == Method::Post
            && exchange.request.path.ends_with("/merge")
            && let Some(body) = exchange.request.body.as_mut()
            && body["Do"] == "squash"
        {
            body["delete_branch_after_merge"] = json!(flag);
        }
    };
    let merge = |cassette: Cassette, delete: bool| {
        let sha = sha.clone();
        async move {
            let transport = Counting::new(cassette);
            let forge = backend(ForgeKind::Forgejo, transport).await;
            let options = MergeOptions {
                delete_branch: delete,
                ..squash(&sha)
            };
            forge
                .merge(&sandbox(ForgeKind::Forgejo), number, &options)
                .await
                .unwrap();
        }
    };
    merge(recorded.clone(), true).await;
    let mut not_asked = recorded.clone();
    edit(&mut not_asked, |e| delete_flag(e, false));
    merge(not_asked, false).await;
    // A fork: asked, but the call must say false.
    let mut fork = recorded;
    edit(&mut fork, |exchange| {
        delete_flag(exchange, false);
        if exchange.request.method == Method::Get
            && let Some(value) = response_json(exchange)
            && let Some(owner) = value.pointer_mut("/head/repo/owner/login")
        {
            *owner = json!("someone");
        }
    });
    merge(fork, true).await;
}

#[tokio::test]
async fn forgejo_has_no_auto_merge() {
    let recorded = cassette(ForgeKind::Forgejo);
    let number = scratch_numbers(&recorded).merged;
    let forge = backend(ForgeKind::Forgejo, Counting::new(recorded)).await;
    assert!(!forge.capabilities().auto_merge);
    let err = forge
        .set_auto_merge(&sandbox(ForgeKind::Forgejo), number, None)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Unsupported { .. }), "{err:?}");
}
