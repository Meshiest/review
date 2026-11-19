//! GitHub reviewers, assignees and labels. The pickers are GraphQL
//! (`Collaborators`, `RepoLabels`); the writes are REST by login, slug and
//! label name, so no node ids are needed, each followed by a `PullRequest`
//! read for the answer. Writes are never resent.
//!
//! Operations: `Collaborators`, `RepoLabels`; REST `POST` and `DELETE`
//! `.../pulls/{n}/requested_reviewers`, `.../issues/{n}/assignees` and
//! `.../issues/{n}/labels[/{name}]`.

use serde::Deserialize;
use serde_json::{Value, json};

use super::{Actor, Connection, GitHub, LabelNode, user};
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::{HttpRequest, Method, RetryMode};
use crate::model::{Label, PrNumber, PullRequest, RepoRef, User};
use crate::people::{COLLABORATOR_LIMIT, plan_labels, plan_users, team_slug};

const COLLABORATORS: &str = "query Collaborators($owner:String!,$name:String!,$query:String,$first:Int!){\n     repository(owner:$owner,name:$name){assignableUsers(first:$first,query:$query){\n     nodes{__typename id login name avatarUrl}}}}";

const LABELS: &str = concat!(
    "query RepoLabels($owner:String!,$name:String!,$after:String){\
     repository(owner:$owner,name:$name){labels(first:100,after:$after,\
     orderBy:{field:NAME,direction:ASC}){",
    page_info!(),
    " nodes{name color description}}}}"
);

#[derive(Debug, Deserialize)]
struct CollaboratorsEnvelope {
    repository: Option<CollaboratorsRepo>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CollaboratorsRepo {
    assignable_users: Connection<Actor>,
}

#[derive(Debug, Deserialize)]
struct LabelsEnvelope {
    repository: Option<LabelsRepo>,
}

#[derive(Debug, Deserialize)]
struct LabelsRepo {
    labels: Option<Connection<LabelNode>>,
}

impl GitHub {
    async fn rest_write(
        &self,
        method: Method,
        repo: &RepoRef,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<()> {
        let request = HttpRequest {
            method,
            body,
            retry: RetryMode::Never,
            ..HttpRequest::get(self.rest_url(repo, segments)?)
        };
        self.http.send(request).await.map(drop)
    }
}

pub(super) async fn collaborators(
    github: &GitHub,
    repo: &RepoRef,
    query: &str,
) -> Result<Vec<User>> {
    github
        .picker
        .collaborators(github.http.now(), repo, query, || async {
            let search = Some(query.trim()).filter(|text| !text.is_empty());
            let envelope: CollaboratorsEnvelope = github
                .query(
                    "Collaborators",
                    COLLABORATORS,
                    json!({
                        "owner": repo.owner,
                        "name": repo.name,
                        "query": search,
                        "first": COLLABORATOR_LIMIT,
                    }),
                )
                .await?;
            let repository = envelope.repository.ok_or_else(|| Error::NotFound {
                request: format!("repository {repo}"),
            })?;
            Ok(repository
                .assignable_users
                .into_parts()
                .0
                .into_iter()
                .take(COLLABORATOR_LIMIT)
                .map(|actor| user(Some(actor)))
                .collect())
        })
        .await
}

pub(super) async fn labels(github: &GitHub, repo: &RepoRef) -> Result<Vec<Label>> {
    github
        .picker
        .labels(github.http.now(), repo, || fetch_labels(github, repo))
        .await
}

async fn fetch_labels(github: &GitHub, repo: &RepoRef) -> Result<Vec<Label>> {
    let nodes = github
        .paginate(
            None,
            "RepoLabels",
            LABELS,
            json!({"owner": repo.owner, "name": repo.name}),
            |envelope: LabelsEnvelope| envelope.repository?.labels,
        )
        .await?;
    Ok(nodes.into_iter().map(Label::from).collect())
}

/// The `reviewers` and `team_reviewers` body both reviewer endpoints take.
fn reviewer_body(users: &[User], teams: &[User]) -> Value {
    json!({
        "reviewers": users.iter().map(|user| user.login.as_str()).collect::<Vec<_>>(),
        "team_reviewers": teams.iter().map(|team| team_slug(&team.login)).collect::<Vec<_>>(),
    })
}

async fn change_reviewers(
    github: &GitHub,
    method: Method,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    if !users.is_empty() || !teams.is_empty() {
        let n = number.to_string();
        github
            .rest_write(
                method,
                repo,
                &["pulls", &n, "requested_reviewers"],
                Some(reviewer_body(users, teams)),
            )
            .await?;
    }
    Forge::pull_request(github, repo, number).await
}

pub(super) async fn request_reviewers(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    change_reviewers(github, Method::Post, repo, number, users, teams).await
}

pub(super) async fn remove_reviewers(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
    teams: &[User],
) -> Result<PullRequest> {
    change_reviewers(github, Method::Delete, repo, number, users, teams).await
}

pub(super) async fn set_assignees(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    users: &[User],
) -> Result<PullRequest> {
    let current = Forge::pull_request(github, repo, number).await?;
    let (add, remove) = plan_users(&current.assignees, users);
    if add.is_empty() && remove.is_empty() {
        return Ok(current);
    }
    let n = number.to_string();
    for (method, change) in [(Method::Post, add), (Method::Delete, remove)] {
        if change.is_empty() {
            continue;
        }
        let logins: Vec<&str> = change.iter().map(|user| user.login.as_str()).collect();
        github
            .rest_write(
                method,
                repo,
                &["issues", &n, "assignees"],
                Some(json!({"assignees": logins})),
            )
            .await?;
    }
    Forge::pull_request(github, repo, number).await
}

pub(super) async fn set_labels(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    labels: &[String],
) -> Result<PullRequest> {
    // GitHub makes a label out of an unknown name, so names are checked first.
    let wanted = github
        .picker
        .resolve_labels(github.http.now(), repo, labels, || {
            fetch_labels(github, repo)
        })
        .await?;
    let current = Forge::pull_request(github, repo, number).await?;
    let (add, remove) = plan_labels(&current.labels, &wanted);
    if add.is_empty() && remove.is_empty() {
        return Ok(current);
    }
    let n = number.to_string();
    if !add.is_empty() {
        github
            .rest_write(
                Method::Post,
                repo,
                &["issues", &n, "labels"],
                Some(json!({"labels": add})),
            )
            .await?;
    }
    for name in &remove {
        github
            .rest_write(Method::Delete, repo, &["issues", &n, "labels", name], None)
            .await?;
    }
    Forge::pull_request(github, repo, number).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_are_named_after_their_operation() {
        assert!(COLLABORATORS.starts_with("query Collaborators("));
        assert!(LABELS.starts_with("query RepoLabels("));
    }

    #[test]
    fn reviewer_bodies_name_logins_and_team_slugs() {
        let mut team = User::new("acme/core");
        team.kind = crate::model::AccountKind::Team;
        assert_eq!(
            reviewer_body(&[User::new("ann")], &[team]),
            json!({"reviewers": ["ann"], "team_reviewers": ["core"]})
        );
    }
}
