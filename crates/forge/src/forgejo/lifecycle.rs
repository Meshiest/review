//! Forgejo pull request lifecycle (REST, `RetryMode::Never` for the writes):
//! merge, close, reopen, and draft state through the title's `WIP:` prefix,
//! which is all the API has. Auto-merge (`merge_when_checks_succeed`) is not
//! used: [`Capabilities::auto_merge`](crate::Capabilities::auto_merge) is
//! false.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Api, Forgejo, null_default};
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::Method;
use crate::model::{
    CiState, ForgeKind, MergeBlocker, MergeMethod, MergeOptions, MergeSettings, Mergeability,
    PrNumber, PullRequest, RepoRef,
};

/// The title prefixes Forgejo reads as "work in progress", compared without
/// regard to case.
const WIP_PREFIXES: [&str; 2] = ["WIP:", "[WIP]"];

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestPull {
    #[serde(deserialize_with = "null_default")]
    state: String,
    #[serde(deserialize_with = "null_default")]
    title: String,
    draft: bool,
    merged: bool,
    mergeable: bool,
    head: RestHead,
    base: RestBase,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestHead {
    #[serde(deserialize_with = "null_default")]
    sha: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestBase {
    #[serde(rename = "ref", deserialize_with = "null_default")]
    name: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestReview {
    #[serde(deserialize_with = "null_default")]
    state: String,
    dismissed: bool,
    stale: bool,
    official: bool,
}

/// What a base branch's protection rule asks of a merge.
#[derive(Deserialize, Default)]
#[serde(default)]
struct RestProtection {
    enable_status_check: bool,
    required_approvals: u32,
    block_on_rejected_reviews: bool,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RestRepo {
    allow_merge_commits: bool,
    allow_squash_merge: bool,
    allow_rebase: bool,
    #[serde(deserialize_with = "null_default")]
    default_merge_style: String,
}

fn pull_segments<'a>(repo: &'a RepoRef, number: &'a str, tail: &'a [&'a str]) -> Vec<&'a str> {
    let mut segments = vec!["repos", &repo.owner, &repo.name, "pulls", number];
    segments.extend(tail);
    segments
}

pub(super) async fn merge_methods(forgejo: &Forgejo, repo: &RepoRef) -> Result<MergeSettings> {
    let flags: RestRepo = forgejo
        .api
        .get(&["repos", &repo.owner, &repo.name], &[])
        .await?;
    Ok(settings(&flags))
}

fn settings(repo: &RestRepo) -> MergeSettings {
    let allowed = [
        (repo.allow_merge_commits, MergeMethod::Merge),
        (repo.allow_squash_merge, MergeMethod::Squash),
        (repo.allow_rebase, MergeMethod::Rebase),
    ]
    .into_iter()
    .filter_map(|(allowed, method)| allowed.then_some(method));
    let default = match repo.default_merge_style.as_str() {
        "squash" => MergeMethod::Squash,
        "rebase" | "fast-forward-only" => MergeMethod::Rebase,
        _ => MergeMethod::Merge,
    };
    MergeSettings::new(allowed, default)
}

pub(super) async fn mergeability(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Mergeability> {
    let api = &forgejo.api;
    let n = number.to_string();
    let pull_path = pull_segments(repo, &n, &[]);
    let reviews_path = pull_segments(repo, &n, &["reviews"]);
    let (pull, reviews) = tokio::try_join!(
        api.get::<RestPull>(&pull_path, &[]),
        api.paged::<RestReview, Vec<RestReview>>(&reviews_path, &[], usize::MAX, |rows| rows),
    )?;
    let protection = protection(api, repo, &pull.base.name).await?;
    let ci = match &protection {
        Some(rule) if rule.enable_status_check => {
            Some(api.combined_state(repo, &pull.head.sha).await?)
        }
        _ => None,
    };
    Ok(mergeability_of(&pull, &reviews, protection.as_ref(), ci))
}

/// The base branch's protection rule. `None` when it has none, or when the
/// viewer may not read it (only people who can administer the repository
/// can); those cases read as "no rule".
async fn protection(api: &Api, repo: &RepoRef, base: &str) -> Result<Option<RestProtection>> {
    let segments = ["repos", &repo.owner, &repo.name, "branch_protections", base];
    match api.get::<RestProtection>(&segments, &[]).await {
        Ok(rule) => Ok(Some(rule)),
        Err(Error::NotFound { .. } | Error::Status { status: 403, .. }) => Ok(None),
        Err(err) => Err(err),
    }
}

fn mergeability_of(
    pull: &RestPull,
    reviews: &[RestReview],
    protection: Option<&RestProtection>,
    ci: Option<CiState>,
) -> Mergeability {
    let mut reasons = Vec::new();
    if pull.merged || pull.state == "closed" {
        reasons.push(MergeBlocker::NotOpen);
    } else {
        if pull.draft {
            reasons.push(MergeBlocker::Draft);
        }
        // The server reports a draft as not mergeable, so a conflict under a
        // draft cannot be told apart and is left out.
        if !pull.draft && !pull.mergeable {
            reasons.push(MergeBlocker::Conflicts);
        }
        if let Some(rule) = protection {
            let counted = reviews
                .iter()
                .filter(|review| review.official && !review.dismissed && !review.stale);
            let approvals = counted.clone().filter(|r| r.state == "APPROVED").count();
            let rejected = counted.filter(|r| r.state == "REQUEST_CHANGES").count() > 0;
            if usize::try_from(rule.required_approvals).is_ok_and(|need| approvals < need) {
                reasons.push(MergeBlocker::ApprovalsRequired);
            }
            if rule.block_on_rejected_reviews && rejected {
                reasons.push(MergeBlocker::ChangesRequested);
            }
            if rule.enable_status_check {
                match ci {
                    Some(CiState::Fail) => reasons.push(MergeBlocker::ChecksFailing),
                    Some(CiState::Running | CiState::Queued | CiState::None) | None => {
                        reasons.push(MergeBlocker::ChecksPending);
                    }
                    Some(CiState::Pass | CiState::Skipped | CiState::Canceled) => {}
                }
            }
        }
    }
    Mergeability {
        mergeable: Some(reasons.is_empty()),
        reasons,
        auto_merge: false,
    }
}

fn do_name(method: MergeMethod) -> &'static str {
    match method {
        MergeMethod::Merge => "merge",
        MergeMethod::Squash => "squash",
        MergeMethod::Rebase => "rebase",
    }
}

/// The body of the merge call. `delete` is already limited to a head branch
/// in the pull request's own repository; it is always sent, so the
/// repository's "delete branch after merge" default never applies unasked.
fn merge_body(options: &MergeOptions, delete: bool) -> Value {
    let mut body = json!({
        "Do": do_name(options.method),
        "head_commit_id": options.sha,
        "delete_branch_after_merge": delete,
    });
    if let Some(title) = &options.commit_title {
        body["MergeTitleField"] = json!(title);
    }
    if let Some(message) = &options.commit_message {
        body["MergeMessageField"] = json!(message);
    }
    body
}

pub(super) async fn merge(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    options: &MergeOptions,
) -> Result<PullRequest> {
    let before = Forge::pull_request(forgejo, repo, number).await?;
    let delete = options.delete_branch && before.head_in_base_repo();
    let n = number.to_string();
    forgejo
        .api
        .write(
            Method::Post,
            &pull_segments(repo, &n, &["merge"]),
            Some(merge_body(options, delete)),
        )
        .await?;
    Forge::pull_request(forgejo, repo, number).await
}

pub(super) async fn set_state(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    state: &str,
) -> Result<PullRequest> {
    let n = number.to_string();
    forgejo
        .api
        .write(
            Method::Patch,
            &pull_segments(repo, &n, &[]),
            Some(json!({"state": state})),
        )
        .await?;
    Forge::pull_request(forgejo, repo, number).await
}

pub(super) async fn set_draft(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    draft: bool,
) -> Result<PullRequest> {
    let n = number.to_string();
    let pull: RestPull = forgejo.api.get(&pull_segments(repo, &n, &[]), &[]).await?;
    let Some(title) = retitled(&pull.title, pull.draft, draft) else {
        return Forge::pull_request(forgejo, repo, number).await;
    };
    let title = title.ok_or(Error::Unsupported {
        kind: ForgeKind::Forgejo,
        feature: "leaving draft with a custom work-in-progress prefix",
    })?;
    forgejo
        .api
        .write(
            Method::Patch,
            &pull_segments(repo, &n, &[]),
            Some(json!({"title": title})),
        )
        .await?;
    Forge::pull_request(forgejo, repo, number).await
}

/// The title that makes the pull request `want` draft: outer `None` when it
/// already is, inner `None` when it is a draft by a prefix this backend does
/// not know how to remove.
fn retitled(title: &str, is_draft: bool, want: bool) -> Option<Option<String>> {
    match (is_draft, want) {
        (true, true) | (false, false) => None,
        (false, true) => Some(Some(format!("WIP: {title}"))),
        (true, false) => {
            let stripped = strip_wip(title);
            Some((stripped != title).then_some(stripped))
        }
    }
}

/// `title` without its leading work-in-progress prefixes.
fn strip_wip(title: &str) -> String {
    let mut rest = title.trim();
    'again: loop {
        for prefix in WIP_PREFIXES {
            if let Some(head) = rest.get(..prefix.len())
                && head.eq_ignore_ascii_case(prefix)
            {
                rest = rest[prefix.len()..].trim_start();
                continue 'again;
            }
        }
        return rest.to_owned();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Sha;

    fn pull(overrides: Value) -> RestPull {
        let mut base = json!({"state": "open", "draft": false, "merged": false,
                              "mergeable": true, "head": {"sha": "abc"}, "base": {"ref": "main"}});
        base.as_object_mut()
            .unwrap()
            .extend(overrides.as_object().unwrap().clone());
        serde_json::from_value(base).unwrap()
    }

    fn review(state: &str) -> RestReview {
        RestReview {
            state: state.to_owned(),
            official: true,
            ..RestReview::default()
        }
    }

    #[test]
    fn a_draft_is_named_and_a_conflict_only_without_one() {
        use MergeBlocker::{Conflicts, Draft, NotOpen};
        let clean = mergeability_of(&pull(json!({})), &[], None, None);
        assert_eq!((clean.mergeable, clean.reasons), (Some(true), vec![]));
        let draft = mergeability_of(
            &pull(json!({"draft": true, "mergeable": false})),
            &[],
            None,
            None,
        );
        assert_eq!((draft.mergeable, draft.reasons), (Some(false), vec![Draft]));
        let conflicting = mergeability_of(&pull(json!({"mergeable": false})), &[], None, None);
        assert_eq!(
            (conflicting.mergeable, conflicting.reasons),
            (Some(false), vec![Conflicts])
        );
        let merged = mergeability_of(
            &pull(json!({"merged": true, "state": "closed"})),
            &[],
            None,
            None,
        );
        assert_eq!(merged.reasons, vec![NotOpen]);
    }

    #[test]
    fn a_protection_rule_names_approvals_reviews_and_checks() {
        use MergeBlocker::{ApprovalsRequired, ChangesRequested, ChecksFailing, ChecksPending};
        let rule = RestProtection {
            enable_status_check: true,
            required_approvals: 1,
            block_on_rejected_reviews: true,
        };
        let open = pull(json!({}));
        let result = mergeability_of(
            &open,
            &[review("REQUEST_CHANGES")],
            Some(&rule),
            Some(CiState::Fail),
        );
        assert_eq!(
            result.reasons,
            vec![ApprovalsRequired, ChangesRequested, ChecksFailing]
        );
        let result = mergeability_of(
            &open,
            &[review("APPROVED")],
            Some(&rule),
            Some(CiState::Running),
        );
        assert_eq!(result.reasons, vec![ChecksPending]);
        let result = mergeability_of(
            &open,
            &[review("APPROVED")],
            Some(&rule),
            Some(CiState::Pass),
        );
        assert_eq!(result.mergeable, Some(true));
        let stale = RestReview {
            stale: true,
            ..review("APPROVED")
        };
        let result = mergeability_of(&open, &[stale], Some(&rule), Some(CiState::Pass));
        assert_eq!(result.reasons, vec![ApprovalsRequired]);
    }

    #[test]
    fn repository_flags_make_the_allowed_methods() {
        let repo = RestRepo {
            allow_merge_commits: true,
            allow_squash_merge: true,
            allow_rebase: false,
            default_merge_style: "squash".to_owned(),
        };
        let settings = settings(&repo);
        assert_eq!(settings.allowed, [MergeMethod::Merge, MergeMethod::Squash]);
        assert_eq!(settings.default, MergeMethod::Squash);
        let only_rebase = RestRepo {
            allow_rebase: true,
            default_merge_style: "merge".to_owned(),
            ..RestRepo::default()
        };
        let settings = super::settings(&only_rebase);
        assert_eq!(
            (settings.allowed, settings.default),
            (vec![MergeMethod::Rebase], MergeMethod::Rebase)
        );
    }

    #[test]
    fn the_work_in_progress_prefix_is_added_and_removed() {
        assert_eq!(
            retitled("Fix", false, true),
            Some(Some("WIP: Fix".to_owned()))
        );
        assert_eq!(
            retitled("WIP: Fix", true, false),
            Some(Some("Fix".to_owned()))
        );
        assert_eq!(
            retitled("[wip] wip: Fix", true, false),
            Some(Some("Fix".to_owned()))
        );
        assert_eq!(retitled("Fix", true, true), None);
        assert_eq!(retitled("Fix", false, false), None);
        assert_eq!(retitled("Fix", true, false), Some(None));
        assert_eq!(strip_wip("WIPER blades"), "WIPER blades");
    }

    #[test]
    fn the_merge_body_always_says_whether_to_delete_the_branch() {
        let mut options = MergeOptions::new(MergeMethod::Squash, Sha::new("abc"));
        assert_eq!(
            merge_body(&options, false),
            json!({"Do": "squash", "head_commit_id": "abc", "delete_branch_after_merge": false})
        );
        options.commit_title = Some("Title".to_owned());
        options.commit_message = Some("Body".to_owned());
        assert_eq!(
            merge_body(&options, true),
            json!({"Do": "squash", "head_commit_id": "abc", "delete_branch_after_merge": true,
                   "MergeTitleField": "Title", "MergeMessageField": "Body"})
        );
    }
}
