//! GitLab reviewers, assignees and labels. The pickers are REST
//! (`GET /projects/:id/users`, `GET /projects/:id/labels`); every write is
//! one `PUT .../merge_requests/:iid` with `reviewer_ids`, `assignee_ids`,
//! `add_labels` or `remove_labels`, followed by a `PullRequest` read for the
//! answer. `reviewer_ids` and `assignee_ids` replace the whole list, so the
//! current one is read first (from REST: GraphQL's reviewer list also holds
//! the people who only approved). Writes are never resent.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{GitLab, PAGE, RestUser, project_segment};
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::Method;
use crate::model::{ForgeKind, Label, PrNumber, PullRequest, RepoRef, User};
use crate::people::{COLLABORATOR_LIMIT, plan_labels, plan_users, without};

#[derive(Deserialize)]
struct RestLabel {
    name: String,
    color: Option<String>,
    description: Option<String>,
}

/// The people a merge request names, with the numeric ids `PUT` takes.
#[derive(Deserialize, Default)]
struct RestPeople {
    #[serde(default)]
    reviewers: Vec<RestUser>,
    #[serde(default)]
    assignees: Vec<RestUser>,
}

fn mr_path(repo: &RepoRef, number: PrNumber) -> String {
    format!("projects/{}/merge_requests/{number}", project_segment(repo))
}

impl GitLab {
    async fn people_of(&self, repo: &RepoRef, number: PrNumber) -> Result<RestPeople> {
        self.http
            .get_json(self.rest_url(&mr_path(repo, number), &[])?)
            .await
    }

    /// The numeric id of `user`: from its global id, or looked up by name
    /// for an account that came without one.
    async fn user_number(&self, user: &User) -> Result<u64> {
        if let Some(number) = user
            .id
            .as_ref()
            .and_then(|id| id.as_str().rsplit('/').next())
            .and_then(|digits| digits.parse().ok())
        {
            return Ok(number);
        }
        let url = self.rest_url("users", &[("username", &user.login)])?;
        let found: Vec<RestUser> = self.http.get_json(url).await?;
        found
            .into_iter()
            .find(|found| found.username.eq_ignore_ascii_case(&user.login))
            .map(|found| found.id)
            .ok_or_else(|| Error::NotFound {
                request: format!("user {}", user.login),
            })
    }

    async fn user_numbers(&self, users: &[User]) -> Result<Vec<u64>> {
        let mut numbers = Vec::with_capacity(users.len());
        for user in users {
            numbers.push(self.user_number(user).await?);
        }
        Ok(numbers)
    }

    async fn update_merge_request(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        body: Value,
    ) -> Result<PullRequest> {
        self.write_empty(Method::Put, &mr_path(repo, number), Some(body))
            .await?;
        Forge::pull_request(self, repo, number).await
    }
}

/// A list `PUT` takes to mean "no one": `[0]`, which GitLab reads as the
/// unassigned id.
fn id_list(ids: Vec<u64>) -> Vec<u64> {
    if ids.is_empty() { vec![0] } else { ids }
}

pub(super) async fn collaborators(
    gitlab: &GitLab,
    repo: &RepoRef,
    query: &str,
) -> Result<Vec<User>> {
    gitlab
        .picker
        .collaborators(gitlab.http.now(), repo, query, || async {
            let path = format!("projects/{}/users", project_segment(repo));
            let per_page = PAGE.to_string();
            let mut params = vec![("per_page", per_page.as_str())];
            let search = query.trim();
            if !search.is_empty() {
                params.push(("search", search));
            }
            let url = gitlab.rest_url(&path, &params)?;
            let rows: Vec<RestUser> = gitlab.rest_pages(url, COLLABORATOR_LIMIT).await?;
            let host = gitlab.http.host();
            Ok(rows.iter().map(|row| row.to_user(host)).collect())
        })
        .await
}

pub(super) async fn labels(gitlab: &GitLab, repo: &RepoRef) -> Result<Vec<Label>> {
    gitlab
        .picker
        .labels(gitlab.http.now(), repo, || fetch_labels(gitlab, repo))
        .await
}

async fn fetch_labels(gitlab: &GitLab, repo: &RepoRef) -> Result<Vec<Label>> {
    let path = format!("projects/{}/labels", project_segment(repo));
    let url = gitlab.rest_url(&path, &[("per_page", &PAGE.to_string())])?;
    let rows: Vec<RestLabel> = gitlab.rest_pages(url, usize::MAX).await?;
    Ok(rows
        .into_iter()
        .map(|row| Label {
            name: row.name,
            color: row
                .color
                .map(|color| color.trim_start_matches('#').to_owned())
                .filter(|color| !color.is_empty()),
            description: row.description.filter(|text| !text.is_empty()),
        })
        .collect())
}

fn refuse_teams(teams: &[User]) -> Result<()> {
    if teams.is_empty() {
        return Ok(());
    }
    Err(Error::Unsupported {
        kind: ForgeKind::GitLab,
        feature: "team reviewers",
    })
}

pub(super) async fn request_reviewers(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    refuse_teams(teams)?;
    let current = gitlab.people_of(repo, number).await?;
    let mut ids: Vec<u64> = current.reviewers.iter().map(|user| user.id).collect();
    for id in gitlab.user_numbers(users).await? {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    gitlab
        .update_merge_request(repo, number, json!({"reviewer_ids": id_list(ids)}))
        .await
}

pub(super) async fn remove_reviewers(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    refuse_teams(teams)?;
    let current = gitlab.people_of(repo, number).await?;
    let drop = gitlab.user_numbers(users).await?;
    let ids: Vec<u64> = current
        .reviewers
        .iter()
        .map(|user| user.id)
        .filter(|id| !drop.contains(id))
        .collect();
    if ids.len() == current.reviewers.len() {
        return Forge::pull_request(gitlab, repo, number).await;
    }
    gitlab
        .update_merge_request(repo, number, json!({"reviewer_ids": id_list(ids)}))
        .await
}

pub(super) async fn set_assignees(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
) -> Result<PullRequest> {
    let host = gitlab.http.host();
    let current: Vec<User> = gitlab
        .people_of(repo, number)
        .await?
        .assignees
        .iter()
        .map(|user| user.to_user(host))
        .collect();
    let (add, remove) = plan_users(&current, users);
    if add.is_empty() && remove.is_empty() {
        return Forge::pull_request(gitlab, repo, number).await;
    }
    let kept = without(&current, &remove);
    let ids = gitlab
        .user_numbers(&[kept.as_slice(), add.as_slice()].concat())
        .await?;
    gitlab
        .update_merge_request(repo, number, json!({"assignee_ids": id_list(ids)}))
        .await
}

pub(super) async fn set_labels(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    labels: &[String],
) -> Result<PullRequest> {
    // GitLab makes a label out of an unknown name, so names are checked first.
    let wanted = gitlab
        .picker
        .resolve_labels(gitlab.http.now(), repo, labels, || {
            fetch_labels(gitlab, repo)
        })
        .await?;
    let current = Forge::pull_request(gitlab, repo, number).await?;
    let (add, remove) = plan_labels(&current.labels, &wanted);
    if add.is_empty() && remove.is_empty() {
        return Ok(current);
    }
    let mut body = serde_json::Map::new();
    if !add.is_empty() {
        body.insert("add_labels".to_owned(), json!(add.join(",")));
    }
    if !remove.is_empty() {
        body.insert("remove_labels".to_owned(), json!(remove.join(",")));
    }
    gitlab
        .update_merge_request(repo, number, Value::Object(body))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_list_is_sent_as_the_unassigned_id() {
        assert_eq!(id_list(vec![]), [0]);
        assert_eq!(id_list(vec![4, 9]), [4, 9]);
    }

    #[test]
    fn groups_cannot_be_asked_to_review() {
        assert!(refuse_teams(&[]).is_ok());
        let err = refuse_teams(&[User::new("g")]).unwrap_err();
        assert!(matches!(err, Error::Unsupported { .. }));
    }
}
