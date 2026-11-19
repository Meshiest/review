//! The forge side of the live round trip: every write on
//! a scratch pull request, each read back through the same backend. Shared
//! by `examples/round_trip` (live) and the GitHub writes replay test, so the
//! replay sends exactly the requests that were recorded.
//!
//! The scratch change replaces line [`REPLACED`] of [`TARGET`] with the
//! three [`ADDED`] lines, so the diff has one removed line (old side) and
//! three added ones (new side).

use std::fmt;

use forge::model::{
    CommentAnchor, LineRef, NewComment, PendingPlace, PrNumber, RepoRef, ReviewState, Side, Thread,
    ThreadId, Verdict, ViewedState,
};
use forge::{Capabilities, Forge, ReplyMode, ResolveMode, ViewedSync};

/// The file the scratch change edits; it exists on every sandbox's `main`.
pub const TARGET: &str = "docs/intro.md";
/// The old-side line the change removes.
pub const REPLACED: u32 = 3;
/// What replaces it, new-side lines `REPLACED..REPLACED + 3`.
pub const ADDED: [&str; 3] = [
    "Round trip line one.",
    "Round trip line two.",
    "Round trip line three.",
];

const DRAFT_BODY: &str = "Round trip: a draft, deleted before submitting.";
const KEPT_DRAFT_BODY: &str = "Round trip: a draft, discarded with its review.";
const SINGLE_BODY: &str = "Round trip: a single line.";
const MULTI_BODY: &str = "Round trip: three lines.";
const REMOVED_BODY: &str = "Round trip: a removed line.";
const REVIEW_BODY: &str = "Round trip review.";
const REPLY_BODY: &str = "Round trip reply.";
const SUGGESTION_BODY: &str =
    "Round trip suggestion:\n\n```suggestion\nRound trip line two, suggested.\n```";
const CONVERSATION_BODY: &str = "Round trip conversation comment.";
const APPROVE_BODY: &str = "Round trip approval.";
const REQUEST_CHANGES_BODY: &str = "Round trip: changes requested.";

fn new_line(line: u32) -> LineRef {
    LineRef {
        side: Side::New,
        line,
    }
}

fn line_anchor(line: LineRef, start: Option<LineRef>) -> CommentAnchor {
    CommentAnchor::Line {
        path: TARGET.to_owned(),
        line,
        start,
    }
}

fn single() -> CommentAnchor {
    line_anchor(new_line(REPLACED), None)
}

fn multi() -> CommentAnchor {
    line_anchor(new_line(REPLACED + 2), Some(new_line(REPLACED)))
}

fn removed() -> CommentAnchor {
    line_anchor(
        LineRef {
            side: Side::Old,
            line: REPLACED,
        },
        None,
    )
}

fn suggestion() -> CommentAnchor {
    line_anchor(new_line(REPLACED + 1), None)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Pass(String),
    Fail(String),
    /// The capability the forge lacks.
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
            Status::Skip(capability) => write!(f, "SKIP {}: {capability}", self.step),
        }
    }
}

type Step = Result<String, String>;

/// Fails the step with a message unless `ok`.
fn ensure(ok: bool, message: impl FnOnce() -> String) -> Result<(), String> {
    if ok { Ok(()) } else { Err(message()) }
}

fn err(error: forge::Error) -> String {
    error.to_string()
}

/// Runs every step against `number` in `repo`, calling `report` after each,
/// and returns the outcomes in order.
pub async fn run(
    forge: &dyn Forge,
    repo: &RepoRef,
    number: PrNumber,
    mut report: impl FnMut(&Outcome),
) -> Vec<Outcome> {
    let caps = forge.capabilities();
    let trip = Trip {
        forge,
        repo,
        number,
        caps,
    };
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

    if !caps.queued_review {
        for step in ["pending", "review", "suggestion"] {
            push(step, Status::Skip("queued_review".to_owned()));
        }
    } else {
        push("pending", status(trip.pending().await));
        if !caps.multi_line_comments {
            push(
                "review/multi-line",
                Status::Skip("multi_line_comments".to_owned()),
            );
        }
    }
    let reviewed = if caps.queued_review {
        let result = trip.review().await;
        let thread = result.as_ref().ok().map(|(thread, _)| thread.clone());
        push("review", status(result.map(|(_, detail)| detail)));
        thread
    } else {
        None
    };

    let needs_review = || "needs the thread from step `review`".to_owned();
    match &reviewed {
        Some(thread) => push("reply", status(trip.reply(thread).await)),
        None => push("reply", Status::Fail(needs_review())),
    }
    match (&reviewed, caps.resolve) {
        (_, ResolveMode::ReadOnly) => push("resolve", Status::Skip("resolve: read-only".into())),
        (Some(thread), ResolveMode::ReadWrite) => {
            push("resolve", status(trip.resolve(thread).await));
        }
        (None, ResolveMode::ReadWrite) => push("resolve", Status::Fail(needs_review())),
    }
    if caps.queued_review {
        push("suggestion", status(trip.suggestion().await));
    }
    push("conversation", status(trip.conversation().await));
    match caps.viewed {
        ViewedSync::Forge => push("viewed", status(trip.viewed().await)),
        ViewedSync::Local => push("viewed", Status::Skip("viewed: local only".into())),
    }
    for (step, verdict_kind) in [
        ("approve", Verdict::Approve),
        ("request-changes", Verdict::RequestChanges),
    ] {
        if !caps.queued_review {
            push(step, Status::Skip("queued_review".into()));
        } else if !caps.own_pull_request_verdicts {
            push(step, Status::Skip("own_pull_request_verdicts".into()));
        } else if verdict_kind == Verdict::RequestChanges && !caps.request_changes {
            push(step, Status::Skip("request_changes".into()));
        } else {
            push(step, status(trip.verdict(verdict_kind).await));
        }
    }
    outcomes
}

struct Trip<'a> {
    forge: &'a dyn Forge,
    repo: &'a RepoRef,
    number: PrNumber,
    caps: Capabilities,
}

impl Trip<'_> {
    async fn threads(&self) -> Result<Vec<Thread>, String> {
        self.forge
            .threads(self.repo, self.number)
            .await
            .map_err(err)
    }

    async fn head(&self) -> Result<forge::model::Sha, String> {
        let pr = self
            .forge
            .pull_request(self.repo, self.number)
            .await
            .map_err(err)?;
        Ok(pr.head.sha)
    }

    /// Start, queue two comments (a file comment where supported), list
    /// them, delete one, discard the review. One stays queued until the
    /// discard: GitHub drops a pending review whose last comment is deleted.
    async fn pending(&self) -> Step {
        let (forge, repo, n) = (self.forge, self.repo, self.number);
        let head = self.head().await?;
        let started = forge.start_review(repo, n, &head).await.map_err(err)?;
        ensure(
            started.review.state == ReviewState::Pending && started.comments.is_empty(),
            || format!("start_review returned {started:?}"),
        )?;
        let review = started.review.id;
        let second = if self.caps.file_comments {
            CommentAnchor::File {
                path: TARGET.to_owned(),
            }
        } else {
            removed()
        };
        let mut queued = Vec::new();
        for (anchor, body) in [(single(), DRAFT_BODY), (second, KEPT_DRAFT_BODY)] {
            let draft = NewComment {
                place: PendingPlace::New(anchor),
                body: body.to_owned(),
            };
            let comment = forge
                .add_review_comment(repo, n, &review, &draft)
                .await
                .map_err(err)?;
            ensure(comment.place == draft.place && comment.body == body, || {
                format!("add_review_comment returned {comment:?}")
            })?;
            queued.push(comment);
        }
        let listed = forge.pending_review(repo, n).await.map_err(err)?;
        ensure(
            listed
                .as_ref()
                .is_some_and(|p| p.review.id == review && p.comments == queued),
            || format!("pending_review after queueing two: {listed:?}"),
        )?;
        forge
            .delete_review_comment(repo, n, &queued[0].id)
            .await
            .map_err(err)?;
        let listed = forge.pending_review(repo, n).await.map_err(err)?;
        ensure(
            listed.as_ref().is_some_and(|p| p.comments == queued[1..]),
            || format!("pending_review after deleting one: {listed:?}"),
        )?;
        forge.discard_review(repo, n, &review).await.map_err(err)?;
        let listed = forge.pending_review(repo, n).await.map_err(err)?;
        ensure(listed.is_none(), || {
            format!("pending_review after discard: {listed:?}")
        })?;
        let kind = if self.caps.file_comments {
            "line and file"
        } else {
            "two line"
        };
        Ok(format!(
            "started, queued {kind} comments, listed 2, deleted 1, discarded"
        ))
    }

    /// Three anchored comments submitted as COMMENT; returns the
    /// single-line thread for the next steps.
    async fn review(&self) -> Result<(ThreadId, String), String> {
        let (forge, repo, n) = (self.forge, self.repo, self.number);
        let head = self.head().await?;
        let review = forge
            .start_review(repo, n, &head)
            .await
            .map_err(err)?
            .review
            .id;
        let mut wanted = vec![(single(), SINGLE_BODY), (removed(), REMOVED_BODY)];
        if self.caps.multi_line_comments {
            wanted.insert(1, (multi(), MULTI_BODY));
        }
        for (anchor, body) in &wanted {
            let comment = NewComment {
                place: PendingPlace::New(anchor.clone()),
                body: (*body).to_owned(),
            };
            let queued = forge
                .add_review_comment(repo, n, &review, &comment)
                .await
                .map_err(err)?;
            ensure(
                queued.place == comment.place && queued.body == *body,
                || format!("add_review_comment returned {queued:?} for {anchor:?}"),
            )?;
        }
        let pending = forge.pending_review(repo, n).await.map_err(err)?;
        let queued = pending.as_ref().map_or(0, |p| p.comments.len());
        ensure(queued == wanted.len(), || {
            format!(
                "pending_review lists {queued} comments, expected {}",
                wanted.len()
            )
        })?;
        let submitted = forge
            .submit_review(repo, n, &review, Verdict::Comment, REVIEW_BODY)
            .await
            .map_err(err)?;
        ensure(
            submitted.state == ReviewState::Commented
                && submitted.body == REVIEW_BODY
                && submitted.submitted_at.is_some(),
            || format!("submit_review returned {submitted:?}"),
        )?;

        let threads = self.threads().await?;
        let summaries = threads
            .iter()
            .filter(|t| t.anchor.is_none())
            .filter(|t| t.comments[0].verdict == Some(ReviewState::Commented))
            .filter(|t| t.comments[0].body == REVIEW_BODY)
            .count();
        ensure(summaries == 1, || {
            format!("expected 1 COMMENT review summary, read {summaries}")
        })?;
        let anchored = threads.iter().filter(|t| t.anchor.is_some()).count();
        ensure(anchored == wanted.len(), || {
            format!(
                "expected {} anchored threads, read {anchored}",
                wanted.len()
            )
        })?;
        let mut single_thread = None;
        for (anchor, body) in &wanted {
            let thread = find_thread(&threads, anchor, body)?;
            ensure(thread.comments.len() == 1, || {
                format!(
                    "thread at {anchor:?} has {} comments",
                    thread.comments.len()
                )
            })?;
            if *anchor == single() {
                single_thread = Some(thread.id.clone());
            }
        }
        let thread = single_thread.ok_or("no single-line thread")?;
        Ok((
            thread,
            format!(
                "1 review, {} anchored threads at the right lines and sides",
                wanted.len()
            ),
        ))
    }

    async fn reply(&self, thread: &ThreadId) -> Step {
        let replied = self
            .forge
            .reply(self.repo, self.number, thread, REPLY_BODY)
            .await
            .map_err(err)?;
        ensure(replied.body == REPLY_BODY, || {
            format!("reply returned {replied:?}")
        })?;
        let threads = self.threads().await?;
        let found = find_thread(&threads, &single(), SINGLE_BODY)?;
        let bodies: Vec<&str> = found.comments.iter().map(|c| c.body.as_str()).collect();
        ensure(bodies == [SINGLE_BODY, REPLY_BODY], || {
            format!("thread reads {bodies:?}")
        })?;
        let mode = match self.caps.reply {
            ReplyMode::Native => "native",
            ReplyMode::SamePathAndLine => "same path and line",
        };
        Ok(format!("2 comments in the thread ({mode})"))
    }

    async fn resolve(&self, thread: &ThreadId) -> Step {
        for resolved in [true, false] {
            let returned = self
                .forge
                .set_resolved(self.repo, self.number, thread, resolved)
                .await
                .map_err(err)?;
            ensure(
                returned.is_resolved() == resolved && returned.id == *thread,
                || format!("set_resolved({resolved}) returned {returned:?}"),
            )?;
            let threads = self.threads().await?;
            let read = threads
                .iter()
                .find(|t| t.id == *thread)
                .ok_or("the thread is gone")?;
            ensure(read.is_resolved() == resolved, || {
                format!(
                    "after set_resolved({resolved}) it reads {:?}",
                    read.resolution
                )
            })?;
        }
        Ok("resolved, then unresolved, read back each".to_owned())
    }

    async fn suggestion(&self) -> Step {
        let (forge, repo, n) = (self.forge, self.repo, self.number);
        let head = self.head().await?;
        let review = forge
            .start_review(repo, n, &head)
            .await
            .map_err(err)?
            .review
            .id;
        let comment = NewComment {
            place: PendingPlace::New(suggestion()),
            body: SUGGESTION_BODY.to_owned(),
        };
        let queued = forge
            .add_review_comment(repo, n, &review, &comment)
            .await
            .map_err(err)?;
        ensure(queued.body == SUGGESTION_BODY, || {
            format!("add_review_comment returned {queued:?}")
        })?;
        forge
            .submit_review(repo, n, &review, Verdict::Comment, "")
            .await
            .map_err(err)?;
        let threads = self.threads().await?;
        find_thread(&threads, &suggestion(), SUGGESTION_BODY)?;
        Ok("suggestion fence read back unchanged".to_owned())
    }

    async fn conversation(&self) -> Step {
        let added = self
            .forge
            .add_comment(self.repo, self.number, CONVERSATION_BODY)
            .await
            .map_err(err)?;
        ensure(added.body == CONVERSATION_BODY, || {
            format!("add_comment returned {added:?}")
        })?;
        let threads = self.threads().await?;
        let found = threads
            .iter()
            .filter(|t| t.anchor.is_none())
            .flat_map(|t| &t.comments)
            .any(|c| c.id == added.id && c.body == CONVERSATION_BODY);
        ensure(found, || {
            "the conversation comment is not read back".to_owned()
        })?;
        Ok("conversation comment read back".to_owned())
    }

    async fn viewed(&self) -> Step {
        for viewed in [true, false] {
            let returned = self
                .forge
                .set_viewed(self.repo, self.number, TARGET, viewed)
                .await
                .map_err(err)?;
            let want = if viewed {
                ViewedState::Viewed
            } else {
                ViewedState::Unviewed
            };
            ensure(returned.path == TARGET && returned.state == want, || {
                format!("set_viewed({viewed}) returned {returned:?}")
            })?;
            let files = self
                .forge
                .viewed_files(self.repo, self.number)
                .await
                .map_err(err)?;
            let read = files.iter().find(|f| f.path == TARGET).map(|f| f.state);
            ensure(read == Some(want), || {
                format!("after set_viewed({viewed}) it reads {read:?}")
            })?;
        }
        Ok("viewed, then unviewed, read back each".to_owned())
    }

    async fn verdict(&self, verdict: Verdict) -> Step {
        let (forge, repo, n) = (self.forge, self.repo, self.number);
        let body = match verdict {
            Verdict::Approve => APPROVE_BODY,
            Verdict::RequestChanges => REQUEST_CHANGES_BODY,
            Verdict::Comment => REVIEW_BODY,
        };
        let head = self.head().await?;
        let review = forge
            .start_review(repo, n, &head)
            .await
            .map_err(err)?
            .review
            .id;
        let submitted = forge
            .submit_review(repo, n, &review, verdict, body)
            .await
            .map_err(err)?;
        ensure(submitted.state == verdict.state(), || {
            format!("submit_review returned {submitted:?}")
        })?;
        let threads = self.threads().await?;
        let found = threads.iter().any(|t| {
            t.anchor.is_none()
                && t.comments[0].verdict == Some(verdict.state())
                && t.comments[0].body == body
        });
        ensure(found, || format!("no {verdict:?} review read back"))?;
        Ok(format!("{verdict:?} read back"))
    }
}

/// The current anchor of an inline thread, in the composer's terms.
fn anchor_of(thread: &Thread) -> Option<CommentAnchor> {
    let anchor = thread.anchor.as_ref()?;
    Some(match &anchor.position {
        Some(position) => CommentAnchor::Line {
            path: anchor.path.clone(),
            line: position.line,
            start: position.start,
        },
        None => CommentAnchor::File {
            path: anchor.path.clone(),
        },
    })
}

/// The one thread at `anchor` whose first comment is `body`.
fn find_thread<'t>(
    threads: &'t [Thread],
    anchor: &CommentAnchor,
    body: &str,
) -> Result<&'t Thread, String> {
    let mut found = threads
        .iter()
        .filter(|t| t.comments[0].body == body)
        .filter(|t| !t.outdated);
    let thread = found
        .next()
        .ok_or_else(|| format!("no thread starts with {body:?}"))?;
    ensure(found.next().is_none(), || {
        format!("several threads start with {body:?}")
    })?;
    let read = anchor_of(thread);
    ensure(read.as_ref() == Some(anchor), || {
        format!("thread {body:?} is at {read:?}, expected {anchor:?}")
    })?;
    Ok(thread)
}
