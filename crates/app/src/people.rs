//! Reviewers, assignees and labels of an open pull request: the writes the
//! Conversation's side column asks for, through the forge hub, patching the
//! pull request in place with what the forge answers. Fixtures make the
//! answer up from the request.

use forge::Capabilities;
use forge::model::{AccountKind, ForgeKind, Label, PullRequest, ReviewState, Reviewer, User};
use gpui::{Context, SharedString};

use crate::model::PrKey;
use crate::session::Session;

/// Which part of the side column a write changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Reviewers,
    Assignees,
    Labels,
}

/// One change to the pull request's people or labels.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeopleWrite {
    RequestReviewer(User),
    /// Asks everyone who already reviewed to review again.
    RequestAgain(Vec<User>),
    RemoveReviewer(User),
    SetAssignees(Vec<User>),
    /// Label names, as the repository spells them.
    SetLabels(Vec<String>),
}

impl PeopleWrite {
    #[must_use]
    pub fn field(&self) -> Field {
        match self {
            Self::RequestReviewer(_) | Self::RequestAgain(_) | Self::RemoveReviewer(_) => {
                Field::Reviewers
            }
            Self::SetAssignees(_) => Field::Assignees,
            Self::SetLabels(_) => Field::Labels,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::RequestReviewer(_) => "Requesting a review",
            Self::RequestAgain(_) => "Requesting a re-review",
            Self::RemoveReviewer(_) => "Removing a reviewer",
            Self::SetAssignees(_) => "Changing assignees",
            Self::SetLabels(_) => "Changing labels",
        }
    }
}

/// The side column's write in flight and the last failure, per open pull
/// request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PeopleState {
    pub busy: Option<Field>,
    pub error: Option<(Field, SharedString)>,
}

/// Why `user` cannot be asked to review a pull request by `author`, if the
/// forge refuses it: the author on GitHub and Forgejo, and teams where the
/// forge has no team requests.
#[must_use]
pub fn review_request_blocked(
    user: &User,
    author: &str,
    caps: &Capabilities,
    forge: ForgeKind,
) -> Option<String> {
    if user.kind == AccountKind::Team && !caps.team_reviewers {
        return Some(format!("{} has no review requests to teams", forge.label()));
    }
    (user.login.eq_ignore_ascii_case(author) && !caps.own_pull_request_verdicts).then(|| {
        format!(
            "{} does not let the author review their own pull request",
            forge.label()
        )
    })
}

fn same(a: &User, b: &User) -> bool {
    a.login.eq_ignore_ascii_case(&b.login)
}

/// `write` applied to `pr` as a forge would answer it, for fixtures.
/// `catalog` is the repository's labels.
pub fn apply_locally(pr: &mut PullRequest, write: &PeopleWrite, catalog: &[Label]) {
    match write {
        PeopleWrite::RequestReviewer(user) => request(pr, user),
        PeopleWrite::RequestAgain(users) => {
            for user in users {
                request(pr, user);
            }
        }
        PeopleWrite::RemoveReviewer(user) => pr.reviewers.retain(|r| !same(&r.user, user)),
        PeopleWrite::SetAssignees(users) => pr.assignees.clone_from(users),
        PeopleWrite::SetLabels(names) => {
            pr.labels = names
                .iter()
                .map(|name| {
                    catalog
                        .iter()
                        .find(|label| label.name.eq_ignore_ascii_case(name))
                        .cloned()
                        .unwrap_or_else(|| Label {
                            name: name.clone(),
                            color: None,
                            description: None,
                        })
                })
                .collect();
        }
    }
}

/// Asks `user` to review `pr`; asking someone who already reviewed is a
/// re-request.
fn request(pr: &mut PullRequest, user: &User) {
    match pr.reviewers.iter_mut().find(|r| same(&r.user, user)) {
        Some(row) => {
            row.requested_again = row.state != ReviewState::Requested;
            row.stale = false;
            row.state = ReviewState::Requested;
        }
        None => pr
            .reviewers
            .push(Reviewer::new(user.clone(), ReviewState::Requested)),
    }
}

/// Who a re-review would ask: everyone with a submitted review, people
/// and teams alike.
#[must_use]
pub fn reviewed_by(pr: &PullRequest) -> Vec<User> {
    pr.reviewers
        .iter()
        .filter(|r| {
            matches!(
                r.state,
                ReviewState::Commented
                    | ReviewState::Approved
                    | ReviewState::ChangesRequested
                    | ReviewState::Dismissed
            )
        })
        .map(|r| r.user.clone())
        .collect()
}

/// Whether a picker's `query` matches `user`: the login, or a word of the
/// display name, starts with it.
#[must_use]
pub fn matches(user: &User, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty()
        || user.login.to_lowercase().starts_with(&query)
        || user.name.as_deref().is_some_and(|name| {
            name.to_lowercase()
                .split_whitespace()
                .any(|word| word.starts_with(&query))
        })
}

impl Session {
    /// Sends `write` for `key`; one at a time per pull request. A refusal
    /// the app can see coming (see [`review_request_blocked`]) is shown, not
    /// sent.
    pub fn people_write(&mut self, key: &PrKey, write: PeopleWrite, cx: &mut Context<Self>) {
        let caps = self.capabilities_for(key);
        let services = self.services().cloned();
        let Some(open) = self.pr_mut(key) else {
            return;
        };
        if open.people.busy.is_some() {
            return;
        }
        let field = write.field();
        open.people.error = None;
        let Some(mut pr) = open.forge_pr().cloned() else {
            return;
        };
        if let (PeopleWrite::RequestReviewer(user), Some(caps)) = (&write, caps)
            && let Some(why) = review_request_blocked(user, &pr.author.login, &caps, open.pr.forge)
        {
            open.people.error = Some((field, why.into()));
            cx.notify();
            return;
        }
        let Some(services) = services else {
            apply_locally(&mut pr, &write, &crate::fixtures::labels());
            self.patch_forge_pr(key, pr, cx);
            return;
        };
        open.people.busy = Some(field);
        cx.notify();
        let (repo, number) = (key.repo.clone(), key.number);
        let sent = write.clone();
        let call = services.forge(&key.repo.host, move |forge| async move {
            let split = |user: &User| -> (Vec<User>, Vec<User>) {
                if user.kind == AccountKind::Team {
                    (Vec::new(), vec![user.clone()])
                } else {
                    (vec![user.clone()], Vec::new())
                }
            };
            match &sent {
                PeopleWrite::RequestReviewer(user) => {
                    let (users, teams) = split(user);
                    forge.request_reviewers(&repo, number, &users, &teams).await
                }
                PeopleWrite::RequestAgain(asked) => {
                    let (teams, users): (Vec<User>, Vec<User>) = asked
                        .iter()
                        .cloned()
                        .partition(|user| user.kind == AccountKind::Team);
                    forge.request_reviewers(&repo, number, &users, &teams).await
                }
                PeopleWrite::RemoveReviewer(user) => {
                    let (users, teams) = split(user);
                    forge.remove_reviewers(&repo, number, &users, &teams).await
                }
                PeopleWrite::SetAssignees(users) => forge.set_assignees(&repo, number, users).await,
                PeopleWrite::SetLabels(names) => forge.set_labels(&repo, number, names).await,
            }
        });
        let key = key.clone();
        cx.spawn(async move |this, cx| {
            let result = call.await;
            this.update(cx, |session, cx| {
                let Some(open) = session.pr_mut(&key) else {
                    return;
                };
                open.people.busy = None;
                match result {
                    Ok(pr) => session.patch_forge_pr(&key, pr, cx),
                    Err(error) => {
                        open.people.error =
                            Some((field, format!("{} failed: {error}", write.label()).into()));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_author_cannot_be_asked_where_the_forge_forbids_it() {
        let github = Capabilities::baseline(ForgeKind::GitHub);
        let gitlab = Capabilities::baseline(ForgeKind::GitLab);
        let author = User::new("Cora");
        assert_eq!(
            review_request_blocked(&author, "cora", &github, ForgeKind::GitHub).as_deref(),
            Some("GitHub does not let the author review their own pull request")
        );
        assert_eq!(
            review_request_blocked(&author, "cora", &gitlab, ForgeKind::GitLab),
            None
        );
        let team = User {
            kind: AccountKind::Team,
            ..User::new("acme/core")
        };
        assert!(review_request_blocked(&team, "cora", &gitlab, ForgeKind::GitLab).is_some());
        assert!(review_request_blocked(&team, "cora", &github, ForgeKind::GitHub).is_none());
    }

    #[test]
    fn local_writes_answer_like_a_forge() {
        let mut pr = crate::fixtures::Fixtures::load();
        let key = pr.inbox.prs[0].key.clone();
        let mut record = pr.forge_pr(&key).expect("a record");
        pr.inbox.prs.clear();
        let mira = User::new("MIRA");
        record.reviewers.clear();
        apply_locally(
            &mut record,
            &PeopleWrite::RequestReviewer(mira.clone()),
            &[],
        );
        apply_locally(
            &mut record,
            &PeopleWrite::RequestReviewer(User::new("mira")),
            &[],
        );
        assert_eq!(record.reviewers.len(), 1, "asked once");
        apply_locally(
            &mut record,
            &PeopleWrite::RemoveReviewer(User::new("mira")),
            &[],
        );
        assert!(record.reviewers.is_empty());
        let catalog = crate::fixtures::labels();
        apply_locally(
            &mut record,
            &PeopleWrite::SetLabels(vec!["BUG".to_owned()]),
            &catalog,
        );
        assert_eq!(record.labels[0].name, "bug");
        assert!(record.labels[0].color.is_some(), "the catalog's color");
    }

    #[test]
    fn a_re_review_asks_everyone_who_reviewed_again() {
        let fixtures = crate::fixtures::Fixtures::load();
        let key = fixtures.inbox.prs[0].key.clone();
        let mut record = fixtures.forge_pr(&key).expect("a record");
        record.reviewers = vec![
            Reviewer::new(User::new("mira"), ReviewState::Approved),
            Reviewer::new(User::new("wen"), ReviewState::Requested),
        ];
        let asked = reviewed_by(&record);
        assert_eq!(asked, [User::new("mira")], "only who reviewed");
        apply_locally(&mut record, &PeopleWrite::RequestAgain(asked), &[]);
        assert_eq!(record.reviewers[0].state, ReviewState::Requested);
        assert!(record.reviewers[0].requested_again);
        assert!(!record.reviewers[1].requested_again);
    }

    #[test]
    fn pickers_search_logins_and_name_words() {
        let mut user = User::new("wen");
        user.name = Some("Wen Zhou".to_owned());
        assert!(matches(&user, "WE") && matches(&user, "zho") && matches(&user, " "));
        assert!(!matches(&user, "hou"));
    }
}
