//! GitLab merge request lifecycle. REST for everything the merge needs
//! (`detailed_merge_status`, the merge call, `state_event`, the project's
//! merge settings) and one GraphQL mutation, `mergeRequestSetDraft`, so the
//! draft state is never edited through the title. Writes are never resent.
//!
//! GitLab merges in the background: the answer to the merge call can still
//! read open, so [`merge`] looks again a few times at the settle interval.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::{GitLab, GqlMr, MR_FIELDS, project_segment, to_pull_request};
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::Method;
use crate::model::{
    ForgeKind, MergeBlocker, MergeMethod, MergeOptions, MergeSettings, Mergeability, PrNumber,
    PullRequest, RepoRef,
};

/// How many times a merge is looked at before the answer is returned as it
/// reads: at the default one-second interval, longer than a merge takes.
const MERGE_LOOKS: usize = 15;

const SET_DRAFT: &str =
    "mutation MergeRequestSetDraft($path: ID!, $iid: String!, $draft: Boolean!) {
  mergeRequestSetDraft(input: {projectPath: $path, iid: $iid, draft: $draft}) {
    errors
    mergeRequest { ...MrFields }
  }
}";

/// The project settings a merge depends on.
#[derive(Deserialize)]
struct RestProject {
    #[serde(default)]
    merge_method: String,
    #[serde(default)]
    squash_option: String,
}

/// What `GET .../merge_requests/:iid` says about mergeability.
#[derive(Deserialize)]
struct RestMergeStatus {
    #[serde(default)]
    state: String,
    #[serde(default)]
    detailed_merge_status: String,
    #[serde(default)]
    has_conflicts: bool,
    #[serde(default)]
    merge_when_pipeline_succeeds: bool,
    #[serde(default)]
    auto_merge_enabled: Option<bool>,
}

#[derive(Deserialize)]
struct SetDraftData {
    #[serde(rename = "mergeRequestSetDraft")]
    payload: Option<SetDraftPayload>,
}

#[derive(Deserialize)]
struct SetDraftPayload {
    #[serde(default)]
    errors: Vec<String>,
    #[serde(rename = "mergeRequest")]
    merge_request: Option<GqlMr>,
}

fn mr_path(repo: &RepoRef, number: PrNumber) -> String {
    format!("projects/{}/merge_requests/{number}", project_segment(repo))
}

impl GitLab {
    async fn rest_get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.http.get_json(self.rest_url(path, &[])?).await
    }
}

pub(super) async fn merge_methods(gitlab: &GitLab, repo: &RepoRef) -> Result<MergeSettings> {
    let project: RestProject = gitlab
        .rest_get(&format!("projects/{}", project_segment(repo)))
        .await?;
    Ok(settings(&project.merge_method, &project.squash_option))
}

/// `merge` and `rebase_merge` projects make a merge commit, `ff` projects
/// fast-forward (a rebase). The squash option adds a squash, or with
/// `always` is the only way.
fn settings(merge_method: &str, squash_option: &str) -> MergeSettings {
    let base = if merge_method == "ff" {
        MergeMethod::Rebase
    } else {
        MergeMethod::Merge
    };
    match squash_option {
        "always" => MergeSettings::new([MergeMethod::Squash], MergeMethod::Squash),
        "default_on" => MergeSettings::new([base, MergeMethod::Squash], MergeMethod::Squash),
        "never" => MergeSettings::new([base], base),
        _ => MergeSettings::new([base, MergeMethod::Squash], base),
    }
}

pub(super) async fn mergeability(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Mergeability> {
    let status: RestMergeStatus = gitlab.rest_get(&mr_path(repo, number)).await?;
    Ok(status.into_mergeability())
}

impl RestMergeStatus {
    fn into_mergeability(self) -> Mergeability {
        use MergeBlocker::{
            ApprovalsRequired, BranchProtection, ChangesRequested, ChecksFailing, ChecksPending,
            Computing, Conflicts, Draft, NotOpen, OutOfDate, UnresolvedThreads,
        };
        let auto_merge = self.merge_when_pipeline_succeeds || self.auto_merge_enabled == Some(true);
        let mut reasons = Vec::new();
        if matches!(self.state.as_str(), "merged" | "closed") {
            reasons.push(NotOpen);
        } else {
            match self.detailed_merge_status.as_str() {
                "mergeable" | "" => {}
                "checking" | "unchecked" | "approvals_syncing" => reasons.push(Computing),
                "ci_must_pass" => reasons.push(ChecksFailing),
                "ci_still_running" => reasons.push(ChecksPending),
                "discussions_not_resolved" => reasons.push(UnresolvedThreads),
                "draft_status" => reasons.push(Draft),
                "not_approved" => reasons.push(ApprovalsRequired),
                "requested_changes" => reasons.push(ChangesRequested),
                "need_rebase" => reasons.push(OutOfDate),
                "broken_status" => reasons.push(Conflicts),
                "not_open" => reasons.push(NotOpen),
                _ => reasons.push(BranchProtection),
            }
            if self.has_conflicts && !reasons.contains(&Conflicts) {
                reasons.push(Conflicts);
            }
        }
        let computing = reasons == [Computing];
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

/// The body of the merge call. `delete` is already limited to a head branch
/// in the merge request's own project. GitLab keeps the project's own
/// "remove source branch" default unless it is sent, so it is always sent.
fn merge_body(options: &MergeOptions, delete: bool) -> Value {
    let mut body = Map::new();
    body.insert("sha".to_owned(), json!(options.sha));
    body.insert(
        "squash".to_owned(),
        json!(options.method == MergeMethod::Squash),
    );
    body.insert("should_remove_source_branch".to_owned(), json!(delete));
    if let Some(text) = options.commit_text() {
        if options.method == MergeMethod::Squash {
            body.insert("squash_commit_message".to_owned(), json!(text));
        }
        body.insert("merge_commit_message".to_owned(), json!(text));
    }
    Value::Object(body)
}

/// Refuses a method the project does not make: a merge call has no method
/// of its own, so GitLab would silently use the project's.
async fn check_method(gitlab: &GitLab, repo: &RepoRef, method: MergeMethod) -> Result<()> {
    let settings = merge_methods(gitlab, repo).await?;
    if settings.allowed.contains(&method) {
        return Ok(());
    }
    Err(Error::Unsupported {
        kind: ForgeKind::GitLab,
        feature: "this merge method in this project",
    })
}

pub(super) async fn merge(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    options: &MergeOptions,
) -> Result<PullRequest> {
    check_method(gitlab, repo, options.method).await?;
    let before = Forge::pull_request(gitlab, repo, number).await?;
    let delete = options.delete_branch && before.head_in_base_repo();
    gitlab
        .write_empty(
            Method::Put,
            &format!("{}/merge", mr_path(repo, number)),
            Some(merge_body(options, delete)),
        )
        .await?;
    let mut merged = Forge::pull_request(gitlab, repo, number).await?;
    for _ in 1..MERGE_LOOKS {
        if !merged.state.is_open() {
            break;
        }
        tokio::time::sleep(gitlab.settle).await;
        merged = Forge::pull_request(gitlab, repo, number).await?;
    }
    Ok(merged)
}

pub(super) async fn set_auto_merge(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    options: Option<&MergeOptions>,
) -> Result<PullRequest> {
    match options {
        Some(options) => {
            check_method(gitlab, repo, options.method).await?;
            let before = Forge::pull_request(gitlab, repo, number).await?;
            let delete = options.delete_branch && before.head_in_base_repo();
            let mut body = merge_body(options, delete);
            body["auto_merge"] = json!(true);
            gitlab
                .write_empty(
                    Method::Put,
                    &format!("{}/merge", mr_path(repo, number)),
                    Some(body),
                )
                .await?;
        }
        None => {
            gitlab
                .write_empty(
                    Method::Post,
                    &format!(
                        "{}/cancel_merge_when_pipeline_succeeds",
                        mr_path(repo, number)
                    ),
                    None,
                )
                .await?;
        }
    }
    Forge::pull_request(gitlab, repo, number).await
}

pub(super) async fn set_state(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    event: &str,
) -> Result<PullRequest> {
    gitlab
        .write_empty(
            Method::Put,
            &mr_path(repo, number),
            Some(json!({"state_event": event})),
        )
        .await?;
    Forge::pull_request(gitlab, repo, number).await
}

pub(super) async fn set_draft(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    draft: bool,
) -> Result<PullRequest> {
    let operation = "MergeRequestSetDraft";
    let query = format!("{SET_DRAFT}\n{MR_FIELDS}");
    let mut variables = GitLab::pull_request_variables(repo, number);
    variables["draft"] = json!(draft);
    let data: SetDraftData = gitlab
        .http
        .graphql_mutation(gitlab.graphql.clone(), operation, &query, variables)
        .await?;
    let payload = data.payload.ok_or_else(|| Error::Decode {
        request: operation.to_owned(),
        reason: "no payload".to_owned(),
    })?;
    if !payload.errors.is_empty() {
        return Err(Error::GraphQl {
            operation: operation.to_owned(),
            message: payload.errors.join("; "),
        });
    }
    let mr = payload.merge_request.ok_or_else(|| Error::Decode {
        request: operation.to_owned(),
        reason: "no merge request in the payload".to_owned(),
    })?;
    to_pull_request(gitlab.http.host(), &mr)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sha;

    fn status(state: &str, detailed: &str, conflicts: bool) -> RestMergeStatus {
        RestMergeStatus {
            state: state.to_owned(),
            detailed_merge_status: detailed.to_owned(),
            has_conflicts: conflicts,
            merge_when_pipeline_succeeds: false,
            auto_merge_enabled: None,
        }
    }

    #[test]
    fn detailed_merge_status_maps_to_blockers() {
        use MergeBlocker::{
            ApprovalsRequired, BranchProtection, ChecksFailing, ChecksPending, Computing,
            Conflicts, Draft, NotOpen, UnresolvedThreads,
        };
        let cases = [
            ("opened", "mergeable", false, Some(true), vec![]),
            ("opened", "checking", false, None, vec![Computing]),
            ("opened", "unchecked", false, None, vec![Computing]),
            ("opened", "draft_status", false, Some(false), vec![Draft]),
            (
                "opened",
                "ci_must_pass",
                false,
                Some(false),
                vec![ChecksFailing],
            ),
            (
                "opened",
                "ci_still_running",
                false,
                Some(false),
                vec![ChecksPending],
            ),
            (
                "opened",
                "not_approved",
                false,
                Some(false),
                vec![ApprovalsRequired],
            ),
            (
                "opened",
                "discussions_not_resolved",
                false,
                Some(false),
                vec![UnresolvedThreads],
            ),
            (
                "opened",
                "broken_status",
                true,
                Some(false),
                vec![Conflicts],
            ),
            (
                "opened",
                "draft_status",
                true,
                Some(false),
                vec![Draft, Conflicts],
            ),
            (
                "opened",
                "external_status_checks",
                false,
                Some(false),
                vec![BranchProtection],
            ),
            ("merged", "not_open", false, Some(false), vec![NotOpen]),
        ];
        for (state, detailed, conflicts, mergeable, reasons) in cases {
            let result = status(state, detailed, conflicts).into_mergeability();
            assert_eq!(result.mergeable, mergeable, "{state} {detailed}");
            assert_eq!(result.reasons, reasons, "{state} {detailed}");
        }
        let mut auto = status("opened", "ci_still_running", false);
        auto.auto_merge_enabled = Some(true);
        assert!(auto.into_mergeability().auto_merge);
    }

    #[test]
    fn project_settings_make_the_allowed_methods() {
        use MergeMethod::{Merge, Rebase, Squash};
        let cases = [
            ("merge", "default_off", vec![Merge, Squash], Merge),
            ("rebase_merge", "never", vec![Merge], Merge),
            ("ff", "default_on", vec![Squash, Rebase], Squash),
            ("ff", "never", vec![Rebase], Rebase),
            ("merge", "always", vec![Squash], Squash),
        ];
        for (method, squash, allowed, default) in cases {
            let settings = settings(method, squash);
            assert_eq!(settings.allowed, allowed, "{method} {squash}");
            assert_eq!(settings.default, default, "{method} {squash}");
        }
    }

    #[test]
    fn the_merge_body_always_says_whether_to_delete_the_branch() {
        let mut options = MergeOptions::new(MergeMethod::Squash, Sha::new("abc"));
        assert_eq!(
            merge_body(&options, false),
            json!({"sha": "abc", "squash": true, "should_remove_source_branch": false})
        );
        options.commit_title = Some("Title".to_owned());
        options.commit_message = Some("Body".to_owned());
        assert_eq!(
            merge_body(&options, true),
            json!({"sha": "abc", "squash": true, "should_remove_source_branch": true,
                   "squash_commit_message": "Title\n\nBody", "merge_commit_message": "Title\n\nBody"})
        );
        options.method = MergeMethod::Merge;
        assert_eq!(
            merge_body(&options, false),
            json!({"sha": "abc", "squash": false, "should_remove_source_branch": false,
                   "merge_commit_message": "Title\n\nBody"})
        );
    }
}
