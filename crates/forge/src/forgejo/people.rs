//! Forgejo reviewers, assignees and labels (REST, writes never resent).
//!
//! - Pickers: `GET /repos/{o}/{r}/assignees` (the accounts that can be
//!   assigned, which is also who can be asked to review; it has no search, so
//!   the prefix is matched here) and `GET /repos/{o}/{r}/labels`.
//! - Reviewers: `POST` and `DELETE .../pulls/{i}/requested_reviewers` with
//!   `reviewers` (logins) and `team_reviewers` (team names).
//! - Assignees: `PATCH .../issues/{i}` with the whole `assignees` list; there
//!   is no add or remove call.
//! - Labels: `POST .../issues/{i}/labels` with names, and `DELETE
//!   .../issues/{i}/labels/{id}`, whose id comes from the repository's list.
//!
//! Each write ends with a `pull_request` read for the answer.

use serde_json::{Value, json};

use super::{ApiLabel, ApiUser, Forgejo};
use crate::error::Result;
use crate::forge::Forge;
use crate::http::Method;
use crate::model::{Label, PrNumber, PullRequest, RepoRef, User};
use crate::people::{COLLABORATOR_LIMIT, matches_query, plan_labels, plan_users, team_slug};

/// Rows of the assignee list read before it is filtered: more than any
/// sandbox or small team has, and a stop for a server that ignores paging.
const ASSIGNEE_ROWS: usize = 500;

async fn api_labels(forgejo: &Forgejo, repo: &RepoRef) -> Result<Vec<ApiLabel>> {
    forgejo
        .api
        .paged(
            &["repos", &repo.owner, &repo.name, "labels"],
            &[],
            usize::MAX,
            |rows: Vec<ApiLabel>| rows,
        )
        .await
}

async fn fetch_labels(forgejo: &Forgejo, repo: &RepoRef) -> Result<Vec<Label>> {
    let rows = api_labels(forgejo, repo).await?;
    Ok(rows.into_iter().map(ApiLabel::into_label).collect())
}

pub(super) async fn collaborators(
    forgejo: &Forgejo,
    repo: &RepoRef,
    query: &str,
) -> Result<Vec<User>> {
    forgejo
        .picker
        .collaborators(forgejo.api.http.now(), repo, query, || async {
            let rows: Vec<ApiUser> = forgejo
                .api
                .paged(
                    &["repos", &repo.owner, &repo.name, "assignees"],
                    &[],
                    ASSIGNEE_ROWS,
                    |rows: Vec<ApiUser>| rows,
                )
                .await?;
            let mut users: Vec<User> = Vec::new();
            for user in rows.into_iter().map(ApiUser::into_user) {
                if matches_query(&user, query) && !users.iter().any(|seen| seen.login == user.login)
                {
                    users.push(user);
                }
            }
            users.truncate(COLLABORATOR_LIMIT);
            Ok(users)
        })
        .await
}

pub(super) async fn labels(forgejo: &Forgejo, repo: &RepoRef) -> Result<Vec<Label>> {
    forgejo
        .picker
        .labels(forgejo.api.http.now(), repo, || fetch_labels(forgejo, repo))
        .await
}

fn reviewer_body(users: &[User], teams: &[User]) -> Value {
    json!({
        "reviewers": users.iter().map(|user| user.login.as_str()).collect::<Vec<_>>(),
        "team_reviewers": teams.iter().map(|team| team_slug(&team.login)).collect::<Vec<_>>(),
    })
}

async fn change_reviewers(
    forgejo: &Forgejo,
    method: Method,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    if !users.is_empty() || !teams.is_empty() {
        let n = number.to_string();
        forgejo
            .api
            .write(
                method,
                &[
                    "repos",
                    &repo.owner,
                    &repo.name,
                    "pulls",
                    &n,
                    "requested_reviewers",
                ],
                Some(reviewer_body(users, teams)),
            )
            .await?;
    }
    Forge::pull_request(forgejo, repo, number).await
}

pub(super) async fn request_reviewers(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    change_reviewers(forgejo, Method::Post, repo, number, users, teams).await
}

pub(super) async fn remove_reviewers(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    change_reviewers(forgejo, Method::Delete, repo, number, users, teams).await
}

pub(super) async fn set_assignees(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
) -> Result<PullRequest> {
    let current = Forge::pull_request(forgejo, repo, number).await?;
    let (add, remove) = plan_users(&current.assignees, users);
    if add.is_empty() && remove.is_empty() {
        return Ok(current);
    }
    let logins: Vec<&str> = users.iter().map(|user| user.login.as_str()).collect();
    let n = number.to_string();
    forgejo
        .api
        .write(
            Method::Patch,
            &["repos", &repo.owner, &repo.name, "issues", &n],
            Some(json!({"assignees": logins})),
        )
        .await?;
    Forge::pull_request(forgejo, repo, number).await
}

pub(super) async fn set_labels(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    labels: &[String],
) -> Result<PullRequest> {
    let wanted = forgejo
        .picker
        .resolve_labels(forgejo.api.http.now(), repo, labels, || {
            fetch_labels(forgejo, repo)
        })
        .await?;
    let current = Forge::pull_request(forgejo, repo, number).await?;
    let (add, remove) = plan_labels(&current.labels, &wanted);
    if add.is_empty() && remove.is_empty() {
        return Ok(current);
    }
    let n = number.to_string();
    if !add.is_empty() {
        forgejo
            .api
            .write(
                Method::Post,
                &["repos", &repo.owner, &repo.name, "issues", &n, "labels"],
                Some(json!({"labels": add})),
            )
            .await?;
    }
    if !remove.is_empty() {
        let ids = api_labels(forgejo, repo).await?;
        for name in &remove {
            let Some(label) = ids.iter().find(|label| label.name == *name) else {
                continue;
            };
            let id = label.id.to_string();
            forgejo
                .api
                .write(
                    Method::Delete,
                    &[
                        "repos",
                        &repo.owner,
                        &repo.name,
                        "issues",
                        &n,
                        "labels",
                        &id,
                    ],
                    None,
                )
                .await?;
        }
    }
    Forge::pull_request(forgejo, repo, number).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reviewer_bodies_name_logins_and_team_names() {
        let mut team = User::new("acme/core");
        team.kind = crate::model::AccountKind::Team;
        assert_eq!(
            reviewer_body(&[User::new("ann")], &[team]),
            json!({"reviewers": ["ann"], "team_reviewers": ["core"]})
        );
    }
}
