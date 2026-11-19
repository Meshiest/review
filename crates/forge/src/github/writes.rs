//! GitHub writes: one named GraphQL mutation per operation, sent without
//! retries (`HttpClient::graphql_mutation`), and REST for Actions re-runs
//! [G7]. Each returns the object the mutation's payload selects, mapped the
//! same way as the reads.
//!
//! Operations: `PullRequestId` (the node id writes address, cached),
//! `PendingReview`, `AddPullRequestReview`, `AddPullRequestReviewThread`,
//! `AddPullRequestReviewThreadReply`, `DeletePullRequestReviewComment`,
//! `UpdatePullRequestReviewComment`,
//! `SubmitPullRequestReview`, `DeletePullRequestReview`,
//! `ResolveReviewThread`, `UnresolveReviewThread`, `AddComment`,
//! `MarkFileAsViewed`, `UnmarkFileAsViewed`.

use std::sync::PoisonError;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{
    Actor, Connection, GitHub, IssueCommentNode, Oid, RepoEnvelope, ReviewCommentNode,
    ReviewStateWire, SubjectType, THREADS_PAGE, ThreadNode, comment, map_threads, range_start,
    review_state, side, user, variables,
};
use crate::error::{Error, Result};
use crate::http::{HttpRequest, Url};
use crate::model::{
    Comment, CommentAnchor, CommentId, JobId, LineRef, NewComment, PendingComment, PendingPlace,
    PendingReview, PrNumber, PullRequestId, RepoRef, Review, ReviewId, RunId, Sha, Side, Thread,
    ThreadId, Timestamp, Verdict, ViewedFile, ViewedState,
};

macro_rules! review_fields {
    () => {
        concat!(
            "id state body submittedAt url commit{oid} author{",
            actor!(),
            "}"
        )
    };
}

macro_rules! review_comment_fields {
    () => {
        concat!(
            "id author{",
            actor!(),
            "} body createdAt lastEditedAt url originalCommit{oid} commit{oid} \
             pullRequestReview{id state}"
        )
    };
}

const PULL_REQUEST_ID: &str = on_pull_request!("PullRequestId", "id");

const PENDING_REVIEW: &str = on_pull_request!(
    "PendingReview",
    concat!(
        "reviews(states:[PENDING],first:1){nodes{",
        review_fields!(),
        "}} reviewThreads(first:50){",
        page_info!(),
        " nodes{",
        thread_fields!(),
        "}}"
    )
);

const ADD_REVIEW: &str = concat!(
    "mutation AddPullRequestReview($pullRequestId:ID!,$commitOID:GitObjectID!){\
     addPullRequestReview(input:{pullRequestId:$pullRequestId,commitOID:$commitOID}){\
     pullRequestReview{",
    review_fields!(),
    "}}}"
);

const ADD_THREAD: &str = concat!(
    "mutation AddPullRequestReviewThread($pullRequestReviewId:ID!,$path:String!,$body:String!,\
     $line:Int,$side:DiffSide,$startLine:Int,$startSide:DiffSide,\
     $subjectType:PullRequestReviewThreadSubjectType){\
     addPullRequestReviewThread(input:{pullRequestReviewId:$pullRequestReviewId,path:$path,\
     body:$body,line:$line,side:$side,startLine:$startLine,startSide:$startSide,\
     subjectType:$subjectType}){thread{",
    thread_fields!(),
    "}}}"
);

const ADD_REPLY: &str = concat!(
    "mutation AddPullRequestReviewThreadReply($pullRequestReviewThreadId:ID!,\
     $pullRequestReviewId:ID,$body:String!){\
     addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$pullRequestReviewThreadId,\
     pullRequestReviewId:$pullRequestReviewId,body:$body}){comment{",
    review_comment_fields!(),
    "}}}"
);

const DELETE_COMMENT: &str = "mutation DeletePullRequestReviewComment($id:ID!){\
     deletePullRequestReviewComment(input:{id:$id}){pullRequestReview{id}}}";

const UPDATE_COMMENT: &str = concat!(
    "mutation UpdatePullRequestReviewComment($pullRequestReviewCommentId:ID!,$body:String!){     updatePullRequestReviewComment(input:{pullRequestReviewCommentId:$pullRequestReviewCommentId,     body:$body}){pullRequestReviewComment{",
    review_comment_fields!(),
    "}}}"
);

const SUBMIT_REVIEW: &str = concat!(
    "mutation SubmitPullRequestReview($pullRequestReviewId:ID!,$event:PullRequestReviewEvent!,\
     $body:String){submitPullRequestReview(input:{pullRequestReviewId:$pullRequestReviewId,\
     event:$event,body:$body}){pullRequestReview{",
    review_fields!(),
    "}}}"
);

const DELETE_REVIEW: &str = "mutation DeletePullRequestReview($pullRequestReviewId:ID!){\
     deletePullRequestReview(input:{pullRequestReviewId:$pullRequestReviewId}){\
     pullRequestReview{id}}}";

const RESOLVE: &str = concat!(
    "mutation ResolveReviewThread($threadId:ID!){resolveReviewThread(input:{threadId:$threadId}){\
     thread{",
    thread_fields!(),
    "}}}"
);

const UNRESOLVE: &str = concat!(
    "mutation UnresolveReviewThread($threadId:ID!){\
     unresolveReviewThread(input:{threadId:$threadId}){thread{",
    thread_fields!(),
    "}}}"
);

const ADD_COMMENT: &str = concat!(
    "mutation AddComment($subjectId:ID!,$body:String!){\
     addComment(input:{subjectId:$subjectId,body:$body}){commentEdge{node{id author{",
    actor!(),
    "} body createdAt lastEditedAt url}}}}"
);

const MARK_VIEWED: &str = "mutation MarkFileAsViewed($pullRequestId:ID!,$path:String!){\
     markFileAsViewed(input:{pullRequestId:$pullRequestId,path:$path}){pullRequest{id}}}";

const UNMARK_VIEWED: &str = "mutation UnmarkFileAsViewed($pullRequestId:ID!,$path:String!){\
     unmarkFileAsViewed(input:{pullRequestId:$pullRequestId,path:$path}){pullRequest{id}}}";

impl GitHub {
    /// Sends a mutation and decodes its payload, the field of `data` named
    /// after the mutation (`addComment` for `AddComment`).
    pub(super) async fn mutate<T: DeserializeOwned>(
        &self,
        operation: &str,
        field: &str,
        query: &str,
        variables: Value,
    ) -> Result<T> {
        let mut data: Value = self
            .http
            .graphql_mutation(self.graphql.clone(), operation, query, variables)
            .await?;
        let payload = data
            .get_mut(field)
            .map(Value::take)
            .filter(|payload| !payload.is_null())
            .ok_or_else(|| decode_error(operation, format!("no `{field}` payload")))?;
        T::deserialize(payload).map_err(|err| decode_error(operation, err.to_string()))
    }

    /// The pull request's node id, which every mutation on it takes. Asked
    /// once per pull request; node ids never change.
    pub(super) async fn pull_request_id(
        &self,
        repo: &RepoRef,
        number: PrNumber,
    ) -> Result<PullRequestId> {
        let key = (repo.clone(), number);
        if let Some(id) = self.lock_node_ids().get(&key) {
            return Ok(id.clone());
        }
        let envelope: RepoEnvelope<IdNode> = self
            .query("PullRequestId", PULL_REQUEST_ID, variables(repo, number))
            .await?;
        let id = PullRequestId::new(envelope.into_pull_request(repo, number)?.id);
        self.lock_node_ids().insert(key, id.clone());
        Ok(id)
    }

    fn lock_node_ids(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<(RepoRef, PrNumber), PullRequestId>>
    {
        self.node_ids.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `{rest}/repos/{owner}/{name}/{segments...}`, each segment escaped.
    pub(super) fn rest_url(&self, repo: &RepoRef, segments: &[&str]) -> Result<Url> {
        let mut url = self.rest.clone();
        url.path_segments_mut()
            .map_err(|()| Error::Config {
                host: self.http.host().to_owned(),
                reason: "REST URL cannot take a path".to_owned(),
            })?
            .pop_if_empty()
            .extend(["repos", repo.owner.as_str(), repo.name.as_str()])
            .extend(segments);
        Ok(url)
    }

    async fn rest_post(&self, url: Url) -> Result<()> {
        self.http
            .send(HttpRequest::post_json(url, json!({})))
            .await
            .map(drop)
    }

    async fn thread(&self, mut node: ThreadNode, operation: &str) -> Result<Thread> {
        self.complete_thread(&mut node).await?;
        map_threads(Vec::new(), Vec::new(), vec![node])
            .pop()
            .ok_or_else(|| decode_error(operation, "the thread has no submitted comment".into()))
    }
}

pub(super) async fn start_review(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    commit: &Sha,
) -> Result<PendingReview> {
    let id = github.pull_request_id(repo, number).await?;
    let operation = "AddPullRequestReview";
    let payload: ReviewPayload = github
        .mutate(
            operation,
            "addPullRequestReview",
            ADD_REVIEW,
            json!({"pullRequestId": id, "commitOID": commit}),
        )
        .await?;
    Ok(PendingReview {
        review: payload.review(operation)?,
        comments: Vec::new(),
    })
}

pub(super) async fn pending_review(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Option<PendingReview>> {
    let envelope: RepoEnvelope<PendingSections> = github
        .query("PendingReview", PENDING_REVIEW, variables(repo, number))
        .await?;
    let sections = envelope.into_pull_request(repo, number)?;
    let Some(review) = sections.reviews.into_parts().0.into_iter().next() else {
        return Ok(None);
    };
    let mut threads = github
        .paginate_pull_request(
            Some(sections.review_threads),
            "ReviewThreadsPage",
            THREADS_PAGE,
            repo,
            number,
            |sections| sections.review_threads,
        )
        .await?;
    let mut comments = Vec::new();
    for thread in &mut threads {
        github.complete_thread(thread).await?;
        let thread_id = ThreadId::new(&thread.id);
        for (at, node) in thread.comment_nodes.iter().enumerate() {
            if node
                .pull_request_review
                .as_ref()
                .is_none_or(|r| r.id != review.id)
            {
                continue;
            }
            let place = if at == 0 {
                PendingPlace::New(comment_anchor(thread))
            } else {
                PendingPlace::Reply(thread_id.clone())
            };
            comments.push(pending_comment(node.clone(), place, thread_id.clone()));
        }
    }
    comments.sort_by_key(|c| c.created_at);
    Ok(Some(PendingReview {
        review: review_from(review),
        comments,
    }))
}

pub(super) async fn add_review_comment(
    github: &GitHub,
    review: &ReviewId,
    comment: &NewComment,
) -> Result<PendingComment> {
    match &comment.place {
        PendingPlace::New(anchor) => {
            let mut vars = json!({
                "pullRequestReviewId": review,
                "path": anchor.path(),
                "body": comment.body,
            });
            match anchor {
                CommentAnchor::Line { line, start, .. } => {
                    vars["subjectType"] = json!("LINE");
                    vars["line"] = json!(line.line);
                    vars["side"] = json!(diff_side(line.side));
                    if let Some(start) = start {
                        vars["startLine"] = json!(start.line);
                        vars["startSide"] = json!(diff_side(start.side));
                    }
                }
                CommentAnchor::File { .. } => vars["subjectType"] = json!("FILE"),
            }
            let operation = "AddPullRequestReviewThread";
            let payload: ThreadPayload = github
                .mutate(operation, "addPullRequestReviewThread", ADD_THREAD, vars)
                .await?;
            let thread = payload.thread(operation)?;
            let place = PendingPlace::New(comment_anchor(&thread));
            let thread_id = ThreadId::new(&thread.id);
            let first = thread
                .comments
                .into_parts()
                .0
                .into_iter()
                .next()
                .ok_or_else(|| decode_error(operation, "the new thread has no comment".into()))?;
            Ok(pending_comment(first, place, thread_id))
        }
        PendingPlace::Reply(thread) => {
            let node = add_reply(github, thread, Some(review), &comment.body).await?;
            Ok(pending_comment(
                node,
                PendingPlace::Reply(thread.clone()),
                thread.clone(),
            ))
        }
    }
}

pub(super) async fn delete_review_comment(github: &GitHub, comment: &CommentId) -> Result<()> {
    github
        .mutate::<Value>(
            "DeletePullRequestReviewComment",
            "deletePullRequestReviewComment",
            DELETE_COMMENT,
            json!({"id": comment}),
        )
        .await
        .map(drop)
}

pub(super) async fn edit_review_comment(
    github: &GitHub,
    comment: &PendingComment,
    body: &str,
) -> Result<PendingComment> {
    let operation = "UpdatePullRequestReviewComment";
    let payload: UpdatePayload = github
        .mutate(
            operation,
            "updatePullRequestReviewComment",
            UPDATE_COMMENT,
            json!({"pullRequestReviewCommentId": comment.id, "body": body}),
        )
        .await?;
    let node = payload
        .pull_request_review_comment
        .ok_or_else(|| decode_error(operation, "no comment in the payload".into()))?;
    Ok(PendingComment {
        id: CommentId::new(node.id),
        body: node.body,
        created_at: Some(node.created_at),
        ..comment.clone()
    })
}

pub(super) async fn submit_review(
    github: &GitHub,
    review: &ReviewId,
    verdict: Verdict,
    body: &str,
) -> Result<Review> {
    let event = match verdict {
        Verdict::Comment => "COMMENT",
        Verdict::Approve => "APPROVE",
        Verdict::RequestChanges => "REQUEST_CHANGES",
    };
    let operation = "SubmitPullRequestReview";
    let payload: ReviewPayload = github
        .mutate(
            operation,
            "submitPullRequestReview",
            SUBMIT_REVIEW,
            json!({"pullRequestReviewId": review, "event": event, "body": body}),
        )
        .await?;
    payload.review(operation)
}

pub(super) async fn discard_review(github: &GitHub, review: &ReviewId) -> Result<()> {
    github
        .mutate::<Value>(
            "DeletePullRequestReview",
            "deletePullRequestReview",
            DELETE_REVIEW,
            json!({"pullRequestReviewId": review}),
        )
        .await
        .map(drop)
}

pub(super) async fn reply(github: &GitHub, thread: &ThreadId, body: &str) -> Result<Comment> {
    add_reply(github, thread, None, body).await.map(comment)
}

pub(super) async fn set_resolved(
    github: &GitHub,
    thread: &ThreadId,
    resolved: bool,
) -> Result<Thread> {
    let (operation, field, query) = if resolved {
        ("ResolveReviewThread", "resolveReviewThread", RESOLVE)
    } else {
        ("UnresolveReviewThread", "unresolveReviewThread", UNRESOLVE)
    };
    let payload: ThreadPayload = github
        .mutate(operation, field, query, json!({"threadId": thread}))
        .await?;
    github.thread(payload.thread(operation)?, operation).await
}

pub(super) async fn add_comment(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    body: &str,
) -> Result<Comment> {
    let id = github.pull_request_id(repo, number).await?;
    let operation = "AddComment";
    let payload: AddCommentPayload = github
        .mutate(
            operation,
            "addComment",
            ADD_COMMENT,
            json!({"subjectId": id, "body": body}),
        )
        .await?;
    let node = payload
        .comment_edge
        .and_then(|edge| edge.node)
        .ok_or_else(|| decode_error(operation, "no comment in the payload".into()))?;
    Ok(Comment {
        id: CommentId::new(node.id),
        author: user(node.author),
        body: node.body,
        created_at: node.created_at,
        edited_at: node.last_edited_at,
        web_url: node.url,
        review: None,
        verdict: None,
        suggestions: Vec::new(),
    })
}

pub(super) async fn set_viewed(
    github: &GitHub,
    repo: &RepoRef,
    number: PrNumber,
    path: &str,
    viewed: bool,
) -> Result<ViewedFile> {
    let id = github.pull_request_id(repo, number).await?;
    let (operation, field, query, state) = if viewed {
        (
            "MarkFileAsViewed",
            "markFileAsViewed",
            MARK_VIEWED,
            ViewedState::Viewed,
        )
    } else {
        (
            "UnmarkFileAsViewed",
            "unmarkFileAsViewed",
            UNMARK_VIEWED,
            ViewedState::Unviewed,
        )
    };
    github
        .mutate::<Value>(
            operation,
            field,
            query,
            json!({"pullRequestId": id, "path": path}),
        )
        .await?;
    Ok(ViewedFile {
        path: path.to_owned(),
        state,
    })
}

pub(super) async fn retry_job(github: &GitHub, repo: &RepoRef, job: &JobId) -> Result<()> {
    let url = github.rest_url(repo, &["actions", "jobs", job.as_str(), "rerun"])?;
    github.rest_post(url).await
}

pub(super) async fn retry_failed_jobs(github: &GitHub, repo: &RepoRef, run: &RunId) -> Result<()> {
    let url = github.rest_url(
        repo,
        &["actions", "runs", run.as_str(), "rerun-failed-jobs"],
    )?;
    github.rest_post(url).await
}

async fn add_reply(
    github: &GitHub,
    thread: &ThreadId,
    review: Option<&ReviewId>,
    body: &str,
) -> Result<ReviewCommentNode> {
    let mut vars = json!({"pullRequestReviewThreadId": thread, "body": body});
    if let Some(review) = review {
        vars["pullRequestReviewId"] = json!(review);
    }
    let operation = "AddPullRequestReviewThreadReply";
    let payload: ReplyPayload = github
        .mutate(
            operation,
            "addPullRequestReviewThreadReply",
            ADD_REPLY,
            vars,
        )
        .await?;
    payload
        .comment
        .ok_or_else(|| decode_error(operation, "no comment in the payload".into()))
}

fn decode_error(operation: &str, reason: String) -> Error {
    Error::Decode {
        request: operation.to_owned(),
        reason,
    }
}

fn diff_side(side: Side) -> &'static str {
    match side {
        Side::Old => "LEFT",
        Side::New => "RIGHT",
    }
}

/// The anchor a thread was opened at, as the composer would have sent it.
fn comment_anchor(thread: &ThreadNode) -> CommentAnchor {
    let path = thread.path.clone();
    let line = thread
        .line
        .or(thread.original_line)
        .filter(|_| thread.subject_type != Some(SubjectType::File));
    let Some(line) = line else {
        return CommentAnchor::File { path };
    };
    let line_side = side(thread.diff_side);
    let start_side = thread
        .start_diff_side
        .map_or(line_side, |start| side(Some(start)));
    let start_line = thread.start_line.or(thread.original_start_line);
    CommentAnchor::Line {
        path,
        line: LineRef {
            side: line_side,
            line,
        },
        start: range_start(start_line, start_side, line, line_side),
    }
}

fn pending_comment(
    node: ReviewCommentNode,
    place: PendingPlace,
    thread: ThreadId,
) -> PendingComment {
    PendingComment {
        id: CommentId::new(node.id),
        place,
        thread: Some(thread),
        body: node.body,
        created_at: Some(node.created_at),
    }
}

fn review_from(node: ReviewObject) -> Review {
    Review {
        id: ReviewId::new(node.id),
        author: user(node.author),
        state: review_state(node.state),
        body: node.body,
        commit: node.commit.map(|c| Sha::new(c.oid)),
        submitted_at: node.submitted_at,
        web_url: node.url,
    }
}

// ---- wire types -----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct IdNode {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewObject {
    id: String,
    state: ReviewStateWire,
    #[serde(default)]
    body: String,
    submitted_at: Option<Timestamp>,
    url: Option<String>,
    commit: Option<Oid>,
    author: Option<Actor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PendingSections {
    #[serde(default)]
    reviews: Connection<ReviewObject>,
    #[serde(default)]
    review_threads: Connection<ThreadNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReviewPayload {
    pull_request_review: Option<ReviewObject>,
}

impl ReviewPayload {
    fn review(self, operation: &str) -> Result<Review> {
        self.pull_request_review
            .map(review_from)
            .ok_or_else(|| decode_error(operation, "no review in the payload".into()))
    }
}

#[derive(Debug, Deserialize)]
struct ThreadPayload {
    thread: Option<ThreadNode>,
}

impl ThreadPayload {
    fn thread(self, operation: &str) -> Result<ThreadNode> {
        self.thread
            .ok_or_else(|| decode_error(operation, "no thread in the payload".into()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdatePayload {
    pull_request_review_comment: Option<ReviewCommentNode>,
}

#[derive(Debug, Deserialize)]
struct ReplyPayload {
    comment: Option<ReviewCommentNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddCommentPayload {
    comment_edge: Option<CommentEdge>,
}

#[derive(Debug, Deserialize)]
struct CommentEdge {
    node: Option<IssueCommentNode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mutations_are_named_after_their_operation() {
        for (operation, query) in [
            ("AddPullRequestReview", ADD_REVIEW),
            ("AddPullRequestReviewThread", ADD_THREAD),
            ("AddPullRequestReviewThreadReply", ADD_REPLY),
            ("DeletePullRequestReviewComment", DELETE_COMMENT),
            ("UpdatePullRequestReviewComment", UPDATE_COMMENT),
            ("SubmitPullRequestReview", SUBMIT_REVIEW),
            ("DeletePullRequestReview", DELETE_REVIEW),
            ("ResolveReviewThread", RESOLVE),
            ("UnresolveReviewThread", UNRESOLVE),
            ("AddComment", ADD_COMMENT),
            ("MarkFileAsViewed", MARK_VIEWED),
            ("UnmarkFileAsViewed", UNMARK_VIEWED),
        ] {
            assert!(
                query.starts_with(&format!("mutation {operation}(")),
                "{operation}: {query}"
            );
        }
        assert!(PULL_REQUEST_ID.starts_with("query PullRequestId("));
        assert!(PENDING_REVIEW.starts_with("query PendingReview("));
    }
}
