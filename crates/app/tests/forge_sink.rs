//! `ForgeSink` against a fake forge kept in memory: which writes it sends
//! for each command, and what it hands back for the app to patch with.

use std::sync::{Arc, Mutex};

use app::config::Config;
use app::forge_sink::ForgeSink;
use app::services::{Services, Spawn};
use app::sink::{NewComment, Place, ReviewCommand, ReviewSink, SinkOutcome, ThreadChange, Verdict};
use async_trait::async_trait;
use chrono::Utc;
use forge::model::{
    BranchRef, CheckRun, Comment, CommentAnchor, CommentId, ForgeKind, InboxSection, LineRef,
    NewComment as ForgeComment, PendingComment, PendingPlace, PendingReview, PrNumber, PrState,
    PullRequest, PullRequestId, RepoRef, Resolution, Review, ReviewDecision, ReviewId, ReviewState,
    Sha, Side, Stack, Thread, ThreadId, User,
};
use forge::{Capabilities, Error, Forge, ForgeHub, Result};
use store::PrKey;

const HOST: &str = "github.com";

/// A GitHub-like forge whose pending review lives in memory. `log` names
/// every write in order.
struct Fake {
    capabilities: Capabilities,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    log: Vec<String>,
    pending: Option<PendingReview>,
    next: u32,
    /// Submits answer 422 with this message.
    refuse_submit: Option<&'static str>,
}

impl Fake {
    fn new(capabilities: Capabilities) -> Self {
        Self {
            capabilities,
            state: Mutex::default(),
        }
    }

    fn log(&self, entry: impl Into<String>) {
        self.state.lock().unwrap().log.push(entry.into());
    }

    fn id(&self, prefix: &str) -> String {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        format!("{prefix}{}", state.next)
    }
}

fn repo() -> RepoRef {
    RepoRef::new(HOST, "Lovelace", "review-sandbox")
}

fn pr(number: PrNumber) -> PullRequest {
    let branch = |name: &str| BranchRef {
        repo: Some(repo()),
        name: name.to_owned(),
        sha: Sha::new("f".repeat(40)),
    };
    PullRequest {
        id: PullRequestId::new("PR"),
        repo: repo(),
        number,
        title: String::new(),
        body: String::new(),
        author: User::new("cora"),
        state: PrState::Open,
        base: branch("main"),
        head: branch("topic"),
        merge_base: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        review_decision: ReviewDecision::None,
        reviewers: Vec::new(),
        ci: None,
        comment_count: None,
        unresolved_threads: None,
        labels: Vec::new(),
        diff_stat: None,
        assignees: Vec::new(),
        milestone: None,
        web_url: String::new(),
        head_writable: true,
    }
}

fn review(id: &str, state: ReviewState) -> Review {
    Review {
        id: ReviewId::new(id),
        author: User::new("cora"),
        state,
        body: String::new(),
        commit: Some(Sha::new("f".repeat(40))),
        submitted_at: None,
        web_url: None,
    }
}

#[async_trait]
impl Forge for Fake {
    fn kind(&self) -> ForgeKind {
        ForgeKind::GitHub
    }

    fn host(&self) -> &str {
        HOST
    }

    fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    async fn whoami(&self) -> Result<User> {
        Ok(User::new("cora"))
    }

    async fn inbox(&self, _: &InboxSection) -> Result<Vec<PullRequest>> {
        Ok(Vec::new())
    }

    async fn pull_request(&self, _: &RepoRef, number: PrNumber) -> Result<PullRequest> {
        Ok(pr(number))
    }

    async fn threads(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<Thread>> {
        Ok(Vec::new())
    }

    async fn checks(&self, _: &RepoRef, _: PrNumber) -> Result<Vec<CheckRun>> {
        Ok(Vec::new())
    }

    async fn stack(&self, _: &RepoRef, number: PrNumber) -> Result<Stack> {
        Ok(forge::stack_by_base_branch(&pr(number), &[]))
    }

    async fn start_review(&self, _: &RepoRef, _: PrNumber, commit: &Sha) -> Result<PendingReview> {
        if self.state.lock().unwrap().pending.is_some() {
            return Err(Error::Status {
                status: 422,
                message: "User can only have one pending review per pull request".into(),
                request: "start review".into(),
            });
        }
        self.log(format!("start {}", commit.short()));
        let id = self.id("R");
        let pending = PendingReview {
            review: review(&id, ReviewState::Pending),
            comments: Vec::new(),
        };
        self.state.lock().unwrap().pending = Some(pending.clone());
        Ok(pending)
    }

    async fn pending_review(&self, _: &RepoRef, _: PrNumber) -> Result<Option<PendingReview>> {
        let pending = self.state.lock().unwrap().pending.clone();
        // Long enough for a write sent beside this one to ask too.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        Ok(pending)
    }

    async fn add_review_comment(
        &self,
        _: &RepoRef,
        _: PrNumber,
        review: &ReviewId,
        comment: &ForgeComment,
    ) -> Result<PendingComment> {
        self.log(format!("add {review}"));
        let id = self.id("C");
        let added = PendingComment {
            id: CommentId::new(&id),
            place: comment.place.clone(),
            thread: Some(ThreadId::new(format!("T{id}"))),
            body: comment.body.clone(),
            created_at: Some(Utc::now()),
        };
        let mut state = self.state.lock().unwrap();
        match &mut state.pending {
            Some(pending) if pending.review.id == *review => pending.comments.push(added.clone()),
            _ => {
                return Err(Error::NotFound {
                    request: format!("review {review}"),
                });
            }
        }
        Ok(added)
    }

    async fn delete_review_comment(
        &self,
        _: &RepoRef,
        _: PrNumber,
        comment: &CommentId,
    ) -> Result<()> {
        self.log(format!("delete {comment}"));
        if let Some(pending) = &mut self.state.lock().unwrap().pending {
            pending.comments.retain(|c| c.id != *comment);
        }
        Ok(())
    }

    async fn submit_review(
        &self,
        _: &RepoRef,
        _: PrNumber,
        review_id: &ReviewId,
        verdict: forge::model::Verdict,
        body: &str,
    ) -> Result<Review> {
        self.log(format!("submit {review_id} {verdict:?}"));
        if let Some(message) = self.state.lock().unwrap().refuse_submit {
            return Err(Error::Status {
                request: "POST /graphql".into(),
                status: 422,
                message: message.into(),
            });
        }
        self.state.lock().unwrap().pending = None;
        Ok(Review {
            body: body.to_owned(),
            submitted_at: Some(Utc::now()),
            ..review(review_id.as_str(), verdict.state())
        })
    }

    async fn discard_review(&self, _: &RepoRef, _: PrNumber, review: &ReviewId) -> Result<()> {
        self.log(format!("discard {review}"));
        self.state.lock().unwrap().pending = None;
        Ok(())
    }

    async fn reply(
        &self,
        _: &RepoRef,
        _: PrNumber,
        thread: &ThreadId,
        body: &str,
    ) -> Result<Comment> {
        self.log(format!("reply {thread}"));
        Ok(Comment {
            id: CommentId::new(self.id("C")),
            author: User::new("cora"),
            body: body.to_owned(),
            created_at: Utc::now(),
            edited_at: None,
            web_url: None,
            review: None,
            verdict: None,
            suggestions: Vec::new(),
        })
    }

    async fn set_resolved(
        &self,
        _: &RepoRef,
        _: PrNumber,
        thread: &ThreadId,
        resolved: bool,
    ) -> Result<Thread> {
        self.log(format!("resolve {thread} {resolved}"));
        Ok(Thread {
            id: thread.clone(),
            anchor: None,
            resolution: if resolved {
                Resolution::Resolved { by: None }
            } else {
                Resolution::Unresolved
            },
            outdated: false,
            comments: Vec::new(),
        })
    }
}

fn sink(capabilities: Capabilities) -> (ForgeSink, Arc<Fake>) {
    let fake = Arc::new(Fake::new(capabilities));
    let hub = ForgeHub::with_backends(vec![fake.clone() as Arc<dyn Forge>]).unwrap();
    let spawn: Spawn = Arc::new(|future| {
        std::thread::spawn(move || futures::executor::block_on(future));
    });
    let config = Config::parse("[[forges]]\nkind = \"github\"\nhost = \"github.com\"\n").unwrap();
    let services = Services::new(Arc::new(config), None, Some(Arc::new(hub)), spawn);
    (ForgeSink::new(services), fake)
}

fn key() -> PrKey {
    PrKey::new(repo(), PrNumber(4))
}

fn line(n: u32) -> NewComment {
    NewComment {
        place: Place::Line {
            path: "src/a.rs".into(),
            line: LineRef {
                side: Side::New,
                line: n,
            },
            start: None,
        },
        body: format!("comment on {n}"),
        commit: Some(Sha::new("a".repeat(40))),
    }
}

fn send(sink: &ForgeSink, command: ReviewCommand) -> std::result::Result<SinkOutcome, String> {
    futures::executor::block_on(sink.submit(&key(), command)).map_err(|e| e.0)
}

fn log(fake: &Fake) -> Vec<String> {
    std::mem::take(&mut fake.state.lock().unwrap().log)
}

#[test]
fn the_first_queued_comment_starts_the_review_and_the_rest_join_it() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    for n in [3, 4] {
        let outcome = send(&sink, ReviewCommand::QueueComment(line(n))).unwrap();
        assert!(matches!(outcome, SinkOutcome::Queued(_)), "{outcome:?}");
    }
    assert_eq!(log(&fake), ["start aaaaaaa", "add R1", "add R1"]);

    let outcome = send(
        &sink,
        ReviewCommand::SubmitReview {
            verdict: Verdict::Comment,
            body: String::new(),
        },
    )
    .unwrap();
    let SinkOutcome::Submitted { review, change } = outcome else {
        panic!("expected a submitted review");
    };
    assert_eq!(review.state, ReviewState::Commented);
    let ThreadChange::Add { threads, replies } = change else {
        panic!("expected new threads");
    };
    let lines: Vec<u32> = threads
        .iter()
        .filter_map(|t| Some(t.anchor.as_ref()?.position.as_ref()?.line.line))
        .collect();
    assert_eq!(lines, [3, 4], "one thread per queued comment, no summary");
    assert!(replies.is_empty());
    assert_eq!(log(&fake), ["submit R1 Comment"]);
}

#[test]
fn comments_queued_at_once_go_into_one_review() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    let outcomes = futures::executor::block_on(futures::future::join_all(
        [3, 4, 5].map(|n| sink.submit(&key(), ReviewCommand::QueueComment(line(n)))),
    ));
    for outcome in outcomes {
        assert!(matches!(outcome, Ok(SinkOutcome::Queued(_))), "{outcome:?}");
    }
    assert_eq!(log(&fake), ["start aaaaaaa", "add R1", "add R1", "add R1"]);
}

#[test]
fn a_review_started_elsewhere_is_picked_up() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    futures::executor::block_on(fake.start_review(&repo(), PrNumber(4), &Sha::new("b".repeat(40))))
        .unwrap();
    log(&fake);
    let pending = futures::executor::block_on(sink.pending(&key())).unwrap();
    assert!(pending.is_some());
    send(&sink, ReviewCommand::QueueComment(line(3))).unwrap();
    assert_eq!(log(&fake), ["add R1"], "no second review is started");
}

#[test]
fn a_single_comment_is_its_own_review_unless_one_is_pending() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    let outcome = send(&sink, ReviewCommand::SendComment(line(5))).unwrap();
    let SinkOutcome::Posted(ThreadChange::Add { threads, .. }) = outcome else {
        panic!("expected a posted thread");
    };
    assert_eq!(threads.len(), 1);
    assert_eq!(threads[0].comments[0].body, "comment on 5");
    assert_eq!(log(&fake), ["start aaaaaaa", "add R1", "submit R1 Comment"]);

    send(&sink, ReviewCommand::QueueComment(line(6))).unwrap();
    let refused = send(&sink, ReviewCommand::SendComment(line(7))).unwrap_err();
    assert!(refused.contains("pending"), "{refused}");
    assert_eq!(
        log(&fake),
        ["start aaaaaaa", "add R3"],
        "nothing sent for the refused one"
    );
}

#[test]
fn edits_go_in_place_or_as_delete_and_add() {
    let place = PendingPlace::New(CommentAnchor::Line {
        path: "src/a.rs".into(),
        line: LineRef {
            side: Side::New,
            line: 3,
        },
        start: None,
    });
    // Forgejo has no edit: the comment is deleted and added again.
    let (sink, fake) = sink(Capabilities::forgejo(
        forge::ForgejoVersion::OLDEST_SUPPORTED,
    ));
    let SinkOutcome::Queued(queued) = send(&sink, ReviewCommand::QueueComment(line(3))).unwrap()
    else {
        panic!("queued");
    };
    log(&fake);
    let outcome = send(
        &sink,
        ReviewCommand::EditQueued {
            comment: queued.clone(),
            body: "better".into(),
        },
    )
    .unwrap();
    let SinkOutcome::Edited(edited) = outcome else {
        panic!("edited");
    };
    assert_eq!(edited.body, "better");
    assert_eq!(edited.place, place);
    assert_ne!(edited.id, queued.id, "a new comment");
    assert_eq!(
        log(&fake),
        [format!("delete {}", queued.id), "add R1".to_owned()]
    );
}

#[test]
fn replies_and_resolves_hand_back_what_the_forge_returned() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    let thread = ThreadId::new("T9");
    let outcome = send(
        &sink,
        ReviewCommand::Reply {
            thread: thread.clone(),
            body: "ok".into(),
        },
    )
    .unwrap();
    assert!(
        matches!(&outcome, SinkOutcome::Replied { thread: t, comment } if *t == thread && comment.body == "ok")
    );
    let outcome = send(
        &sink,
        ReviewCommand::SetResolved {
            thread: thread.clone(),
            resolved: true,
        },
    )
    .unwrap();
    assert!(matches!(&outcome, SinkOutcome::Resolved(t) if t.is_resolved()));
    assert_eq!(log(&fake), ["reply T9", "resolve T9 true"]);
}

#[test]
fn nothing_to_submit_is_said_plainly() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    let error = send(
        &sink,
        ReviewCommand::SubmitReview {
            verdict: Verdict::Comment,
            body: String::new(),
        },
    )
    .unwrap_err();
    assert!(error.starts_with("Nothing to submit"), "{error}");
    assert!(log(&fake).is_empty());
}

#[test]
fn a_refused_submit_leaves_no_review_it_started_behind() {
    let (sink, fake) = sink(Capabilities::GITHUB);
    fake.state.lock().unwrap().refuse_submit = Some("Can not approve your own pull request");
    let error = send(
        &sink,
        ReviewCommand::SubmitReview {
            verdict: Verdict::Approve,
            body: String::new(),
        },
    )
    .unwrap_err();
    assert_eq!(
        error, "Can not approve your own pull request",
        "the forge's own words"
    );
    assert_eq!(
        log(&fake),
        ["start fffffff", "submit R1 Approve", "discard R1"]
    );

    let error = send(&sink, ReviewCommand::SendComment(line(5))).unwrap_err();
    assert!(!error.is_empty());
    assert_eq!(
        log(&fake),
        ["start aaaaaaa", "add R2", "submit R2 Comment", "discard R2"]
    );
    assert!(fake.state.lock().unwrap().pending.is_none());
}
