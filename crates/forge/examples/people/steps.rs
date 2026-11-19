//! The forge side of the people round trip: labels, assignees and reviewers
//! on one scratch pull request, each read back through the same backend.
//! Shared by `examples/people` (live) and the replay tests, so a replay sends
//! exactly the requests that were recorded.
//!
//! The scratch pull request starts with no labels, assignees or reviewers
//! and ends the same way. The label is [`LABEL`], which the live run creates
//! in the sandbox before it starts when the repository lacks it.

use std::fmt;

use forge::model::{PrNumber, PullRequest, RepoRef, ReviewState, User};
use forge::{COLLABORATOR_LIMIT, Error, Forge};

/// The sandbox label the steps put on and take off the pull request.
pub const LABEL: &str = "review-test";

/// A label name no repository has.
const MISSING_LABEL: &str = "review-test-missing";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pass(String),
    Fail(String),
    /// Why the step is not run, or the refusal that stopped it.
    Skip(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub step: &'static str,
    pub status: Status,
}

impl Outcome {
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.status, Status::Fail(_))
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.status {
            Status::Pass(detail) => write!(f, "PASS {}: {detail}", self.step),
            Status::Fail(reason) => write!(f, "FAIL {}: {reason}", self.step),
            Status::Skip(why) => write!(f, "SKIP {}: {why}", self.step),
        }
    }
}

type Step = Result<String, String>;

fn ensure(ok: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message()) }
}

fn err(error: Error) -> String {
    error.to_string()
}

fn same_login(a: &User, b: &User) -> bool {
    a.login.eq_ignore_ascii_case(&b.login)
}

fn label_names(pr: &PullRequest) -> Vec<&str> {
    pr.labels.iter().map(|label| label.name.as_str()).collect()
}

fn logins(users: &[User]) -> Vec<&str> {
    users.iter().map(|user| user.login.as_str()).collect()
}

/// The first three characters of `login`, lowercased: a prefix the picker
/// must match.
fn prefix(login: &str) -> String {
    login.chars().take(3).collect::<String>().to_lowercase()
}

/// What asking for a review of the viewer's own pull request came to.
enum Requested {
    Done(String),
    Refused(String),
}

pub async fn run(
    forge: &dyn Forge,
    repo: &RepoRef,
    number: PrNumber,
    mut report: impl FnMut(&Outcome),
) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    let mut push = |step: &'static str, status: Status| {
        let outcome = Outcome { step, status };
        report(&outcome);
        outcomes.push(outcome);
    };
    let status = |result: Step| match result {
        Ok(detail) => Status::Pass(detail),
        Err(reason) => Status::Fail(reason),
    };

    let me = match forge.whoami().await {
        Ok(me) => me,
        Err(error) => {
            push("whoami", Status::Fail(err(error)));
            return outcomes;
        }
    };
    let people = People {
        forge,
        repo,
        number,
        me: &me,
    };
    push("fresh", status(people.fresh().await));
    push("labels-list", status(people.labels_list().await));
    push("collaborators", status(people.collaborators().await));
    push("label-add", status(people.label_add().await));
    push("label-unknown", status(people.label_unknown().await));
    push("assign", status(people.assign().await));
    let requested = match people.request_review().await {
        Ok(Requested::Done(detail)) => {
            push("request-review", Status::Pass(detail));
            true
        }
        Ok(Requested::Refused(why)) => {
            push("request-review", Status::Skip(why));
            false
        }
        Err(reason) => {
            push("request-review", Status::Fail(reason));
            false
        }
    };
    push("snapshot", status(people.snapshot(requested).await));
    push("unassign", status(people.unassign().await));
    push("label-remove", status(people.label_remove().await));
    if requested {
        push("unrequest-review", status(people.unrequest_review().await));
    } else {
        push(
            "unrequest-review",
            Status::Skip("no review was requested".into()),
        );
    }
    push(
        "team-reviewers",
        Status::Skip(if forge.capabilities().team_reviewers {
            "the sandbox is a personal repository with no team".into()
        } else {
            "team_reviewers: the forge has no review requests to groups".into()
        }),
    );
    outcomes
}

struct People<'a> {
    forge: &'a dyn Forge,
    repo: &'a RepoRef,
    number: PrNumber,
    me: &'a User,
}

fn reviewer_rows(pr: &PullRequest) -> Vec<(&str, ReviewState)> {
    pr.reviewers
        .iter()
        .map(|row| (row.user.login.as_str(), row.state))
        .collect()
}

impl People<'_> {
    async fn pull(&self) -> Result<PullRequest, String> {
        self.forge
            .pull_request(self.repo, self.number)
            .await
            .map_err(err)
    }

    fn is_me(&self, users: &[User]) -> bool {
        users.len() == 1 && same_login(&users[0], self.me)
    }

    fn is_requested(&self, pr: &PullRequest) -> bool {
        pr.reviewers.len() == 1
            && same_login(&pr.reviewers[0].user, self.me)
            && pr.reviewers[0].state == ReviewState::Requested
            && !pr.reviewers[0].is_team()
    }

    async fn fresh(&self) -> Step {
        let pr = self.pull().await?;
        ensure(
            pr.reviewers.is_empty() && pr.assignees.is_empty() && pr.labels.is_empty(),
            || {
                format!(
                    "a new pull request has reviewers {:?}, assignees {:?}, labels {:?}",
                    reviewer_rows(&pr),
                    logins(&pr.assignees),
                    label_names(&pr)
                )
            },
        )?;
        ensure(pr.milestone.is_none(), || {
            format!("a new pull request has milestone {:?}", pr.milestone)
        })?;
        Ok("no reviewers, assignees, labels or milestone".into())
    }

    async fn labels_list(&self) -> Step {
        let labels = self.forge.labels(self.repo).await.map_err(err)?;
        let Some(label) = labels.iter().find(|label| label.name == LABEL) else {
            return Err(format!("the repository has no {LABEL} label"));
        };
        ensure(label.color.as_ref().is_some_and(|c| c.len() == 6), || {
            format!("the color reads {:?}", label.color)
        })?;
        Ok(format!(
            "{} labels, {LABEL} is #{} ({})",
            labels.len(),
            label.color.as_deref().unwrap_or_default(),
            label.description.as_deref().unwrap_or("no description")
        ))
    }

    async fn collaborators(&self) -> Step {
        let all = self.forge.collaborators(self.repo, "").await.map_err(err)?;
        ensure(all.len() <= COLLABORATOR_LIMIT, || {
            format!("{} users is over the limit", all.len())
        })?;
        ensure(all.iter().any(|user| same_login(user, self.me)), || {
            format!("{} is not among {:?}", self.me.login, logins(&all))
        })?;
        let wanted = prefix(&self.me.login);
        let found = self
            .forge
            .collaborators(self.repo, &wanted)
            .await
            .map_err(err)?;
        ensure(found.iter().any(|user| same_login(user, self.me)), || {
            format!("the prefix {wanted:?} misses {}", self.me.login)
        })?;
        let none = self
            .forge
            .collaborators(self.repo, "zzzz-nobody")
            .await
            .map_err(err)?;
        ensure(none.is_empty(), || {
            format!("a prefix nobody has found {:?}", logins(&none))
        })?;
        Ok(format!(
            "{} users, {} for {wanted:?}",
            all.len(),
            found.len()
        ))
    }

    async fn label_add(&self) -> Step {
        let answer = self
            .forge
            .set_labels(self.repo, self.number, &[LABEL.to_owned()])
            .await
            .map_err(err)?;
        ensure(label_names(&answer) == [LABEL], || {
            format!("the answer reads {:?}", label_names(&answer))
        })?;
        let again = self.pull().await?;
        ensure(label_names(&again) == [LABEL], || {
            format!("read back as {:?}", label_names(&again))
        })?;
        ensure(again.labels[0].color.is_some(), || {
            "the label came back without a color".to_owned()
        })?;
        Ok(format!("{LABEL} on the pull request"))
    }

    /// A name the repository lacks is refused and nothing changes (several
    /// forges would make a label of it).
    async fn label_unknown(&self) -> Step {
        let refused = self
            .forge
            .set_labels(
                self.repo,
                self.number,
                &[LABEL.to_owned(), MISSING_LABEL.to_owned()],
            )
            .await;
        let Err(error) = refused else {
            return Err("an unknown label was accepted".to_owned());
        };
        ensure(matches!(error, Error::NotFound { .. }), || {
            format!("refused with {error:?}, not NotFound")
        })?;
        let again = self.pull().await?;
        ensure(label_names(&again) == [LABEL], || {
            format!(
                "after the refusal the labels read {:?}",
                label_names(&again)
            )
        })?;
        Ok(format!("refused ({error}), labels unchanged"))
    }

    async fn assign(&self) -> Step {
        let answer = self
            .forge
            .set_assignees(self.repo, self.number, std::slice::from_ref(self.me))
            .await
            .map_err(err)?;
        ensure(self.is_me(&answer.assignees), || {
            format!("the answer reads {:?}", logins(&answer.assignees))
        })?;
        let again = self.pull().await?;
        ensure(self.is_me(&again.assignees), || {
            format!("read back as {:?}", logins(&again.assignees))
        })?;
        Ok(format!("assigned to {}", self.me.login))
    }

    async fn request_review(&self) -> Result<Requested, String> {
        let own_allowed = self.forge.capabilities().own_pull_request_verdicts;
        let asked = self
            .forge
            .request_reviewers(self.repo, self.number, std::slice::from_ref(self.me), &[])
            .await;
        match asked {
            Ok(answer) => {
                ensure(self.is_requested(&answer), || {
                    format!("the answer lists {:?}", reviewer_rows(&answer))
                })?;
                let again = self.pull().await?;
                ensure(self.is_requested(&again), || {
                    format!("read back as {:?}", reviewer_rows(&again))
                })?;
                let note = if own_allowed {
                    ""
                } else {
                    " (the forge was expected to refuse the author)"
                };
                Ok(Requested::Done(format!(
                    "{} asked to review{note}",
                    self.me.login
                )))
            }
            Err(error) if !own_allowed => {
                let again = self.pull().await?;
                ensure(again.reviewers.is_empty(), || {
                    format!(
                        "refused, but the reviewers read {:?}",
                        reviewer_rows(&again)
                    )
                })?;
                Ok(Requested::Refused(format!(
                    "own_pull_request_verdicts: the forge refuses the author as reviewer ({error})"
                )))
            }
            Err(error) => Err(err(error)),
        }
    }

    /// What `snapshot` reports is what `pull_request` does.
    async fn snapshot(&self, requested: bool) -> Step {
        let pr = self.pull().await?;
        let snapshot = self
            .forge
            .snapshot(self.repo, self.number)
            .await
            .map_err(err)?
            .pull_request;
        ensure(snapshot.labels == pr.labels, || {
            format!(
                "snapshot labels {:?}, pull request labels {:?}",
                label_names(&snapshot),
                label_names(&pr)
            )
        })?;
        ensure(snapshot.assignees == pr.assignees, || {
            format!(
                "snapshot assignees {:?}, pull request assignees {:?}",
                logins(&snapshot.assignees),
                logins(&pr.assignees)
            )
        })?;
        ensure(snapshot.reviewers == pr.reviewers, || {
            format!(
                "snapshot reviewers {:?}, pull request reviewers {:?}",
                reviewer_rows(&snapshot),
                reviewer_rows(&pr)
            )
        })?;
        ensure(label_names(&snapshot) == [LABEL], || {
            "the snapshot lacks the label".to_owned()
        })?;
        ensure(self.is_me(&snapshot.assignees), || {
            "the snapshot lacks the assignee".to_owned()
        })?;
        ensure(snapshot.reviewers.len() == usize::from(requested), || {
            format!("the snapshot reviewers read {:?}", reviewer_rows(&snapshot))
        })?;
        Ok("snapshot and pull_request agree on labels, assignees and reviewers".into())
    }

    async fn unassign(&self) -> Step {
        let answer = self
            .forge
            .set_assignees(self.repo, self.number, &[])
            .await
            .map_err(err)?;
        ensure(answer.assignees.is_empty(), || {
            format!("the answer reads {:?}", logins(&answer.assignees))
        })?;
        let again = self.pull().await?;
        ensure(again.assignees.is_empty(), || {
            format!("read back as {:?}", logins(&again.assignees))
        })?;
        Ok("no assignees".into())
    }

    async fn label_remove(&self) -> Step {
        let answer = self
            .forge
            .set_labels(self.repo, self.number, &[])
            .await
            .map_err(err)?;
        ensure(answer.labels.is_empty(), || {
            format!("the answer reads {:?}", label_names(&answer))
        })?;
        let again = self.pull().await?;
        ensure(again.labels.is_empty(), || {
            format!("read back as {:?}", label_names(&again))
        })?;
        let known = self.forge.labels(self.repo).await.map_err(err)?;
        ensure(
            known.iter().any(|label| label.name == LABEL)
                && known.iter().all(|label| label.name != MISSING_LABEL),
            || "the repository labels changed".to_owned(),
        )?;
        Ok("no labels, the repository labels are as they were".into())
    }

    async fn unrequest_review(&self) -> Step {
        let answer = self
            .forge
            .remove_reviewers(self.repo, self.number, std::slice::from_ref(self.me), &[])
            .await
            .map_err(err)?;
        ensure(answer.reviewers.is_empty(), || {
            format!("the answer lists {:?}", reviewer_rows(&answer))
        })?;
        let again = self.pull().await?;
        ensure(again.reviewers.is_empty(), || {
            format!("read back as {:?}", reviewer_rows(&again))
        })?;
        Ok("no reviewers".into())
    }
}
