//! The stack fixtures (`tests/fixtures/stacks.json`): native stacks as
//! GitHub and GitLab answer them, and base-branch chains with a fork, a
//! closed member, two children and a cycle. `cargo test -p forge stack`
//! runs them with the backends' own stack tests.

use std::sync::Arc;
use std::time::Duration;

use forge::auth::{Credential, StaticCredential, Token};
use forge::gitlab::GitLab;
use forge::http::{HttpClient, HttpOptions, ManualClock, Method};
use forge::model::{ForgeKind, PrNumber, PrState, RepoRef, Stack, StackSource};
use forge::record::{
    Cassette, Exchange, RecordedBody, RecordedRequest, RecordedResponse, ReplayTransport,
};
use forge::{Forge, stack_by_base_branch};
use serde::Deserialize;
use serde_json::{Value, json};

use super::common::{pr, repo};
use super::github::{on_pr, page, pr_json, scripted};

#[derive(Deserialize)]
struct Fixtures {
    github_native: Vec<GitHubCase>,
    gitlab_native: Vec<GitLabCase>,
    base_branch: Vec<BaseBranchCase>,
}

#[derive(Deserialize)]
struct GitHubCase {
    name: String,
    target: u64,
    head: String,
    base: String,
    entries: Vec<GitHubEntry>,
    expected: Vec<u64>,
}

#[derive(Clone, Deserialize)]
struct GitHubEntry {
    position: u32,
    number: u64,
    state: String,
    draft: bool,
    head: String,
    base: String,
}

#[derive(Deserialize)]
struct GitLabCase {
    name: String,
    target: u64,
    stack: Vec<GitLabEntry>,
    expected: Vec<u64>,
}

#[derive(Deserialize)]
struct GitLabEntry {
    iid: String,
    state: String,
    head: String,
    base: String,
}

#[derive(Deserialize)]
struct BaseBranchCase {
    name: String,
    target: u64,
    prs: Vec<CasePr>,
    expected: Vec<u64>,
}

#[derive(Deserialize)]
struct CasePr {
    number: u64,
    head: String,
    base: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    fork: bool,
}

fn fixtures() -> Fixtures {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/stacks.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn numbers(stack: &Stack) -> Vec<u64> {
    stack.links.iter().map(|link| link.number.0).collect()
}

/// Every link sits on the one below it.
fn assert_chained(stack: &Stack, name: &str) {
    for pair in stack.links.windows(2) {
        assert_eq!(pair[1].base, pair[0].head, "{name}: {stack:?}");
    }
}

#[test]
fn stack_fixtures_by_base_branch_have_no_differences() {
    let mut differences = Vec::new();
    for case in fixtures().base_branch {
        let prs: Vec<_> = case
            .prs
            .iter()
            .map(|spec| {
                let mut pr = pr(spec.number, &spec.head, &spec.base);
                if spec.fork {
                    pr.head.repo = Some(RepoRef::new("github.com", "someone", "review-sandbox"));
                }
                if spec.state.as_deref() == Some("closed") {
                    pr.state = PrState::Closed;
                }
                pr
            })
            .collect();
        let target = prs
            .iter()
            .find(|pr| pr.number.0 == case.target)
            .expect("the target is among the fixture's pull requests");
        let stack = stack_by_base_branch(target, &prs);
        assert_eq!(stack.source, StackSource::BaseBranch);
        assert_chained(&stack, &case.name);
        if numbers(&stack) != case.expected {
            differences.push(format!(
                "{}: got {:?}, want {:?}",
                case.name,
                numbers(&stack),
                case.expected
            ));
        }
    }
    assert!(differences.is_empty(), "{differences:#?}");
}

fn entry_json(entry: &GitHubEntry) -> Value {
    json!({"position": entry.position, "pullRequest": {
        "number": entry.number, "title": format!("Change {}", entry.number),
        "state": entry.state, "isDraft": entry.draft,
        "headRefName": entry.head, "baseRefName": entry.base}})
}

#[tokio::test]
async fn stack_fixtures_native_on_github() {
    for case in fixtures().github_native {
        let GitHubCase {
            name,
            target,
            head,
            base,
            entries,
            expected,
        } = case;
        let native = !entries.is_empty();
        let github = scripted(move |operation, _| match operation {
            "PullRequestStack" => {
                let mut node = pr_json(target, &head, &base);
                node["stack"] = if entries.is_empty() {
                    Value::Null
                } else {
                    json!({"id": "S_1", "entries": page(entries.iter().map(entry_json).collect(), None)})
                };
                on_pr(node)
            }
            "OpenPullRequests" => json!({"repository": {"pullRequests":
                page(vec![pr_json(target, &head, &base)], None)}}),
            other => panic!("unexpected {other}"),
        })
        .await;
        let stack = github.stack(&repo(), PrNumber(target)).await.unwrap();
        let source = if native {
            StackSource::Native
        } else {
            StackSource::BaseBranch
        };
        assert_eq!(stack.source, source, "{name}");
        assert_eq!(numbers(&stack), expected, "{name}");
        assert_chained(&stack, &name);
    }
}

fn gitlab_stack_exchange(target: u64, stack: &[GitLabEntry]) -> Exchange {
    let entry = |e: &GitLabEntry| {
        json!({"iid": e.iid, "title": format!("Change {}", e.iid), "state": e.state,
               "draft": false, "sourceBranch": e.head, "targetBranch": e.base})
    };
    let me = stack
        .iter()
        .find(|e| e.iid == target.to_string())
        .expect("the target is in its stack");
    let mr = json!({
        "id": format!("gid://gitlab/MergeRequest/{target}"), "iid": target.to_string(),
        "title": format!("Change {target}"), "state": "opened",
        "sourceBranch": me.head, "targetBranch": me.base,
        "targetProject": {"fullPath": "Lovelace/review-sandbox"},
        "sourceProject": {"fullPath": "Lovelace/review-sandbox"},
        "createdAt": "2026-11-16T12:00:00Z", "updatedAt": "2026-11-16T12:05:00Z",
        "webUrl": format!("https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/{target}"),
        "stack": stack.iter().map(entry).collect::<Vec<_>>(),
    });
    Exchange {
        request: RecordedRequest {
            method: Method::Post,
            path: "/api/graphql".to_owned(),
            query: Vec::new(),
            operation: Some("Stack".to_owned()),
            variables: Some(json!({"path": "Lovelace/review-sandbox", "iid": target.to_string()})),
            body: None,
        },
        response: RecordedResponse {
            status: 200,
            headers: Default::default(),
            body: RecordedBody::Json(json!({"data": {"project": {"mergeRequest": mr}}})),
        },
    }
}

#[tokio::test]
async fn stack_fixtures_native_on_gitlab() {
    for case in fixtures().gitlab_native {
        let replay = Arc::new(ReplayTransport::new(
            "inline",
            Cassette {
                exchanges: vec![gitlab_stack_exchange(case.target, &case.stack)],
            },
        ));
        let options = HttpOptions {
            clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
            ..HttpOptions::for_kind(ForgeKind::GitLab)
        };
        let credentials = Arc::new(StaticCredential(Credential::bearer(Token::new("replay"))));
        let http = HttpClient::new(
            ForgeKind::GitLab,
            "gitlab.com",
            replay.clone(),
            credentials,
            options,
        );
        let gitlab = GitLab::connect(http).await.unwrap();
        let sandbox = RepoRef::new("gitlab.com", "Lovelace", "review-sandbox");
        let stack = gitlab.stack(&sandbox, PrNumber(case.target)).await.unwrap();
        assert_eq!(stack.source, StackSource::Native, "{}", case.name);
        assert_eq!(numbers(&stack), case.expected, "{}", case.name);
        assert_chained(&stack, &case.name);
        assert!(replay.unused().is_empty(), "{}", case.name);
    }
}
