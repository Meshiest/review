//! Where review writes go. The composer, the cards and the submit bar hand a
//! [`ReviewCommand`] to a [`ReviewSink`] and keep the user's text until the
//! sink says it worked. The live app wires [`crate::forge_sink::ForgeSink`];
//! fixtures and benchmarks use [`LogSink`].

use forge::model::{
    Comment, CommentAnchor, CommentId, LineRef, PendingComment, PendingPlace, PendingReview,
    Review, Sha, Thread, ThreadId,
};
use futures::future::{BoxFuture, FutureExt as _};
use store::{DraftAnchor, PrKey};

/// A failed write, as text for the composer's footer: the forge's own
/// message where it gave one.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SinkError(pub String);

/// How a review is closed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Verdict {
    #[default]
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    pub const ALL: [Self; 3] = [Self::Comment, Self::Approve, Self::RequestChanges];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Comment => "Comment",
            Self::Approve => "Approve",
            Self::RequestChanges => "Request changes",
        }
    }

    #[must_use]
    pub fn to_forge(self) -> forge::model::Verdict {
        match self {
            Self::Comment => forge::model::Verdict::Comment,
            Self::Approve => forge::model::Verdict::Approve,
            Self::RequestChanges => forge::model::Verdict::RequestChanges,
        }
    }
}

/// What a new comment is attached to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A line, or the range ending at `line` and starting at `start`.
    Line {
        path: String,
        line: LineRef,
        start: Option<LineRef>,
    },
    File {
        path: String,
    },
    /// The pull request's own conversation.
    Conversation,
}

impl Place {
    /// The place a draft anchor names; replies, edits and the review
    /// summary have none.
    #[must_use]
    pub fn of(anchor: &DraftAnchor) -> Option<Self> {
        match anchor {
            DraftAnchor::Line { path, line, start } => Some(Self::Line {
                path: path.clone(),
                line: *line,
                start: *start,
            }),
            DraftAnchor::File { path } => Some(Self::File { path: path.clone() }),
            DraftAnchor::Conversation => Some(Self::Conversation),
            DraftAnchor::Reply { .. } | DraftAnchor::Review | DraftAnchor::Edit { .. } => None,
        }
    }

    /// The forge's anchor for an inline place.
    #[must_use]
    pub fn comment_anchor(&self) -> Option<CommentAnchor> {
        match self {
            Self::Line { path, line, start } => Some(CommentAnchor::Line {
                path: path.clone(),
                line: *line,
                start: *start,
            }),
            Self::File { path } => Some(CommentAnchor::File { path: path.clone() }),
            Self::Conversation => None,
        }
    }
}

/// Where a queued comment sits in the forge's pending review, from the
/// anchor it was drafted under.
#[must_use]
pub fn pending_place(anchor: &DraftAnchor) -> Option<PendingPlace> {
    match anchor {
        DraftAnchor::Reply { thread } => Some(PendingPlace::Reply(thread.clone())),
        other => Place::of(other)?.comment_anchor().map(PendingPlace::New),
    }
}

/// The draft anchor of a comment the forge's pending review already has.
#[must_use]
pub fn draft_anchor(place: &PendingPlace) -> DraftAnchor {
    match place {
        PendingPlace::Reply(thread) => DraftAnchor::Reply {
            thread: thread.clone(),
        },
        PendingPlace::New(CommentAnchor::Line { path, line, start }) => DraftAnchor::Line {
            path: path.clone(),
            line: *line,
            start: *start,
        },
        PendingPlace::New(CommentAnchor::File { path }) => DraftAnchor::File { path: path.clone() },
    }
}

/// A new comment: where, what it says, and the head it was written against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewComment {
    pub place: Place,
    pub body: String,
    pub commit: Option<Sha>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReviewCommand {
    /// Add the comment to the pending review, starting one if needed.
    QueueComment(NewComment),
    /// Post the comment on its own, outside any review.
    SendComment(NewComment),
    Reply {
        thread: ThreadId,
        body: String,
    },
    /// Submit the pending review with its summary.
    SubmitReview {
        verdict: Verdict,
        body: String,
    },
    SetResolved {
        thread: ThreadId,
        resolved: bool,
    },
    /// Remove a queued comment from the forge's pending review.
    DeleteQueued(CommentId),
    /// Give a queued comment new text: in place where the forge allows it,
    /// otherwise by deleting and adding it again.
    EditQueued {
        comment: PendingComment,
        body: String,
    },
}

/// What a write gave back, for the app to patch its state with instead of
/// reading everything again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SinkOutcome {
    /// Done, with nothing to show (the log sink, a delete).
    Done,
    /// The comment is in the forge's pending review.
    Queued(PendingComment),
    /// A queued comment's new text (and id, after a delete and re-add).
    Edited(PendingComment),
    /// Threads that went out: a single comment's, or a submitted review's.
    Posted(ThreadChange),
    /// A comment that joined `thread`.
    Replied { thread: ThreadId, comment: Comment },
    /// The submitted review and the threads it made.
    Submitted {
        review: Box<Review>,
        change: ThreadChange,
    },
    /// A thread as it reads after being resolved or unresolved.
    Resolved(Box<Thread>),
}

/// How a write changed the pull request's threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadChange {
    /// New threads, and comments that joined existing ones.
    Add {
        threads: Vec<Thread>,
        replies: Vec<(ThreadId, Comment)>,
    },
    /// One thread as it now reads (resolved or unresolved).
    Replace(Box<Thread>),
    /// Every thread, read again: the forge gave no ids to patch with
    /// (GitLab draft notes become notes with new ids when published).
    All(Vec<Thread>),
}

impl ThreadChange {
    /// `threads` with the change applied: new threads after the others
    /// (oldest first, as reads list them), replies at the end of their
    /// thread, unless a read already has the comment.
    #[must_use]
    pub fn apply(&self, threads: &[Thread]) -> Vec<Thread> {
        match self {
            Self::All(all) => all.clone(),
            Self::Replace(thread) => threads
                .iter()
                .map(|old| {
                    if old.id == thread.id {
                        (**thread).clone()
                    } else {
                        old.clone()
                    }
                })
                .collect(),
            Self::Add {
                threads: added,
                replies,
            } => {
                let mut out = threads.to_vec();
                for (thread, comment) in replies {
                    if let Some(target) = out.iter_mut().find(|t| t.id == *thread)
                        && target.comments.iter().all(|c| c.id != comment.id)
                    {
                        target.comments.push(comment.clone());
                    }
                }
                for thread in added {
                    if out.iter().all(|t| t.id != thread.id) {
                        out.push(thread.clone());
                    }
                }
                out
            }
        }
    }
}

/// Receives the user's writes. The returned future resolves when the forge
/// (or the local queue) has the write; only then does the app delete the
/// draft behind it. It is polled on the UI thread's executor, so a sink that
/// needs another runtime hands the work over and returns a channel.
pub trait ReviewSink: Send + Sync + 'static {
    fn submit(
        &self,
        pr: &PrKey,
        command: ReviewCommand,
    ) -> BoxFuture<'static, Result<SinkOutcome, SinkError>>;

    /// The forge's pending review of `pr`, to pick up one started elsewhere.
    /// The default has none.
    fn pending(&self, pr: &PrKey) -> BoxFuture<'static, Result<Option<PendingReview>, SinkError>> {
        let _ = pr;
        async { Ok(None) }.boxed()
    }
}

/// Prints each command to stderr and reports success.
#[derive(Clone, Copy, Debug, Default)]
pub struct LogSink;

impl ReviewSink for LogSink {
    fn submit(
        &self,
        pr: &PrKey,
        command: ReviewCommand,
    ) -> BoxFuture<'static, Result<SinkOutcome, SinkError>> {
        eprintln!(
            "review sink: {}#{}: {command:?}",
            pr.repo.path(),
            pr.number.0
        );
        async { Ok(SinkOutcome::Done) }.boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge::model::{Resolution, Side, User};

    fn thread(id: &str, comments: &[&str]) -> Thread {
        Thread {
            id: ThreadId::new(id),
            anchor: None,
            resolution: Resolution::Unresolved,
            outdated: false,
            comments: comments.iter().map(|c| comment(c)).collect(),
        }
    }

    fn comment(id: &str) -> Comment {
        Comment {
            id: CommentId::new(id),
            author: User::new("cora"),
            body: id.to_owned(),
            created_at: chrono::DateTime::UNIX_EPOCH,
            edited_at: None,
            web_url: None,
            review: None,
            verdict: None,
            suggestions: Vec::new(),
        }
    }

    fn ids(threads: &[Thread]) -> Vec<(String, usize)> {
        threads
            .iter()
            .map(|t| (t.id.to_string(), t.comments.len()))
            .collect()
    }

    #[test]
    fn changes_patch_threads_without_doubling_what_is_there() {
        let threads = [thread("A", &["a1"]), thread("B", &["b1"])];
        let add = ThreadChange::Add {
            threads: vec![thread("C", &["c1"]), thread("A", &["a1"])],
            replies: vec![
                (ThreadId::new("B"), comment("b2")),
                (ThreadId::new("B"), comment("b1")),
            ],
        };
        assert_eq!(
            ids(&add.apply(&threads)),
            [("A".into(), 1), ("B".into(), 2), ("C".into(), 1)]
        );
        let mut resolved = thread("B", &["b1"]);
        resolved.resolution = Resolution::Resolved { by: None };
        let patched = ThreadChange::Replace(Box::new(resolved)).apply(&threads);
        assert!(patched[1].is_resolved() && !patched[0].is_resolved());
        assert_eq!(ids(&ThreadChange::All(Vec::new()).apply(&threads)), []);
    }

    #[test]
    fn draft_anchors_and_pending_places_convert_both_ways() {
        let line = DraftAnchor::Line {
            path: "a.rs".into(),
            line: LineRef {
                side: Side::Old,
                line: 3,
            },
            start: None,
        };
        let reply = DraftAnchor::Reply {
            thread: ThreadId::new("T"),
        };
        let file = DraftAnchor::File {
            path: "b.rs".into(),
        };
        for anchor in [line, reply, file] {
            let place = pending_place(&anchor).expect("an inline anchor has a place");
            assert_eq!(draft_anchor(&place), anchor);
        }
        assert_eq!(pending_place(&DraftAnchor::Conversation), None);
        assert_eq!(pending_place(&DraftAnchor::Review), None);
    }
}
