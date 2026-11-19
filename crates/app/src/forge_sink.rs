//! The [`ReviewSink`] that writes through the forge hub.
//!
//! - The first queued comment of a pull request starts the viewer's pending
//!   review, or picks up the one the forge already has (started on the web
//!   or in another session); later ones go into the same review.
//! - A single comment on code is a review of its own with that one comment,
//!   submitted as Comment. While another review is pending it is refused:
//!   one pending review per person is all GitHub and Forgejo keep, and
//!   GitLab's publish sends every draft note at once. A conversation comment
//!   is the forge's plain comment.
//! - Every outcome carries what the forge returned, so the app patches its
//!   threads instead of reading the pull request again. GitLab gives a
//!   published draft note a new id, so after a submit there it reads the
//!   threads once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::Utc;
use forge::model::{
    Anchor, Comment, CommentAnchor, CommentId, ForgeKind, NewComment as ForgeComment,
    PendingComment, PendingPlace, PendingReview, Position, PrNumber, RepoRef, Resolution, Review,
    ReviewId, ReviewState, Sha, Thread, ThreadId,
};
use forge::{Error, Forge};
use futures::future::{BoxFuture, FutureExt as _};
use store::PrKey;

use crate::services::Services;
use crate::sink::{
    NewComment, Place, ReviewCommand, ReviewSink, SinkError, SinkOutcome, ThreadChange, Verdict,
};

type Reviews = Arc<Mutex<HashMap<PrKey, ReviewId>>>;

pub struct ForgeSink {
    services: Services,
    /// The pending review each pull request's comments go into, once known.
    reviews: Reviews,
}

impl ForgeSink {
    #[must_use]
    pub fn new(services: Services) -> Self {
        Self {
            services,
            reviews: Arc::default(),
        }
    }
}

impl ReviewSink for ForgeSink {
    fn submit(
        &self,
        pr: &PrKey,
        command: ReviewCommand,
    ) -> BoxFuture<'static, Result<SinkOutcome, SinkError>> {
        let (reviews, key) = (self.reviews.clone(), pr.clone());
        let call = self.services.forge(&pr.repo.host, move |forge| async move {
            let write = Write {
                forge: forge.as_ref(),
                reviews: &reviews,
                key: &key,
            };
            Ok(write.run(command).await)
        });
        async move { call.await.map_err(|error| forge_error(&error))? }.boxed()
    }

    fn pending(&self, pr: &PrKey) -> BoxFuture<'static, Result<Option<PendingReview>, SinkError>> {
        let (reviews, key) = (self.reviews.clone(), pr.clone());
        let call = self.services.forge(&pr.repo.host, move |forge| async move {
            let pending = forge.pending_review(&key.repo, key.number).await;
            if let Ok(Some(found)) = &pending {
                lock(&reviews).insert(key, found.review.id.clone());
            }
            pending
        });
        async move {
            match call.await {
                Ok(pending) => Ok(pending),
                // A forge without queued reviews simply has none pending.
                Err(Error::Unsupported { .. }) => Ok(None),
                Err(error) => Err(forge_error(&error)),
            }
        }
        .boxed()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The forge's own message where it sent one, otherwise the whole error.
fn forge_error(error: &Error) -> SinkError {
    SinkError(match error {
        Error::Status { message, .. } if !message.trim().is_empty() => message.trim().to_owned(),
        Error::GraphQl { message, .. } => message.clone(),
        other => other.to_string(),
    })
}

/// One command against one backend.
struct Write<'a> {
    forge: &'a dyn Forge,
    reviews: &'a Reviews,
    key: &'a PrKey,
}

impl Write<'_> {
    fn repo(&self) -> &RepoRef {
        &self.key.repo
    }

    fn number(&self) -> PrNumber {
        self.key.number
    }

    async fn run(&self, command: ReviewCommand) -> Result<SinkOutcome, SinkError> {
        match command {
            ReviewCommand::QueueComment(comment) => self.queue(comment).await,
            ReviewCommand::SendComment(comment) => self.send(comment).await,
            ReviewCommand::Reply { thread, body } => {
                let comment = self
                    .forge(self.forge.reply(self.repo(), self.number(), &thread, &body))
                    .await?;
                Ok(SinkOutcome::Replied { thread, comment })
            }
            ReviewCommand::SubmitReview { verdict, body } => self.submit(verdict, &body).await,
            ReviewCommand::SetResolved { thread, resolved } => {
                let thread = self
                    .forge(
                        self.forge
                            .set_resolved(self.repo(), self.number(), &thread, resolved),
                    )
                    .await?;
                Ok(SinkOutcome::Resolved(Box::new(thread)))
            }
            ReviewCommand::DeleteQueued(comment) => {
                self.forge(
                    self.forge
                        .delete_review_comment(self.repo(), self.number(), &comment),
                )
                .await?;
                Ok(SinkOutcome::Done)
            }
            ReviewCommand::EditQueued { comment, body } => self.edit(&comment, &body).await,
        }
    }

    async fn forge<T>(&self, call: impl Future<Output = forge::Result<T>>) -> Result<T, SinkError> {
        call.await.map_err(|error| forge_error(&error))
    }

    fn cached_review(&self) -> Option<ReviewId> {
        lock(self.reviews).get(self.key).cloned()
    }

    fn remember(&self, review: Option<ReviewId>) {
        let mut reviews = lock(self.reviews);
        match review {
            Some(review) => reviews.insert(self.key.clone(), review),
            None => reviews.remove(self.key),
        };
    }

    /// The head a new review starts on: the one the comment was written
    /// against, or the pull request's current head.
    async fn head(&self, commit: Option<Sha>) -> Result<Sha, SinkError> {
        match commit {
            Some(commit) => Ok(commit),
            None => Ok(self
                .forge(self.forge.pull_request(self.repo(), self.number()))
                .await?
                .head
                .sha),
        }
    }

    /// The pending review to add to: the one this sink started, the one the
    /// forge has, or a new one on `commit`. The flag says it came from the
    /// cache, which may be stale.
    async fn review(&self, commit: Option<Sha>) -> Result<(ReviewId, bool), SinkError> {
        if let Some(review) = self.cached_review() {
            return Ok((review, true));
        }
        let pending = self
            .forge(self.forge.pending_review(self.repo(), self.number()))
            .await?;
        let review = match pending {
            Some(pending) => pending.review.id,
            None => {
                let head = self.head(commit).await?;
                self.forge(self.forge.start_review(self.repo(), self.number(), &head))
                    .await?
                    .review
                    .id
            }
        };
        self.remember(Some(review.clone()));
        Ok((review, false))
    }

    async fn queue(&self, comment: NewComment) -> Result<SinkOutcome, SinkError> {
        let anchor = inline_anchor(&comment.place)?;
        let (review, cached) = self.review(comment.commit.clone()).await?;
        let new = ForgeComment {
            place: PendingPlace::New(anchor),
            body: comment.body,
        };
        match self
            .forge(
                self.forge
                    .add_review_comment(self.repo(), self.number(), &review, &new),
            )
            .await
        {
            Ok(pending) => Ok(SinkOutcome::Queued(pending)),
            Err(error) => {
                // The review may have been submitted or dropped elsewhere;
                // the next try asks the forge again.
                if cached {
                    self.remember(None);
                }
                Err(error)
            }
        }
    }

    async fn send(&self, comment: NewComment) -> Result<SinkOutcome, SinkError> {
        if comment.place == Place::Conversation {
            let posted = self
                .forge(
                    self.forge
                        .add_comment(self.repo(), self.number(), &comment.body),
                )
                .await?;
            return Ok(SinkOutcome::Posted(ThreadChange::Add {
                threads: vec![conversation_thread(posted)],
                replies: Vec::new(),
            }));
        }
        let anchor = inline_anchor(&comment.place)?;
        let pending = self
            .forge(self.forge.pending_review(self.repo(), self.number()))
            .await?;
        if let Some(pending) = &pending
            && !pending.comments.is_empty()
        {
            return Err(SinkError(format!(
                "A review with {} queued comments is pending: add this one to it, or submit \
                 the review first.",
                pending.comments.len()
            )));
        }
        let (review, started) = match pending {
            Some(pending) => (pending.review.id, false),
            None => {
                let head = self.head(comment.commit.clone()).await?;
                let started = self
                    .forge(self.forge.start_review(self.repo(), self.number(), &head))
                    .await?;
                (started.review.id, true)
            }
        };
        let new = ForgeComment {
            place: PendingPlace::New(anchor),
            body: comment.body,
        };
        let added = self
            .forge(
                self.forge
                    .add_review_comment(self.repo(), self.number(), &review, &new),
            )
            .await;
        let submitted = match added {
            Ok(added) => match self
                .forge(self.forge.submit_review(
                    self.repo(),
                    self.number(),
                    &review,
                    forge::model::Verdict::Comment,
                    "",
                ))
                .await
            {
                Ok(submitted) => Ok((added, submitted)),
                Err(error) => Err((Some(added), error)),
            },
            Err(error) => Err((None, error)),
        };
        let (added, submitted) = match submitted {
            Ok(done) => done,
            Err((added, error)) => {
                // Leave nothing of this one comment pending on the forge.
                if started {
                    let _ = self
                        .forge
                        .discard_review(self.repo(), self.number(), &review)
                        .await;
                } else if let Some(added) = added {
                    let _ = self
                        .forge
                        .delete_review_comment(self.repo(), self.number(), &added.id)
                        .await;
                }
                return Err(error);
            }
        };
        self.remember(None);
        let change = self
            .change(
                &submitted,
                std::slice::from_ref(&added),
                comment.commit.as_ref(),
            )
            .await?;
        Ok(SinkOutcome::Posted(change))
    }

    async fn submit(&self, verdict: Verdict, body: &str) -> Result<SinkOutcome, SinkError> {
        let pending = self
            .forge(self.forge.pending_review(self.repo(), self.number()))
            .await?;
        let (review, comments, commit, started) = match pending {
            Some(pending) => (
                pending.review.id,
                pending.comments,
                pending.review.commit,
                false,
            ),
            None if verdict == Verdict::Comment && body.trim().is_empty() => {
                return Err(SinkError(
                    "Nothing to submit: the forge has no pending review for this pull request."
                        .to_owned(),
                ));
            }
            None => {
                let head = self.head(None).await?;
                let started = self
                    .forge(self.forge.start_review(self.repo(), self.number(), &head))
                    .await?;
                (started.review.id, Vec::new(), Some(head), true)
            }
        };
        let submitted = self
            .forge(self.forge.submit_review(
                self.repo(),
                self.number(),
                &review,
                verdict.to_forge(),
                body,
            ))
            .await;
        let submitted = match submitted {
            Ok(submitted) => submitted,
            Err(error) => {
                // A review started only for this submit is not left pending.
                if started {
                    let _ = self
                        .forge
                        .discard_review(self.repo(), self.number(), &review)
                        .await;
                }
                return Err(error);
            }
        };
        self.remember(None);
        let mut change = self.change(&submitted, &comments, commit.as_ref()).await?;
        if let ThreadChange::Add { threads, .. } = &mut change
            && (!submitted.body.trim().is_empty() || verdict != Verdict::Comment)
        {
            threads.push(summary_thread(&submitted));
        }
        Ok(SinkOutcome::Submitted {
            review: Box::new(submitted),
            change,
        })
    }

    async fn edit(&self, comment: &PendingComment, body: &str) -> Result<SinkOutcome, SinkError> {
        if self.forge.capabilities().edit_queued {
            let edited = self
                .forge(
                    self.forge
                        .edit_review_comment(self.repo(), self.number(), comment, body),
                )
                .await?;
            return Ok(SinkOutcome::Edited(edited));
        }
        let (review, _) = self.review(None).await?;
        self.forge(
            self.forge
                .delete_review_comment(self.repo(), self.number(), &comment.id),
        )
        .await?;
        let new = ForgeComment {
            place: comment.place.clone(),
            body: body.to_owned(),
        };
        let added = self
            .forge(
                self.forge
                    .add_review_comment(self.repo(), self.number(), &review, &new),
            )
            .await
            .map_err(|error| {
                SinkError(format!(
                    "The old comment was removed from the review, but the new text was not \
                     added: {error}"
                ))
            })?;
        Ok(SinkOutcome::Edited(added))
    }

    /// The threads a submitted review made from its comments. GitLab gives
    /// the published notes ids the drafts did not have, so there the threads
    /// are read again.
    async fn change(
        &self,
        review: &Review,
        comments: &[PendingComment],
        commit: Option<&Sha>,
    ) -> Result<ThreadChange, SinkError> {
        if self.forge.kind() == ForgeKind::GitLab && !comments.is_empty() {
            let threads = self
                .forge(self.forge.threads(self.repo(), self.number()))
                .await?;
            return Ok(ThreadChange::All(threads));
        }
        Ok(threads_of(review, comments, commit))
    }
}

fn inline_anchor(place: &Place) -> Result<CommentAnchor, SinkError> {
    place
        .comment_anchor()
        .ok_or_else(|| SinkError("a conversation comment cannot go into a review".to_owned()))
}

/// The submitted comments of `review` as threads and replies, the way a read
/// would show them.
fn threads_of(review: &Review, comments: &[PendingComment], commit: Option<&Sha>) -> ThreadChange {
    let mut threads = Vec::new();
    let mut replies = Vec::new();
    for pending in comments {
        let comment = submitted_comment(review, pending);
        match &pending.place {
            PendingPlace::Reply(thread) => replies.push((thread.clone(), comment)),
            PendingPlace::New(anchor) => {
                // Forgejo has no thread object: its thread is named after its
                // first comment, as the backend's read names it.
                let id = pending
                    .thread
                    .clone()
                    .unwrap_or_else(|| ThreadId::new(pending.id.as_str()));
                threads.push(Thread {
                    id,
                    anchor: Some(thread_anchor(anchor, commit.or(review.commit.as_ref()))),
                    resolution: Resolution::Unresolved,
                    outdated: false,
                    comments: vec![comment],
                });
            }
        }
    }
    ThreadChange::Add { threads, replies }
}

fn submitted_comment(review: &Review, pending: &PendingComment) -> Comment {
    Comment {
        id: pending.id.clone(),
        author: review.author.clone(),
        body: pending.body.clone(),
        created_at: pending
            .created_at
            .or(review.submitted_at)
            .unwrap_or_else(Utc::now),
        edited_at: None,
        web_url: None,
        review: Some(review.id.clone()),
        verdict: None,
        suggestions: Vec::new(),
    }
}

fn thread_anchor(anchor: &CommentAnchor, commit: Option<&Sha>) -> Anchor {
    let position = match anchor {
        CommentAnchor::Line { line, start, .. } => Some(Position {
            line: *line,
            start: *start,
            commit: commit.cloned(),
        }),
        CommentAnchor::File { .. } => None,
    };
    Anchor {
        path: anchor.path().to_owned(),
        old_path: None,
        position,
        original: None,
    }
}

/// A conversation comment as the one-comment thread a read shows it as.
fn conversation_thread(comment: Comment) -> Thread {
    Thread {
        id: ThreadId::new(comment.id.as_str()),
        anchor: None,
        resolution: Resolution::NotResolvable,
        outdated: false,
        comments: vec![comment],
    }
}

/// The review's summary and verdict, as the conversation shows it.
fn summary_thread(review: &Review) -> Thread {
    let id = format!("review-{}", review.id);
    conversation_thread(Comment {
        id: CommentId::new(id),
        author: review.author.clone(),
        body: review.body.clone(),
        created_at: review.submitted_at.unwrap_or_else(Utc::now),
        edited_at: None,
        web_url: review.web_url.clone(),
        review: Some(review.id.clone()),
        verdict: Some(match review.state {
            ReviewState::Pending => ReviewState::Commented,
            state => state,
        }),
        suggestions: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{LineRef, Side, User};

    fn review() -> Review {
        Review {
            id: ReviewId::new("R"),
            author: User::new("cora"),
            state: ReviewState::Commented,
            body: String::new(),
            commit: Some(Sha::new("h")),
            submitted_at: None,
            web_url: None,
        }
    }

    fn pending(id: &str, place: PendingPlace, thread: Option<&str>) -> PendingComment {
        PendingComment {
            id: CommentId::new(id),
            place,
            thread: thread.map(ThreadId::new),
            body: format!("body {id}"),
            created_at: None,
        }
    }

    #[test]
    fn a_submitted_review_becomes_new_threads_and_replies() {
        let line = CommentAnchor::Line {
            path: "a.rs".into(),
            line: LineRef {
                side: Side::New,
                line: 3,
            },
            start: None,
        };
        let comments = [
            pending("C1", PendingPlace::New(line.clone()), Some("T1")),
            pending("C2", PendingPlace::New(line), None),
            pending("C3", PendingPlace::Reply(ThreadId::new("T0")), None),
        ];
        let ThreadChange::Add { threads, replies } = threads_of(&review(), &comments, None) else {
            panic!("expected new threads");
        };
        let ids: Vec<_> = threads.iter().map(|t| t.id.as_str()).collect();
        // A forge without thread ids names the thread after its first comment.
        assert_eq!(ids, ["T1", "C2"]);
        assert_eq!(threads[0].comments[0].author.login, "cora");
        let position = threads[0]
            .anchor
            .as_ref()
            .unwrap()
            .position
            .as_ref()
            .unwrap();
        assert_eq!(position.line.line, 3);
        assert_eq!(position.commit, Some(Sha::new("h")));
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].0, ThreadId::new("T0"));
    }

    #[test]
    fn the_summary_carries_the_verdict() {
        let mut approved = review();
        approved.state = ReviewState::Approved;
        approved.body = "ship it".into();
        let thread = summary_thread(&approved);
        assert!(thread.anchor.is_none());
        assert_eq!(thread.comments[0].verdict, Some(ReviewState::Approved));
    }

    #[test]
    fn forge_messages_are_shown_without_the_request() {
        let error = Error::Status {
            request: "POST https://api.github.com/graphql".into(),
            status: 422,
            message: "Can not approve your own pull request".into(),
        };
        assert_eq!(
            forge_error(&error).0,
            "Can not approve your own pull request"
        );
    }
}
