//! GitHub pull request lifecycle: merge, auto-merge, close, reopen, draft,
//! and the reads that say what a merge may do. Every write is a named GraphQL
//! mutation sent without retries; the branch delete after a merge is the one
//! REST call.
//!
//! Operations: `MergeMethods`, `PullRequestMergeability`, `MergePullRequest`,
//! `EnablePullRequestAutoMerge`, `DisablePullRequestAutoMerge`,
//! `ClosePullRequest`, `ReopenPullRequest`, `ConvertPullRequestToDraft`,
//! `MarkPullRequestReadyForReview`.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{GitHub, PullRequestNode, RepoEnvelope, variables};
use crate::error::{Error, Result};
use crate::http::{HttpRequest, Method, RetryMode};
use crate::model::{
    MergeBlocker, MergeMethod, MergeOptions, MergeSettings, Mergeability, PrNumber, PullRequest,
    RepoRef,
};

const MERGE_METHODS: &str = "query MergeMethods($owner:String!,$name:String!){\
     repository(owner:$owner,name:$name){mergeCommitAllowed squashMergeAllowed \
     rebaseMergeAllowed viewerDefaultMergeMethod}}";

const MERGEABILITY: &str = on_pull_request!(
    "PullRequestMergeability",
    "state isDraft mergeable mergeStateStatus reviewDecision autoMergeRequest{enabledAt} \
     commits(last:1){nodes{commit{statusCheckRollup{state}}}}"
);

macro_rules! merge_mutation {
    ($operation:literal, $field:literal) => {
        concat!(
            "mutation ",
            $operation,
            "($pullRequestId:ID!,$mergeMethod:PullRequestMergeMethod,$commitHeadline:String,\
             $commitBody:String,$expectedHeadOid:GitObjectID){",
            $field,
            "(input:{pullRequestId:$pullRequestId,mergeMethod:$mergeMethod,\
             commitHeadline:$commitHeadline,commitBody:$commitBody,\
             expectedHeadOid:$expectedHeadOid}){pullRequest{",
            pull_request_fields!(),
            "}}}"
        )
    };
}

macro_rules! pull_request_mutation {
    ($operation:literal, $field:literal) => {
        concat!(
            "mutation ",
            $operation,
            "($pullRequestId:ID!){",
            $field,
            "(input:{pullRequestId:$pullRequestId}){pullRequest{",
            pull_request_fields!(),
            "}}}"
        )
    };
}

const MERGE: &str = merge_mutation!("MergePullRequest", "mergePullRequest");
const ENABLE_AUTO_MERGE: &str =
    merge_mutation!("EnablePullRequestAutoMerge", "enablePullRequestAutoMerge");
const DISABLE_AUTO_MERGE: &str =
    pull_request_mutation!("DisablePullRequestAutoMerge", "disablePullRequestAutoMerge");
const CLOSE: &str = pull_request_mutation!("ClosePullRequest", "closePullRequest");
const REOPEN: &str = pull_request_mutation!("ReopenPullRequest", "reopenPullRequest");
const TO_DRAFT: &str =
    pull_request_mutation!("ConvertPullRequestToDraft", "convertPullRequestToDraft");
const TO_READY: &str = pull_request_mutation!(
    "MarkPullRequestReadyForReview",
    "markPullRequestReadyForReview"
);

pub(super) async fn merge_methods(github: &GitHub, repo: &RepoRef) -> Result<MergeSettings> {
    let operation = "MergeMethods";
    let envelope: MethodsEnvelope = github
        .query(
            operation,
            MERGE_METHODS,
            json!({"owner": repo.owner, "name": repo.name}),
        )
        .await?;
    let flags = envelope.repository.ok_or_else(|| Error::NotFound {
        request: format!("repository {repo}"),
    })?;
    let allowed = [
        (flags.merge_commit_allowed, MergeMethod::Merge),
        (flags.squash_merge_allowed, MergeMethod::Squash),
        (flags.rebase_merge_allowed, MergeMethod::Rebase),
    ]
    .into_iter()
    .filter_map(|(allowed, method)| allowed.then_some(method));
    let default = flags
        .viewer_default_merge_method
        .map_or(MergeMethod::Merge, MergeMethod::from);
    Ok(MergeSettings::new(allowed, default))
}

pub(super) async fn mergeability(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Mergeability> {
    let envelope: RepoEnvelope<MergeabilityNode> = github
        .query(
            "PullRequestMergeability",
            MERGEABILITY,
            variables(repo, number),
        )
        .await?;
    Ok(envelope
        .into_pull_request(repo, number)?
        .into_mergeability())
}

pub(super) async fn merge(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    options: &MergeOptions,
) -> Result<PullRequest> {
    let merged = merge_call(github, repo, number, options, "MergePullRequest", MERGE).await?;
    if options.delete_branch {
        delete_head_branch(github, &merged).await;
    }
    Ok(merged)
}

pub(super) async fn set_auto_merge(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    options: Option<&MergeOptions>,
) -> Result<PullRequest> {
    match options {
        Some(options) => {
            merge_call(
                github,
                repo,
                number,
                options,
                "EnablePullRequestAutoMerge",
                ENABLE_AUTO_MERGE,
            )
            .await
        }
        None => {
            pull_request_call(
                github,
                repo,
                number,
                "DisablePullRequestAutoMerge",
                DISABLE_AUTO_MERGE,
            )
            .await
        }
    }
}

pub(super) async fn close(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<PullRequest> {
    pull_request_call(github, repo, number, "ClosePullRequest", CLOSE).await
}

pub(super) async fn reopen(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<PullRequest> {
    pull_request_call(github, repo, number, "ReopenPullRequest", REOPEN).await
}

pub(super) async fn set_draft(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    draft: bool,
) -> Result<PullRequest> {
    let (operation, query) = if draft {
        ("ConvertPullRequestToDraft", TO_DRAFT)
    } else {
        ("MarkPullRequestReadyForReview", TO_READY)
    };
    pull_request_call(github, repo, number, operation, query).await
}

async fn merge_call(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    options: &MergeOptions,
    operation: &str,
    query: &str,
) -> Result<PullRequest> {
    let id = github.pull_request_id(repo, number).await?;
    let payload: PullRequestPayload = github
        .mutate(
            operation,
            &field_name(operation),
            query,
            merge_variables(&id, options),
        )
        .await?;
    payload.into_pull_request(github, repo, operation)
}

async fn pull_request_call(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    operation: &str,
    query: &str,
) -> Result<PullRequest> {
    let id = github.pull_request_id(repo, number).await?;
    let payload: PullRequestPayload = github
        .mutate(
            operation,
            &field_name(operation),
            query,
            json!({"pullRequestId": id}),
        )
        .await?;
    payload.into_pull_request(github, repo, operation)
}

/// `MergePullRequest` -> `mergePullRequest`, the payload field of the
/// mutation.
fn field_name(operation: &str) -> String {
    let mut chars = operation.chars();
    chars
        .next()
        .map(|first| first.to_ascii_lowercase().to_string() + chars.as_str())
        .unwrap_or_default()
}

fn merge_variables(id: &crate::model::PullRequestId, options: &MergeOptions) -> Value {
    let mut vars = Map::new();
    vars.insert("pullRequestId".to_owned(), json!(id));
    vars.insert(
        "mergeMethod".to_owned(),
        json!(merge_method_name(options.method)),
    );
    vars.insert("expectedHeadOid".to_owned(), json!(options.sha));
    if let Some(title) = &options.commit_title {
        vars.insert("commitHeadline".to_owned(), json!(title));
    }
    if let Some(message) = &options.commit_message {
        vars.insert("commitBody".to_owned(), json!(message));
    }
    Value::Object(vars)
}

fn merge_method_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "MERGE",
        MergeMethod::Squash => "SQUASH",
        MergeMethod::Rebase => "REBASE",
    }
}

/// Deletes the merged pull request's head branch: only one that lives in the
/// pull request's own repository and is not its base. The merge has already
/// happened, so a refusal (a protected branch, a repository that deletes
/// merged branches by itself) is not reported.
async fn delete_head_branch(github: &GitHub, merged: &PullRequest) {
    if !merged.head_in_base_repo() || merged.head.name == merged.base.name {
        return;
    }
    let mut segments = vec!["git", "refs", "heads"];
    segments.extend(merged.head.name.split('/'));
    let Ok(url) = github.rest_url(&merged.repo, &segments) else {
        return;
    };
    let request = HttpRequest {
        method: Method::Delete,
        retry: RetryMode::Never,
        ..HttpRequest::get(url)
    };
    let _ = github.http.send(request).await;
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MethodsEnvelope {
    repository: Option<MethodFlags>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MethodFlags {
    #[serde(default)]
    merge_commit_allowed: bool,
    #[serde(default)]
    squash_merge_allowed: bool,
    #[serde(default)]
    rebase_merge_allowed: bool,
    viewer_default_merge_method: Option<MethodWire>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum MethodWire {
    Merge,
    Squash,
    Rebase,
}

impl From<MethodWire> for MergeMethod {
    fn from(wire: MethodWire) -> Self {
        match wire {
            MethodWire::Merge => Self::Merge,
            MethodWire::Squash => Self::Squash,
            MethodWire::Rebase => Self::Rebase,
        }
    }
}

#[derive(Debug, Deserialize)]
struct PullRequestPayload {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestNode>,
}

impl PullRequestPayload {
    fn into_pull_request(
        self,
        github: &GitHub,
        repo: &RepoRef,
        operation: &str,
    ) -> Result<PullRequest> {
        let node = self.pull_request.ok_or_else(|| Error::Decode {
            request: operation.to_owned(),
            reason: "no pull request in the payload".to_owned(),
        })?;
        github.pull_request_from(node, Some(repo))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum StateWire {
    Open,
    Closed,
    Merged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum MergeableWire {
    Mergeable,
    Conflicting,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum StatusWire {
    Behind,
    Blocked,
    Clean,
    Dirty,
    Draft,
    HasHooks,
    Unstable,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum DecisionWire {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum RollupWire {
    Failure,
    Error,
    Pending,
    Expected,
    #[serde(other)]
    Passing,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeabilityNode {
    state: StateWire,
    is_draft: bool,
    mergeable: MergeableWire,
    merge_state_status: StatusWire,
    review_decision: Option<DecisionWire>,
    auto_merge_request: Option<Value>,
    #[serde(default)]
    commits: Commits,
}

#[derive(Debug, Default, Deserialize)]
struct Commits {
    #[serde(default)]
    nodes: Vec<Option<CommitNodeWire>>,
}

#[derive(Debug, Deserialize)]
struct CommitNodeWire {
    commit: CommitRollup,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitRollup {
    status_check_rollup: Option<RollupState>,
}

#[derive(Debug, Deserialize)]
struct RollupState {
    state: Option<RollupWire>,
}

impl MergeabilityNode {
    fn rollup(&self) -> Option<RollupWire> {
        self.commits
            .nodes
            .iter()
            .flatten()
            .next_back()?
            .commit
            .status_check_rollup
            .as_ref()?
            .state
    }

    /// GitHub's `mergeable` only says there is no conflict. What blocks a
    /// merge beyond that is `mergeStateStatus`: `BLOCKED` is a rule of the
    /// base branch, explained here by the review decision and the checks
    /// where they point at it. Checks that are not required leave the status
    /// `UNSTABLE`, which still merges, so they are named only when the
    /// status is `BLOCKED`.
    fn into_mergeability(self) -> Mergeability {
        let auto_merge = self
            .auto_merge_request
            .as_ref()
            .is_some_and(|value| !value.is_null());
        let status = self.merge_state_status;
        let mut reasons = Vec::new();
        if self.state != StateWire::Open {
            reasons.push(MergeBlocker::NotOpen);
        } else {
            if self.is_draft || status == StatusWire::Draft {
                reasons.push(MergeBlocker::Draft);
            }
            if self.mergeable == MergeableWire::Conflicting || status == StatusWire::Dirty {
                reasons.push(MergeBlocker::Conflicts);
            }
            if status == StatusWire::Behind {
                reasons.push(MergeBlocker::OutOfDate);
            }
            if status == StatusWire::Blocked {
                let named = reasons.len();
                match self.review_decision {
                    Some(DecisionWire::ChangesRequested) => {
                        reasons.push(MergeBlocker::ChangesRequested);
                    }
                    Some(DecisionWire::ReviewRequired) => {
                        reasons.push(MergeBlocker::ApprovalsRequired);
                    }
                    Some(DecisionWire::Approved) | None => {}
                }
                match self.rollup() {
                    Some(RollupWire::Failure | RollupWire::Error) => {
                        reasons.push(MergeBlocker::ChecksFailing);
                    }
                    Some(RollupWire::Pending | RollupWire::Expected) => {
                        reasons.push(MergeBlocker::ChecksPending);
                    }
                    Some(RollupWire::Passing) | None => {}
                }
                if reasons.len() == named && !reasons.contains(&MergeBlocker::Draft) {
                    reasons.push(MergeBlocker::BranchProtection);
                }
            }
        }
        let computing = status == StatusWire::Unknown && reasons.is_empty();
        if computing {
            reasons.push(MergeBlocker::Computing);
        }
        Mergeability {
            mergeable: if computing {
                None
            } else {
                Some(reasons.is_empty())
            },
            reasons,
            auto_merge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(overrides: Value) -> MergeabilityNode {
        let mut base = json!({
            "state": "OPEN", "isDraft": false, "mergeable": "MERGEABLE",
            "mergeStateStatus": "CLEAN", "reviewDecision": null,
            "autoMergeRequest": null,
            "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "SUCCESS"}}}]},
        });
        base.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn mergeability_follows_the_state_status() {
        use MergeBlocker::*;
        let cases = [
            (json!({}), Some(true), vec![]),
            (json!({"mergeStateStatus": "UNSTABLE"}), Some(true), vec![]),
            (json!({"mergeStateStatus": "HAS_HOOKS"}), Some(true), vec![]),
            (
                json!({"mergeStateStatus": "UNKNOWN", "mergeable": "UNKNOWN"}),
                None,
                vec![Computing],
            ),
            (
                json!({"mergeStateStatus": "DIRTY", "mergeable": "CONFLICTING"}),
                Some(false),
                vec![Conflicts],
            ),
            (
                json!({"mergeStateStatus": "DRAFT", "isDraft": true}),
                Some(false),
                vec![Draft],
            ),
            (
                json!({"mergeStateStatus": "BLOCKED", "isDraft": true}),
                Some(false),
                vec![Draft],
            ),
            (
                json!({"mergeStateStatus": "BEHIND"}),
                Some(false),
                vec![OutOfDate],
            ),
            (
                json!({"mergeStateStatus": "BLOCKED"}),
                Some(false),
                vec![BranchProtection],
            ),
            (
                json!({"mergeStateStatus": "BLOCKED", "reviewDecision": "REVIEW_REQUIRED"}),
                Some(false),
                vec![ApprovalsRequired],
            ),
            (
                json!({"mergeStateStatus": "BLOCKED", "reviewDecision": "CHANGES_REQUESTED",
                       "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "FAILURE"}}}]}}),
                Some(false),
                vec![ChangesRequested, ChecksFailing],
            ),
            (
                json!({"mergeStateStatus": "BLOCKED",
                       "commits": {"nodes": [{"commit": {"statusCheckRollup": {"state": "PENDING"}}}]}}),
                Some(false),
                vec![ChecksPending],
            ),
            (json!({"state": "MERGED"}), Some(false), vec![NotOpen]),
            (
                json!({"autoMergeRequest": {"enabledAt": "2026-11-17T00:00:00Z"}}),
                Some(true),
                vec![],
            ),
        ];
        for (overrides, mergeable, reasons) in cases {
            let result = node(overrides.clone()).into_mergeability();
            assert_eq!(result.mergeable, mergeable, "{overrides}");
            assert_eq!(result.reasons, reasons, "{overrides}");
        }
        assert!(
            node(json!({"autoMergeRequest": {"enabledAt": "x"}}))
                .into_mergeability()
                .auto_merge
        );
    }

    #[test]
    fn mutations_are_named_after_their_operation() {
        for (operation, query) in [
            ("MergePullRequest", MERGE),
            ("EnablePullRequestAutoMerge", ENABLE_AUTO_MERGE),
            ("DisablePullRequestAutoMerge", DISABLE_AUTO_MERGE),
            ("ClosePullRequest", CLOSE),
            ("ReopenPullRequest", REOPEN),
            ("ConvertPullRequestToDraft", TO_DRAFT),
            ("MarkPullRequestReadyForReview", TO_READY),
        ] {
            assert!(
                query.starts_with(&format!("mutation {operation}(")),
                "{operation}: {query}"
            );
            assert!(
                query.contains(&format!("{}(input:", field_name(operation))),
                "{operation}: {query}"
            );
        }
        assert!(MERGEABILITY.starts_with("query PullRequestMergeability("));
        assert!(MERGE_METHODS.starts_with("query MergeMethods("));
    }

    #[test]
    fn merge_variables_carry_the_expected_head_and_only_the_set_text() {
        let id = crate::model::PullRequestId::new("PR_1");
        let mut options = MergeOptions::new(MergeMethod::Squash, crate::model::Sha::new("abc"));
        assert_eq!(
            merge_variables(&id, &options),
            json!({"pullRequestId": "PR_1", "mergeMethod": "SQUASH", "expectedHeadOid": "abc"})
        );
        options.commit_title = Some("Title".to_owned());
        options.commit_message = Some("Body".to_owned());
        assert_eq!(
            merge_variables(&id, &options),
            json!({"pullRequestId": "PR_1", "mergeMethod": "SQUASH", "expectedHeadOid": "abc",
                   "commitHeadline": "Title", "commitBody": "Body"})
        );
    }
}
