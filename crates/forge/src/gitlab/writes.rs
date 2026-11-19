//! GitLab writes, all REST (GraphQL has no draft notes or approve) and all
//! sent with [`RetryMode::Never`]: a 5xx or a lost response is returned, not
//! resent.
//!
//! - There is no review object [L1]. A pending review is the viewer's draft
//!   notes on the merge request, so a [`ReviewId`] is made up from the merge
//!   request (`host/group/project!iid`), `start_review` sends nothing, and
//!   `submit_review` publishes every draft at once.
//! - Draft notes carry no timestamp, so `PendingComment::created_at` is
//!   `None` for all of them; their order is the order of their ids.
//! - Publishing silently leaves behind a draft whose position GitLab cannot
//!   place (a line that is unchanged needs both its old and new number, so
//!   `add_review_comment` looks the pair up in the file's diff). `submit_review`
//!   reads the drafts back and fails when any is left.
//! - The verdict: `bulk_publish` with `reviewer_state` `reviewed` (Comment) or
//!   `requested_changes`, whose summary note and system note
//!   `build_threads` pairs again on read. Approve is `POST
//!   .../approve` after a `bulk_publish` with no `reviewer_state` (`approved`
//!   is refused), so the summary is followed by the approval's system note.
//!   GitLab writes the verdict's system note from a background job 0.3 to 6 s
//!   after the request, so `submit_review` notes the newest note id before
//!   publishing and waits for the matching note above it.

use std::time::SystemTime;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use super::{
    GitLab, GqlHead, PAGE, RestNote, RestPosition, ThreadContext, comment_of, gid_number,
    position_lines, position_paths, project_segment, thread_of, with_page,
};
use crate::error::{Error, Result};
use crate::forge::Forge;
use crate::http::{HttpRequest, Method};
use crate::model::{
    Comment, CommentAnchor, CommentId, JobId, LineRef, NewComment, PendingComment, PendingPlace,
    PendingReview, PrNumber, RepoRef, Review, ReviewId, ReviewState, RunId, Sha, Side,
    SuggestionId, Thread, ThreadId, User, Verdict,
};

/// A merge request's `diffRefs`, what a position is made against.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRefs {
    diff_refs: Option<GqlRefsShas>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GqlRefsShas {
    base_sha: Option<String>,
    start_sha: String,
    head_sha: String,
}

/// One entry of `GET .../merge_requests/:iid/diffs`.
#[derive(Deserialize)]
struct RestFileDiff {
    old_path: String,
    new_path: String,
    #[serde(default)]
    diff: String,
}

/// How many looks a fresh merge request's diff refs get.
const DIFF_REFS_LOOKS: usize = 15;

/// Waits between looks at a fresh merge request's empty diff, in settle
/// intervals: 1 + 2 + 4 of them, about seven seconds at the default.
const EMPTY_DIFF_BACKOFF: [u32; 3] = [1, 2, 4];

/// How many looks a verdict takes for its system note: GitLab's background
/// job writes it 0.3 to 6 s after the request, so 20 looks at the default
/// one-second interval is more than three times the slowest seen.
const VERDICT_NOTE_LOOKS: usize = 20;

/// A draft note, as created and as listed.
#[derive(Deserialize)]
struct RestDraftNote {
    id: u64,
    note: String,
    discussion_id: Option<String>,
    position: Option<RestPosition>,
}

impl GitLab {
    fn merge_request_path(repo: &RepoRef, number: PrNumber, tail: &str) -> String {
        format!(
            "projects/{}/merge_requests/{number}/{tail}",
            project_segment(repo)
        )
    }

    fn web_url(&self, repo: &RepoRef, number: PrNumber) -> String {
        format!(
            "https://{}/{}/-/merge_requests/{number}",
            self.http.host(),
            repo.path()
        )
    }

    async fn viewer(&self) -> Result<User> {
        self.viewer.get_or_try_init(|| self.whoami()).await.cloned()
    }

    /// Sends a write and decodes the JSON it answers with.
    async fn write<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let request = self.write_request(method, path, body)?;
        let describe = request.describe();
        self.http.send(request).await?.json(&describe)
    }

    /// Sends a write whose answer is empty or not wanted.
    pub(super) async fn write_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<()> {
        let request = self.write_request(method, path, body)?;
        self.http.send(request).await.map(drop)
    }

    fn write_request(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<HttpRequest> {
        let url = self.rest_url(path, &[])?;
        Ok(HttpRequest {
            method,
            body,
            ..HttpRequest::post_json(url, Value::Null)
        })
    }

    async fn draft_notes(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<RestDraftNote>> {
        let path = Self::merge_request_path(repo, number, "draft_notes");
        let url = self.rest_url(&path, &[("per_page", &PAGE.to_string())])?;
        let mut drafts: Vec<RestDraftNote> = self.rest_pages(url, usize::MAX).await?;
        drafts.sort_by_key(|draft| draft.id);
        Ok(drafts)
    }

    /// The refs a position is made against. A merge request opened a moment
    /// ago has none until its diff is generated, so this looks again a few
    /// times.
    async fn diff_refs(&self, repo: &RepoRef, number: PrNumber) -> Result<GqlRefsShas> {
        let query = "query DiffRefs($path: ID!, $iid: String!) {
               project(fullPath: $path) { mergeRequest(iid: $iid) { diffRefs { baseSha startSha headSha } } }
             }";
        for look in 0..DIFF_REFS_LOOKS {
            if look > 0 {
                tokio::time::sleep(self.settle).await;
            }
            let refs: GqlRefs = self.merge_request("DiffRefs", query, repo, number).await?;
            if let Some(refs) = refs.diff_refs {
                return Ok(refs);
            }
        }
        Err(Error::Decode {
            request: "DiffRefs".to_owned(),
            reason: format!("merge request {repo}!{number} has no diff refs yet"),
        })
    }

    /// The diff of the file at `path` (its old or new path), reading pages
    /// only until it turns up. `None` when the merge request does not change
    /// it.
    ///
    /// Right after a merge request opens GitLab answers `/diffs` with no
    /// files at all until its diff is computed, and a line comment placed
    /// then is dropped on publish. An empty first page is looked at again
    /// with backoff, then reported as [`Error::NotReady`].
    async fn file_diff(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        path: &str,
    ) -> Result<Option<RestFileDiff>> {
        let tail = Self::merge_request_path(repo, number, "diffs");
        let first = self.rest_url(&tail, &[("per_page", &PAGE.to_string())])?;
        let mut backoff = EMPTY_DIFF_BACKOFF.iter();
        let mut url = first.clone();
        loop {
            let request = HttpRequest::get(url.clone());
            let describe = request.describe();
            let response = self.http.send(request).await?;
            let files: Vec<RestFileDiff> = response.json(&describe)?;
            if files.is_empty() && url == first {
                let Some(&wait) = backoff.next() else {
                    return Err(Error::NotReady {
                        host: self.http.host().to_owned(),
                        what: format!("the diff of merge request !{number}"),
                    });
                };
                tokio::time::sleep(self.settle * wait).await;
                continue;
            }
            if let Some(file) = files
                .into_iter()
                .find(|file| file.new_path == path || file.old_path == path)
            {
                return Ok(Some(file));
            }
            let next = match response
                .header("x-next-page")
                .map(str::trim)
                .filter(|page| !page.is_empty())
            {
                Some(page) => Some(with_page(&url, page)),
                None => response.next_page(),
            };
            let Some(next) = next else { return Ok(None) };
            url = next;
        }
    }

    /// The newest notes of the merge request, system ones included.
    async fn recent_notes(&self, repo: &RepoRef, number: PrNumber) -> Result<Vec<RestNote>> {
        let path = Self::merge_request_path(repo, number, "notes");
        let url = self.rest_url(
            &path,
            &[
                ("order_by", "created_at"),
                ("sort", "desc"),
                ("per_page", "20"),
            ],
        )?;
        let request = HttpRequest::get(url);
        let describe = request.describe();
        self.http.send(request).await?.json(&describe)
    }

    /// The highest note id there is now: a note with a higher id was written
    /// after this call.
    async fn newest_note_id(&self, repo: &RepoRef, number: PrNumber) -> Result<u64> {
        let notes = self.recent_notes(repo, number).await?;
        Ok(notes.iter().map(|note| note.id).max().unwrap_or(0))
    }

    /// A verdict's system note is added by a background job 0.3 to 6 s after
    /// the request, and it is what a read of the threads shows as the
    /// verdict. Waits (up to [`VERDICT_NOTE_LOOKS` looks) for `login`'s
    /// note `body` with an id above `after`, so a read that follows the
    /// write sees it. `Ok(false)` when it never came.
    async fn await_verdict_note(
        &self,
        repo: &RepoRef,
        number: PrNumber,
        login: &str,
        body: &str,
        after: u64,
    ) -> Result<bool> {
        for look in 0..VERDICT_NOTE_LOOKS {
            if look > 0 {
                tokio::time::sleep(self.settle).await;
            }
            let notes = self.recent_notes(repo, number).await?;
            let found = notes.iter().any(|note| {
                note.system
                    && note.id > after
                    && note.body == body
                    && note
                        .author
                        .as_ref()
                        .is_some_and(|user| user.username == login)
            });
            if found {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn thread_context<'a>(&'a self, head: &'a str, web_url: &'a str) -> ThreadContext<'a> {
        ThreadContext {
            host: self.http.host(),
            head,
            web_url,
        }
    }

    fn note_comment(&self, note: &RestNote, repo: &RepoRef, number: PrNumber) -> Comment {
        let web_url = self.web_url(repo, number);
        comment_of(note, &self.thread_context("", &web_url), None)
    }
}

/// `host/group/project!iid`: GitLab has no review id, so the merge request names it.
fn review_id(repo: &RepoRef, number: PrNumber) -> ReviewId {
    ReviewId::new(format!("{repo}!{number}"))
}

fn pending_review_of(
    author: User,
    web_url: String,
    commit: Option<Sha>,
    comments: Vec<PendingComment>,
    id: ReviewId,
) -> PendingReview {
    PendingReview {
        review: Review {
            id,
            author,
            state: ReviewState::Pending,
            body: String::new(),
            commit,
            submitted_at: None,
            web_url: Some(web_url),
        },
        comments,
    }
}

pub(super) async fn start_review(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    commit: &Sha,
) -> Result<PendingReview> {
    let author = gitlab.viewer().await?;
    Ok(pending_review_of(
        author,
        gitlab.web_url(repo, number),
        Some(commit.clone()),
        Vec::new(),
        review_id(repo, number),
    ))
}

pub(super) async fn pending_review(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<Option<PendingReview>> {
    let drafts = gitlab.draft_notes(repo, number).await?;
    if drafts.is_empty() {
        return Ok(None);
    }
    let commit = drafts
        .iter()
        .filter_map(|draft| draft.position.as_ref())
        .map(|position| position.head_sha.as_str())
        .find(|sha| !sha.is_empty())
        .map(Sha::new);
    // A draft with neither position nor discussion is a plain note from the
    // web UI; the model has no place for it, but it still goes out with the
    // review.
    let comments = drafts.iter().filter_map(pending_comment).collect();
    let author = gitlab.viewer().await?;
    Ok(Some(pending_review_of(
        author,
        gitlab.web_url(repo, number),
        commit,
        comments,
        review_id(repo, number),
    )))
}

pub(super) async fn add_review_comment(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    comment: &NewComment,
) -> Result<PendingComment> {
    let mut body = json!({"note": comment.body});
    match &comment.place {
        PendingPlace::Reply(thread) => body["in_reply_to_discussion_id"] = json!(thread),
        PendingPlace::New(anchor) => {
            let (refs, file) = tokio::try_join!(
                gitlab.diff_refs(repo, number),
                gitlab.file_diff(repo, number, anchor.path()),
            )?;
            body["position"] = position_json(&refs, anchor, file.as_ref());
        }
    }
    let path = GitLab::merge_request_path(repo, number, "draft_notes");
    let draft: RestDraftNote = gitlab.write(Method::Post, &path, Some(body)).await?;
    // The draft just made says what was asked, in the caller's own terms.
    Ok(PendingComment {
        id: CommentId::new(draft.id.to_string()),
        place: comment.place.clone(),
        thread: None,
        body: draft.note,
        created_at: None,
    })
}

pub(super) async fn delete_review_comment(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    comment: &CommentId,
) -> Result<()> {
    let path = GitLab::merge_request_path(repo, number, &format!("draft_notes/{comment}"));
    gitlab.write_empty(Method::Delete, &path, None).await
}

pub(super) async fn edit_review_comment(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    comment: &PendingComment,
    body: &str,
) -> Result<PendingComment> {
    let path = GitLab::merge_request_path(repo, number, &format!("draft_notes/{}", comment.id));
    let draft: RestDraftNote = gitlab
        .write(Method::Put, &path, Some(json!({"note": body})))
        .await?;
    Ok(PendingComment {
        id: CommentId::new(draft.id.to_string()),
        body: draft.note,
        ..comment.clone()
    })
}

pub(super) async fn submit_review(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    review: &ReviewId,
    verdict: Verdict,
    body: &str,
) -> Result<Review> {
    let notes_before = gitlab.newest_note_id(repo, number).await?;
    let mut publish = Map::new();
    if !body.is_empty() {
        publish.insert("note".to_owned(), json!(body));
    }
    match verdict {
        Verdict::Comment => publish.insert("reviewer_state".to_owned(), json!("reviewed")),
        Verdict::RequestChanges => {
            publish.insert("reviewer_state".to_owned(), json!("requested_changes"))
        }
        Verdict::Approve => None,
    };
    let path = GitLab::merge_request_path(repo, number, "draft_notes/bulk_publish");
    gitlab
        .write_empty(Method::Post, &path, Some(Value::Object(publish)))
        .await?;

    let left = gitlab.draft_notes(repo, number).await?;
    if !left.is_empty() {
        return Err(Error::Decode {
            request: format!("POST /{path}"),
            reason: format!(
                "{} draft notes were not published: GitLab could not place them in the diff",
                left.len()
            ),
        });
    }
    let author = gitlab.viewer().await?;
    if verdict == Verdict::Approve {
        let path = GitLab::merge_request_path(repo, number, "approve");
        gitlab
            .write_empty(Method::Post, &path, Some(json!({})))
            .await?;
    }
    let (note, required) = match verdict {
        Verdict::Approve => ("approved this merge request", true),
        Verdict::RequestChanges => ("requested changes", true),
        // Not every publish gets one, so a missing note is not an error.
        Verdict::Comment => ("left review comments", false),
    };
    let seen = gitlab
        .await_verdict_note(repo, number, &author.login, note, notes_before)
        .await?;
    if !seen && required {
        return Err(Error::NotReady {
            host: gitlab.http.host().to_owned(),
            what: format!(
                "the \"{note}\" note of the review just published on {repo}!{number} (the review stands)"
            ),
        });
    }
    Ok(Review {
        id: review.clone(),
        author,
        state: verdict.state(),
        body: body.to_owned(),
        commit: None,
        submitted_at: Some(DateTime::<Utc>::from(SystemTime::now())),
        web_url: Some(gitlab.web_url(repo, number)),
    })
}

pub(super) async fn discard_review(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
) -> Result<()> {
    for draft in gitlab.draft_notes(repo, number).await? {
        let path = GitLab::merge_request_path(repo, number, &format!("draft_notes/{}", draft.id));
        gitlab.write_empty(Method::Delete, &path, None).await?;
    }
    Ok(())
}

pub(super) async fn reply(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    thread: &ThreadId,
    body: &str,
) -> Result<Comment> {
    let path = GitLab::merge_request_path(repo, number, &format!("discussions/{thread}/notes"));
    let note: RestNote = gitlab
        .write(Method::Post, &path, Some(json!({"body": body})))
        .await?;
    Ok(gitlab.note_comment(&note, repo, number))
}

pub(super) async fn set_resolved(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    thread: &ThreadId,
    resolved: bool,
) -> Result<Thread> {
    let path = GitLab::merge_request_path(repo, number, &format!("discussions/{thread}"));
    let discussion = gitlab
        .write(Method::Put, &path, Some(json!({"resolved": resolved})))
        .await?;
    let query = "query ThreadsHead($path: ID!, $iid: String!) {
           project(fullPath: $path) { mergeRequest(iid: $iid) { webUrl diffHeadSha diffRefs { headSha } } }
         }";
    let head: GqlHead = gitlab
        .merge_request("ThreadsHead", query, repo, number)
        .await?;
    let context = gitlab.thread_context(head.head_sha(), &head.web_url);
    thread_of(&discussion, &context).ok_or_else(|| Error::Decode {
        request: format!("PUT /{path}"),
        reason: "the discussion has no note".to_owned(),
    })
}

pub(super) async fn add_comment(
    gitlab: &GitLab,
    repo: &RepoRef,
    number: PrNumber,
    body: &str,
) -> Result<Comment> {
    let path = GitLab::merge_request_path(repo, number, "notes");
    let note: RestNote = gitlab
        .write(Method::Post, &path, Some(json!({"body": body})))
        .await?;
    Ok(gitlab.note_comment(&note, repo, number))
}

pub(super) async fn apply_suggestions(gitlab: &GitLab, suggestions: &[SuggestionId]) -> Result<()> {
    let ids = suggestions
        .iter()
        .map(|id| {
            gid_number(id.as_str())
                .parse::<u64>()
                .map_err(|err| Error::Decode {
                    request: "apply suggestions".to_owned(),
                    reason: format!("suggestion id {id}: {err}"),
                })
        })
        .collect::<Result<Vec<u64>>>()?;
    match ids.as_slice() {
        [] => Ok(()),
        [id] => {
            gitlab
                .write_empty(Method::Put, &format!("suggestions/{id}/apply"), None)
                .await
        }
        _ => {
            gitlab
                .write_empty(
                    Method::Put,
                    "suggestions/batch_apply",
                    Some(json!({"ids": ids})),
                )
                .await
        }
    }
}

pub(super) async fn retry_job(gitlab: &GitLab, repo: &RepoRef, job: &JobId) -> Result<()> {
    let path = format!("projects/{}/jobs/{job}/retry", project_segment(repo));
    gitlab
        .write_empty(Method::Post, &path, Some(json!({})))
        .await
}

pub(super) async fn retry_failed_jobs(gitlab: &GitLab, repo: &RepoRef, run: &RunId) -> Result<()> {
    let path = format!("projects/{}/pipelines/{run}/retry", project_segment(repo));
    gitlab
        .write_empty(Method::Post, &path, Some(json!({})))
        .await
}

// ---- positions --------------------------------------------------------------

/// A draft note's `position` for `anchor`. A file comment is
/// `position_type: file`; a line comment names the unchanged line's numbers
/// on both sides, which GitLab needs to place it.
fn position_json(refs: &GqlRefsShas, anchor: &CommentAnchor, file: Option<&RestFileDiff>) -> Value {
    let path = anchor.path();
    let (old_path, new_path) = file.map_or((path, path), |file| {
        (file.old_path.as_str(), file.new_path.as_str())
    });
    let mut position = json!({
        "base_sha": refs.base_sha.as_deref().unwrap_or(&refs.start_sha),
        "start_sha": refs.start_sha,
        "head_sha": refs.head_sha,
        "old_path": old_path,
        "new_path": new_path,
    });
    let CommentAnchor::Line { line, start, .. } = anchor else {
        position["position_type"] = json!("file");
        return position;
    };
    position["position_type"] = json!("text");
    let diff = file.map(|file| file.diff.as_str());
    let end = line_json(diff, *line);
    if let Some(start) = start {
        position["line_range"] = json!({"start": line_json(diff, *start), "end": end});
    }
    if let Value::Object(numbers) = end
        && let Value::Object(map) = &mut position
    {
        map.extend(numbers.into_iter().filter(|(key, _)| key != "type"));
    }
    position
}

/// `{type, old_line?, new_line?}` for one line.
fn line_json(diff: Option<&str>, line: LineRef) -> Value {
    let (old, new) = diff
        .and_then(|diff| line_numbers(diff, line))
        .unwrap_or(match line.side {
            Side::Old => (Some(line.line), None),
            Side::New => (None, Some(line.line)),
        });
    let mut numbers = Map::new();
    numbers.insert(
        "type".to_owned(),
        json!(match line.side {
            Side::Old => "old",
            Side::New => "new",
        }),
    );
    if let Some(old) = old {
        numbers.insert("old_line".to_owned(), json!(old));
    }
    if let Some(new) = new {
        numbers.insert("new_line".to_owned(), json!(new));
    }
    Value::Object(numbers)
}

/// The (old, new) line numbers of the diff line at `wanted`: both for an
/// unchanged line, one for an added or removed line. `None` when the hunks do
/// not show it.
fn line_numbers(diff: &str, wanted: LineRef) -> Option<(Option<u32>, Option<u32>)> {
    let (mut old, mut new) = (0_u32, 0_u32);
    for text in diff.lines() {
        if let Some(header) = text.strip_prefix("@@ ") {
            let mut ranges = header.split_whitespace();
            old = hunk_start(ranges.next()?, '-')?;
            new = hunk_start(ranges.next()?, '+')?;
            continue;
        }
        let pair = match text.as_bytes().first() {
            Some(b'\\') => continue,
            Some(b'-') => {
                old += 1;
                (Some(old - 1), None)
            }
            Some(b'+') => {
                new += 1;
                (None, Some(new - 1))
            }
            _ => {
                old += 1;
                new += 1;
                (Some(old - 1), Some(new - 1))
            }
        };
        let here = match wanted.side {
            Side::Old => pair.0,
            Side::New => pair.1,
        };
        if here == Some(wanted.line) {
            return Some(pair);
        }
    }
    None
}

/// `-12,5` -> 12.
fn hunk_start(range: &str, sign: char) -> Option<u32> {
    range.strip_prefix(sign)?.split(',').next()?.parse().ok()
}

/// The queued comment a draft note is, or `None` for a plain note.
fn pending_comment(draft: &RestDraftNote) -> Option<PendingComment> {
    let place = match (&draft.discussion_id, &draft.position) {
        (Some(thread), _) => PendingPlace::Reply(ThreadId::new(thread)),
        (None, Some(position)) => PendingPlace::New(comment_anchor(position)?),
        (None, None) => return None,
    };
    Some(PendingComment {
        id: CommentId::new(draft.id.to_string()),
        place,
        thread: None,
        body: draft.note.clone(),
        created_at: None,
    })
}

/// The anchor as the composer would have sent it. An unchanged line, which
/// carries both numbers, reads as its new side.
fn comment_anchor(position: &RestPosition) -> Option<CommentAnchor> {
    let is_file = position.position_type.as_deref() == Some("file");
    let Some((line, start)) = position_lines(position).filter(|_| !is_file) else {
        let (path, _) = position_paths(position, Side::New)?;
        return Some(CommentAnchor::File { path });
    };
    let (path, _) = position_paths(position, line.side)?;
    Some(CommentAnchor::Line { path, line, start })
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIFF: &str = "@@ -1,7 +1,10 @@\n # Intro\n \n This page covers intro.\n+Round trip line one.\n+Round trip line two.\n+Round trip line three.\n \n - first point\n - second point\n-- third point\n+- third point\n\\ No newline at end of file\n";

    fn line(side: Side, line: u32) -> LineRef {
        LineRef { side, line }
    }

    #[test]
    fn line_numbers_pair_unchanged_lines_and_single_changed_ones() {
        let cases = [
            (line(Side::New, 1), Some((Some(1), Some(1)))),
            (line(Side::Old, 3), Some((Some(3), Some(3)))),
            (line(Side::New, 4), Some((None, Some(4)))),
            (line(Side::New, 7), Some((Some(4), Some(7)))),
            (line(Side::Old, 7), Some((Some(7), None))),
            (line(Side::New, 10), Some((None, Some(10)))),
            (line(Side::New, 40), None),
        ];
        for (wanted, expected) in cases {
            assert_eq!(line_numbers(DIFF, wanted), expected, "{wanted:?}");
        }
        assert_eq!(line_numbers("", line(Side::New, 1)), None);
    }

    fn refs() -> GqlRefsShas {
        GqlRefsShas {
            base_sha: Some("base".into()),
            start_sha: "start".into(),
            head_sha: "head".into(),
        }
    }

    fn file() -> RestFileDiff {
        RestFileDiff {
            old_path: "docs/intro.md".into(),
            new_path: "docs/intro.md".into(),
            diff: DIFF.into(),
        }
    }

    fn on(anchor_line: LineRef, start: Option<LineRef>) -> CommentAnchor {
        CommentAnchor::Line {
            path: "docs/intro.md".into(),
            line: anchor_line,
            start,
        }
    }

    #[test]
    fn positions_name_both_numbers_of_an_unchanged_line() {
        let position = position_json(&refs(), &on(line(Side::New, 2), None), Some(&file()));
        assert_eq!(
            position,
            json!({
                "base_sha": "base", "start_sha": "start", "head_sha": "head",
                "old_path": "docs/intro.md", "new_path": "docs/intro.md",
                "position_type": "text", "old_line": 2, "new_line": 2,
            })
        );
    }

    #[test]
    fn positions_for_added_removed_and_ranged_lines() {
        let added = position_json(&refs(), &on(line(Side::New, 4), None), Some(&file()));
        assert_eq!(
            (added["old_line"].as_u64(), added["new_line"].as_u64()),
            (None, Some(4))
        );
        let removed = position_json(&refs(), &on(line(Side::Old, 7), None), Some(&file()));
        assert_eq!(
            (removed["old_line"].as_u64(), removed["new_line"].as_u64()),
            (Some(7), None)
        );
        let range = position_json(
            &refs(),
            &on(line(Side::New, 6), Some(line(Side::New, 4))),
            Some(&file()),
        );
        assert_eq!(range["new_line"], 6);
        assert_eq!(
            range["line_range"],
            json!({"start": {"type": "new", "new_line": 4}, "end": {"type": "new", "new_line": 6}})
        );
        let unknown = position_json(&refs(), &on(line(Side::New, 4), None), None);
        assert_eq!(unknown["new_line"], 4);
        assert_eq!(unknown["old_path"], "docs/intro.md");
    }

    #[test]
    fn a_file_comment_is_a_file_position() {
        let anchor = CommentAnchor::File {
            path: "docs/intro.md".into(),
        };
        let position = position_json(&refs(), &anchor, Some(&file()));
        assert_eq!(position["position_type"], "file");
        assert!(position.get("new_line").is_none());
    }

    fn draft(value: Value) -> RestDraftNote {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn drafts_read_back_as_the_anchors_they_were_made_with() {
        let single = draft(json!({
            "id": 1, "note": "a", "discussion_id": null,
            "position": {"head_sha": "h", "old_path": "a.md", "new_path": "a.md",
                         "position_type": "text", "new_line": 4, "old_line": null, "line_range": null},
        }));
        assert_eq!(
            pending_comment(&single).unwrap().place,
            PendingPlace::New(CommentAnchor::Line {
                path: "a.md".into(),
                line: line(Side::New, 4),
                start: None,
            })
        );
        let ranged = draft(json!({
            "id": 2, "note": "b",
            "position": {"head_sha": "h", "old_path": "a.md", "new_path": "a.md",
                         "position_type": "text", "new_line": 6,
                         "line_range": {"start": {"type": "new", "new_line": 4}, "end": {"type": "new", "new_line": 6}}},
        }));
        let PendingPlace::New(CommentAnchor::Line { start, .. }) =
            pending_comment(&ranged).unwrap().place
        else {
            panic!("a line comment");
        };
        assert_eq!(start, Some(line(Side::New, 4)));
        let file = draft(json!({
            "id": 3, "note": "c",
            "position": {"head_sha": "h", "old_path": "a.md", "new_path": "a.md", "position_type": "file"},
        }));
        assert_eq!(
            pending_comment(&file).unwrap().place,
            PendingPlace::New(CommentAnchor::File {
                path: "a.md".into()
            })
        );
        let reply = draft(json!({
            "id": 4, "note": "d", "discussion_id": "abc",
            "position": {"head_sha": null, "old_path": null, "new_path": null,
                         "position_type": "text", "new_line": null, "old_line": null, "line_range": null},
        }));
        assert_eq!(
            pending_comment(&reply).unwrap().place,
            PendingPlace::Reply(ThreadId::new("abc"))
        );
        let plain = draft(json!({"id": 5, "note": "e", "discussion_id": null, "position": null}));
        assert!(pending_comment(&plain).is_none());
    }

    #[test]
    fn a_review_is_named_after_its_merge_request() {
        let repo = RepoRef::new("gitlab.com", "g/sub", "r");
        assert_eq!(
            review_id(&repo, PrNumber(9)).as_str(),
            "gitlab.com/g/sub/r!9"
        );
    }
}
