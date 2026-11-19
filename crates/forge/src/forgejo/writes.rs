//! Forgejo writes (REST, `RetryMode::Never`): the review endpoints [F1], a
//! reply as a one-comment review [F2], issue comments, and the v17 Actions
//! re-runs [F8]. Each returns the object the response carries, mapped like a
//! read. Resolve, viewed files and forge-side suggestion apply have no
//! v17.0.1 endpoint and keep the trait defaults.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    Api, ApiIssueComment, ApiJob, ApiReview, ApiReviewComment, Forgejo, real, status_state,
};
use crate::capabilities::Capabilities;
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::{HttpRequest, HttpResponse, Method, RetryMode};
use crate::model::{
    CiState, Comment, CommentAnchor, CommentId, ForgeKind, JobId, LineRef, NewComment,
    PendingComment, PendingPlace, PendingReview, PrNumber, RepoRef, Review, ReviewId, ReviewState,
    RunId, Sha, Side, ThreadId, Verdict,
};

/// The server refuses a review with no body and no comments in the same
/// request, which includes starting a pending one and submitting one whose
/// comments were queued earlier. A zero-width space passes its check (it is
/// not whitespace to it) and shows as no summary.
const NO_BODY: &str = "\u{200b}";

/// A comment's place in the diff as Forgejo takes it: the first line of the
/// comment and how many lines follow it.
struct Target {
    path: String,
    first: LineRef,
    extra: u32,
}

impl Target {
    fn from_anchor(anchor: &CommentAnchor, caps: &Capabilities) -> Result<Self> {
        let CommentAnchor::Line { path, line, start } = anchor else {
            return Err(unsupported("file comments"));
        };
        let Some(start) = start else {
            return Ok(Self {
                path: path.clone(),
                first: *line,
                extra: 0,
            });
        };
        if !caps.multi_line_comments {
            return Err(unsupported("multi-line comments"));
        }
        if start.side != line.side {
            return Err(unsupported("comment ranges across both sides"));
        }
        let first = start.line.min(line.line);
        let last = start.line.max(line.line);
        Ok(Self {
            path: path.clone(),
            first: LineRef {
                side: line.side,
                line: first,
            },
            extra: last - first,
        })
    }

    /// The request body of a review comment, which names one side's line.
    fn comment_body(&self, body: &str) -> Value {
        let (new_position, old_position) = match self.first.side {
            Side::New => (self.first.line, 0),
            Side::Old => (0, self.first.line),
        };
        json!({
            "path": self.path,
            "body": body,
            "new_position": new_position,
            "old_position": old_position,
            "extra_lines_count": self.extra,
        })
    }
}

/// Nothing a person wrote: empty, whitespace, or only [`NO_BODY`].
pub(super) fn is_blank(body: &str) -> bool {
    body.chars().all(|c| c.is_whitespace() || c == '\u{200b}')
}

fn unsupported(feature: &'static str) -> Error {
    Error::Unsupported {
        kind: ForgeKind::Forgejo,
        feature,
    }
}

fn decode_error(request: &str, reason: &str) -> Error {
    Error::Decode {
        request: request.to_owned(),
        reason: reason.to_owned(),
    }
}

impl Api {
    /// Sends a write: never resent after a 5xx or a lost response.
    pub(super) async fn write(
        &self,
        method: Method,
        segments: &[&str],
        body: Option<Value>,
    ) -> Result<HttpResponse> {
        let request = HttpRequest {
            method,
            body,
            retry: RetryMode::Never,
            ..HttpRequest::get(self.url(segments, &[])?)
        };
        self.http.send(request).await
    }

    async fn write_json<T: DeserializeOwned>(&self, segments: &[&str], body: Value) -> Result<T> {
        let request = HttpRequest::post_json(self.url(segments, &[])?, body);
        let describe = request.describe();
        self.http.send(request).await?.json(&describe)
    }

    /// The viewer's pending review; the server shows a pending review to its
    /// author only, so any `PENDING` row is the viewer's.
    async fn pending(&self, repo: &RepoRef, number: PrNumber) -> Result<Option<ApiReview>> {
        let reviews = self.reviews(repo, number).await?;
        Ok(reviews
            .into_iter()
            .filter(|review| review.state == "PENDING")
            .max_by_key(|review| review.id))
    }

    async fn review_comments(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        review: u64,
    ) -> Result<Vec<ApiReviewComment>> {
        self.paged(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
                &review.to_string(),
                "comments",
            ],
            &[],
            usize::MAX,
            |rows| rows,
        )
        .await
    }
}

pub(super) async fn start_review(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    commit: &Sha,
) -> Result<PendingReview> {
    let review: ApiReview = forgejo
        .api
        .write_json(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
            ],
            json!({"commit_id": commit.as_str(), "body": NO_BODY}),
        )
        .await?;
    if review.state != "PENDING" {
        return Err(decode_error(
            "POST pulls/reviews",
            &format!("a new review reads {:?}, not PENDING", review.state),
        ));
    }
    Ok(PendingReview {
        review: review_from(review),
        comments: Vec::new(),
    })
}

pub(super) async fn pending_review(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Option<PendingReview>> {
    let Some(review) = forgejo.api.pending(repo, number).await? else {
        return Ok(None);
    };
    let mut comments = forgejo.api.review_comments(repo, number, review.id).await?;
    comments.sort_by_key(|comment| (comment.created(), comment.id));
    Ok(Some(PendingReview {
        review: review_from(review),
        comments: comments
            .into_iter()
            .map(|comment| pending_comment(comment, None))
            .collect(),
    }))
}

pub(super) async fn add_review_comment(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    review: &ReviewId,
    comment: &NewComment,
) -> Result<PendingComment> {
    let (target, place) = match &comment.place {
        PendingPlace::New(anchor) => (Target::from_anchor(anchor, &forgejo.capabilities())?, None),
        PendingPlace::Reply(thread) => (
            thread_target(forgejo, repo, number, thread).await?,
            Some(comment.place.clone()),
        ),
    };
    let created: ApiReviewComment = forgejo
        .api
        .write_json(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
                review.as_str(),
                "comments",
            ],
            target.comment_body(&comment.body),
        )
        .await?;
    Ok(pending_comment(created, place))
}

pub(super) async fn delete_review_comment(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    comment: &CommentId,
) -> Result<()> {
    let Some(review) = forgejo.api.pending(repo, number).await? else {
        return Err(Error::NotFound {
            request: format!("pending review of {repo}#{number}"),
        });
    };
    forgejo
        .api
        .write(
            Method::Delete,
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
                &review.id.to_string(),
                "comments",
                comment.as_str(),
            ],
            None,
        )
        .await
        .map(drop)
}

pub(super) async fn submit_review(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    review: &ReviewId,
    verdict: Verdict,
    body: &str,
) -> Result<Review> {
    let event = match verdict {
        Verdict::Comment => "COMMENT",
        Verdict::Approve => "APPROVED",
        Verdict::RequestChanges => "REQUEST_CHANGES",
    };
    let submitted: ApiReview = forgejo
        .api
        .write_json(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
                review.as_str(),
            ],
            json!({"event": event, "body": if is_blank(body) { NO_BODY } else { body }}),
        )
        .await?;
    // An event the server does not know leaves the review pending.
    if submitted.state == "PENDING" {
        return Err(decode_error(
            "POST pulls/reviews/{id}",
            "the review is still pending after the submit",
        ));
    }
    Ok(review_from(submitted))
}

pub(super) async fn discard_review(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    review: &ReviewId,
) -> Result<()> {
    forgejo
        .api
        .write(
            Method::Delete,
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
                review.as_str(),
            ],
            None,
        )
        .await
        .map(drop)
}

/// A one-comment review submitted at once, on the thread's path and line, so
/// the next read groups it into the thread.
pub(super) async fn reply(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    thread: &ThreadId,
    body: &str,
) -> Result<Comment> {
    let target = thread_target(forgejo, repo, number, thread).await?;
    let review: ApiReview = forgejo
        .api
        .write_json(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "pulls",
                &number.to_string(),
                "reviews",
            ],
            json!({
                "event": "COMMENT",
                "body": "",
                "comments": [target.comment_body(body)],
            }),
        )
        .await?;
    if review.state == "PENDING" {
        return Err(decode_error(
            "POST pulls/reviews",
            "the reply review is still pending",
        ));
    }
    let comments = forgejo.api.review_comments(repo, number, review.id).await?;
    comments
        .into_iter()
        .find(|comment| comment.body == body)
        .map(ApiReviewComment::into_comment)
        .ok_or_else(|| decode_error("GET pulls/reviews/{id}/comments", "the reply is missing"))
}

pub(super) async fn add_comment(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    body: &str,
) -> Result<Comment> {
    let created: ApiIssueComment = forgejo
        .api
        .write_json(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "issues",
                &number.to_string(),
                "comments",
            ],
            json!({"body": body}),
        )
        .await?;
    Ok(created.into_comment())
}

pub(super) async fn retry_job(forgejo: &Forgejo, repo: &RepoRef, job: &JobId) -> Result<()> {
    if !forgejo.capabilities().retry_job {
        return Err(unsupported("re-running jobs"));
    }
    rerun_job(&forgejo.api, repo, job.as_str()).await
}

/// v17 re-runs a job and its dependents, or a whole run; "failed only" is
/// each failed job.
pub(super) async fn retry_failed_jobs(
    forgejo: &Forgejo,
    repo: &RepoRef,
    run: &RunId,
) -> Result<()> {
    if !forgejo.capabilities().retry_job {
        return Err(unsupported("re-running jobs"));
    }
    let mut jobs: Vec<ApiJob> = forgejo
        .api
        .get(
            &[
                "repos",
                &repo.owner,
                &repo.name,
                "actions",
                "runs",
                run.as_str(),
                "jobs",
            ],
            &[],
        )
        .await?;
    jobs.sort_by_key(|job| job.id);
    let mut restarted = false;
    for job in jobs
        .iter()
        .filter(|job| matches!(status_state(&job.status), CiState::Fail | CiState::Canceled))
    {
        match rerun_job(&forgejo.api, repo, &job.id.to_string()).await {
            Ok(()) => restarted = true,
            // A dependent of a job restarted a moment ago is not complete
            // any more, which the server answers with 400.
            Err(Error::Status { status: 400, .. }) if restarted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

async fn rerun_job(api: &Api, repo: &RepoRef, job: &str) -> Result<()> {
    api.write(
        Method::Post,
        &[
            "repos",
            &repo.owner,
            &repo.name,
            "actions",
            "jobs",
            job,
            "rerun",
        ],
        Some(json!({})),
    )
    .await
    .map(drop)
}

/// Where a thread's first comment sits, to post another on the same path and
/// line, with the same range, which is what the read groups by.
async fn thread_target(
    forgejo: &Forgejo,
    repo: &RepoRef,
    number: PrNumber,
    thread: &ThreadId,
) -> Result<Target> {
    let threads = Forge::threads(forgejo, repo, number).await?;
    let anchor = threads
        .iter()
        .filter(|candidate| candidate.id == *thread)
        .find_map(|candidate| candidate.anchor.as_ref())
        .ok_or_else(|| Error::NotFound {
            request: format!("thread {thread} of {repo}#{number}"),
        })?;
    let position = anchor
        .best_position()
        .ok_or_else(|| unsupported("replies to file comments"))?;
    Ok(Target {
        path: anchor.path.clone(),
        first: position.start.unwrap_or(position.line),
        extra: position
            .start
            .map_or(0, |start| position.line.line.abs_diff(start.line)),
    })
}

/// The anchor a comment was written at, as the composer would send it.
fn comment_anchor(comment: &ApiReviewComment) -> CommentAnchor {
    let path = comment.path.clone();
    let Some(first) = comment.side() else {
        return CommentAnchor::File { path };
    };
    let extra = comment.extra_lines_count;
    CommentAnchor::Line {
        path,
        line: LineRef {
            side: first.side,
            line: first.line + extra,
        },
        start: (extra > 0).then_some(first),
    }
}

/// `place` is what the caller asked for when it is not just the anchor: a
/// reply, which the server cannot link to its thread.
fn pending_comment(comment: ApiReviewComment, place: Option<PendingPlace>) -> PendingComment {
    PendingComment {
        id: CommentId::new(comment.id.to_string()),
        place: place.unwrap_or_else(|| PendingPlace::New(comment_anchor(&comment))),
        thread: None,
        created_at: Some(comment.created()),
        body: comment.body,
    }
}

fn review_from(review: ApiReview) -> Review {
    let state = review.review_state();
    let submitted_at = if state == ReviewState::Pending {
        None
    } else {
        real(review.submitted_at.or(review.updated_at))
    };
    Review {
        id: ReviewId::new(review.id.to_string()),
        author: review.author(),
        state,
        commit: Some(review.commit_id)
            .filter(|sha| !sha.is_empty())
            .map(Sha::new),
        submitted_at,
        web_url: Some(review.html_url).filter(|url| !url.is_empty()),
        body: if is_blank(&review.body) {
            String::new()
        } else {
            review.body
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(side: Side, line: u32) -> LineRef {
        LineRef { side, line }
    }

    fn range(start: Side, end: Side) -> CommentAnchor {
        CommentAnchor::Line {
            path: "a.rs".to_owned(),
            line: line(end, 5),
            start: Some(line(start, 3)),
        }
    }

    #[test]
    fn a_range_is_its_first_line_and_a_count() {
        let caps = Capabilities {
            multi_line_comments: true,
            ..Capabilities::GITHUB
        };
        let target = Target::from_anchor(&range(Side::New, Side::New), &caps).unwrap();
        assert_eq!(
            target.comment_body("x"),
            json!({"path": "a.rs", "body": "x", "new_position": 3,
                   "old_position": 0, "extra_lines_count": 2})
        );
        assert!(Target::from_anchor(&range(Side::Old, Side::New), &caps).is_err());
        let single_only = Capabilities {
            multi_line_comments: false,
            ..caps
        };
        assert!(Target::from_anchor(&range(Side::New, Side::New), &single_only).is_err());
        let file = CommentAnchor::File {
            path: "a.rs".to_owned(),
        };
        assert!(Target::from_anchor(&file, &caps).is_err());
    }

    #[test]
    fn an_old_side_comment_names_the_old_line() {
        let anchor = CommentAnchor::Line {
            path: "a.rs".to_owned(),
            line: line(Side::Old, 8),
            start: None,
        };
        let target = Target::from_anchor(&anchor, &Capabilities::GITHUB).unwrap();
        assert_eq!(
            target.comment_body("gone"),
            json!({"path": "a.rs", "body": "gone", "new_position": 0,
                   "old_position": 8, "extra_lines_count": 0})
        );
    }

    #[test]
    fn a_comment_reads_back_as_the_anchor_it_was_sent_at() {
        let comment: ApiReviewComment = serde_json::from_value(json!(
            {"id": 7, "path": "a.rs", "position": 3, "original_position": 0,
             "extra_lines_count": 2, "body": "b"}
        ))
        .unwrap();
        assert_eq!(
            comment_anchor(&comment),
            CommentAnchor::Line {
                path: "a.rs".to_owned(),
                line: line(Side::New, 5),
                start: Some(line(Side::New, 3)),
            }
        );
    }
}
