//! forgejo backend against recordings of `cora/review-sandbox` on a
//! Forgejo 17.0.1 instance (`docs/sandboxes.md`).
//!
//! What the sandbox could not seed on Forgejo (resolved threads, multi-line
//! comments, Actions runs) is covered by the `synthetic_*` cassettes, which
//! are shaped from the v17 OpenAPI spec, not recorded.

use std::sync::Arc;
use std::time::Duration;

use forge::auth::{Credential, Scheme, StaticCredential, Token};
use forge::forgejo::Forgejo;
use forge::http::{HttpClient, HttpOptions, ManualClock};
use forge::model::{
    Anchor, CiState, DiffStat, ForgeKind, InboxSection, LineRef, PrNumber, PrState, PullRequest,
    RepoRef, Resolution, ReviewDecision, ReviewState, Side, StackSource, Thread,
};
use forge::record::{RECORD_ENV, ReplayTransport, fixture_client, fixture_path};
use forge::{Capabilities, Forge, ReplyMode, ResolveMode, StackSupport};

const HOST: &str = "forgejo.example.com";

fn sandbox() -> RepoRef {
    RepoRef::new(HOST, "cora", "review-sandbox")
}

fn recording() -> bool {
    std::env::var(RECORD_ENV).is_ok_and(|v| v == "1")
}

/// The backend over a cassette, and the replay to check at the end. When
/// recording, the network is used and there is nothing to check.
async fn connect(name: &str) -> (Forgejo, Option<Arc<ReplayTransport>>) {
    if recording() {
        let http = fixture_client(ForgeKind::Forgejo, HOST, name).unwrap();
        return (Forgejo::connect(http).await.unwrap(), None);
    }
    let replay = Arc::new(ReplayTransport::load(&fixture_path(ForgeKind::Forgejo, name)).unwrap());
    let http = HttpClient::new(
        ForgeKind::Forgejo,
        HOST,
        replay.clone(),
        Arc::new(StaticCredential(Credential {
            token: Token::new("replay"),
            scheme: Scheme::Token,
        })),
        HttpOptions {
            clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
            ..HttpOptions::for_kind(ForgeKind::Forgejo)
        },
    );
    (Forgejo::connect(http).await.unwrap(), Some(replay))
}

fn all_used(replay: Option<Arc<ReplayTransport>>) {
    if let Some(replay) = replay {
        assert_eq!(
            replay.unused(),
            Vec::<String>::new(),
            "recorded but never requested"
        );
    }
}

fn numbers(pulls: &[PullRequest]) -> Vec<u64> {
    pulls.iter().map(|pr| pr.number.0).collect()
}

fn sorted(mut numbers: Vec<u64>) -> Vec<u64> {
    numbers.sort_unstable();
    numbers
}

fn line(side: Side, line: u32) -> LineRef {
    LineRef { side, line }
}

fn bodies(thread: &Thread) -> Vec<&str> {
    thread.comments.iter().map(|c| c.body.as_str()).collect()
}

fn anchor_of(thread: &Thread) -> &Anchor {
    thread.anchor.as_ref().expect("an inline thread")
}

#[tokio::test]
async fn probes_v17_and_reads_the_account() {
    let (forgejo, replay) = connect("whoami").await;
    assert_eq!((forgejo.version().major, forgejo.version().minor), (17, 0));
    let caps = forgejo.capabilities();
    assert_eq!(caps, Capabilities::forgejo(forgejo.version()));
    assert!(caps.job_logs && caps.retry_job);
    assert_eq!(caps.reply, ReplyMode::SamePathAndLine);
    assert_eq!(caps.resolve, ResolveMode::ReadOnly);
    assert_eq!(caps.stacks, StackSupport::BaseBranch);
    assert_eq!(forgejo.kind(), ForgeKind::Forgejo);
    assert_eq!(forgejo.host(), HOST);

    let me = forgejo.whoami().await.unwrap();
    assert_eq!(me.login, "cora");
    assert!(me.id.is_some());
    assert!(me.avatar_url.is_some());
    all_used(replay);
}

#[tokio::test]
async fn inbox_sections() {
    let (forgejo, replay) = connect("inbox").await;
    let open = vec![1, 2, 3, 4, 5, 8];

    let authored = forgejo.inbox(&InboxSection::AuthoredByMe).await.unwrap();
    assert_eq!(sorted(numbers(&authored)), open);
    assert!(
        authored
            .windows(2)
            .all(|w| w[0].updated_at >= w[1].updated_at)
    );
    assert!(
        authored
            .iter()
            .all(|pr| pr.state.is_open() && pr.repo == sandbox())
    );
    assert!(
        authored
            .iter()
            .all(|pr| pr.head.repo.as_ref() == Some(&sandbox()))
    );

    let requested = forgejo.inbox(&InboxSection::ReviewRequested).await.unwrap();
    assert!(requested.is_empty(), "nobody asked cora for a review");

    let in_repo = forgejo
        .inbox(&InboxSection::OpenInRepo(sandbox()))
        .await
        .unwrap();
    assert_eq!(sorted(numbers(&in_repo)), open);

    // Inbox rows carry line counts and the forge's own comment counts
    // (`comments` 2 plus `review_comments` 2 on the threads PR); Forgejo
    // reports no resolved state in lists.
    let row = |n: u64| in_repo.iter().find(|pr| pr.number.0 == n).unwrap();
    assert_eq!(
        row(5).diff_stat,
        Some(DiffStat {
            additions: 72,
            deletions: 28,
            changed_files: 27
        })
    );
    assert_eq!(row(4).comment_count, Some(4));
    assert_eq!(row(1).comment_count, Some(0));
    assert!(in_repo.iter().all(|pr| pr.unresolved_threads.is_none()));

    // A query spans every repository of the account, so the window is the
    // time the sandbox was seeded.
    let closed = forgejo
        .inbox(&InboxSection::Query(
            "state=closed&since=2026-11-17T04:40:00Z&before=2026-11-17T04:50:00Z".to_owned(),
        ))
        .await
        .unwrap();
    assert_eq!(sorted(numbers(&closed)), vec![6, 7]);
    let state_of = |n: u64| closed.iter().find(|pr| pr.number.0 == n).unwrap().state;
    assert_eq!(
        (state_of(6), state_of(7)),
        (PrState::Merged, PrState::Closed)
    );
    all_used(replay);
}

#[tokio::test]
async fn pull_requests_match_the_sandbox() {
    let (forgejo, replay) = connect("pulls").await;
    let mut pulls = Vec::new();
    for n in 1..=8 {
        pulls.push(forgejo.pull_request(&sandbox(), PrNumber(n)).await.unwrap());
    }
    let by = |n: usize| &pulls[n - 1];
    // number, head, base, state
    let expected = [
        (1, "stack-1", "main", PrState::Open),
        (2, "stack-2", "stack-1", PrState::Open),
        (3, "stack-3", "stack-2", PrState::Open),
        (4, "threads", "main", PrState::Open),
        (5, "big", "main", PrState::Open),
        (6, "feature-merged", "main", PrState::Merged),
        (7, "feature-closed", "main", PrState::Closed),
        (8, "draft-wip", "main", PrState::Draft),
    ];
    for (n, head, base, state) in expected {
        let pr = by(n);
        assert_eq!(pr.number, PrNumber(n as u64));
        assert_eq!(
            (pr.head.name.as_str(), pr.base.name.as_str(), pr.state),
            (head, base, state),
            "#{n}"
        );
        assert_eq!(pr.repo, sandbox());
        assert_eq!(pr.author.login, "cora");
        assert_eq!(pr.head.sha.as_str().len(), 40);
        assert!(pr.merge_base.is_some(), "#{n} has a merge base");
        assert!(
            pr.web_url
                .ends_with(&format!("/cora/review-sandbox/pulls/{n}"))
        );
        assert_eq!(pr.ci, Some(CiState::None), "#{n}: the sandbox has no CI");
    }
    assert_eq!(by(5).diff_stat.unwrap().changed_files, 27);
    assert_eq!(by(8).title, "WIP: Start a feature");
    assert_eq!(by(2).title, "Stack 2/3: add whisper, expand guide");
    // The reviews of #4 are comments only.
    assert_eq!(by(4).review_decision, ReviewDecision::Commented);
    assert_eq!(by(4).reviewers.len(), 1);
    assert_eq!(by(4).reviewers[0].state, ReviewState::Commented);
    assert_eq!(by(1).review_decision, ReviewDecision::None);
    // Created in order, so timestamps follow numbers.
    assert!(pulls.windows(2).all(|w| w[0].created_at <= w[1].created_at));
    all_used(replay);
}

#[tokio::test]
async fn threads_of_the_threads_pull_request() {
    let (forgejo, replay) = connect("threads").await;
    let threads = forgejo.threads(&sandbox(), PrNumber(4)).await.unwrap();
    assert_eq!(threads.len(), 4, "{threads:#?}");

    // Written order: saturating_sub, checked_div (+ reply), removed line, is_even.
    let [saturating, checked, removed, is_even] = &threads[..] else {
        unreachable!()
    };
    for thread in &threads {
        assert_eq!(anchor_of(thread).path, "src/threads_target.rs");
        assert_eq!(
            thread.resolution,
            Resolution::Unresolved,
            "Forgejo could not seed a resolved thread"
        );
        assert!(thread.is_resolvable());
        assert!(
            thread
                .comments
                .iter()
                .all(|c| c.author.login == "cora" && c.review.is_some())
        );
        assert!(thread.comments.iter().all(|c| c.verdict.is_none()));
        assert_eq!(thread.id.as_str(), thread.comments[0].id.as_str());
    }

    let at = |thread: &Thread| {
        anchor_of(thread)
            .position
            .as_ref()
            .map(|p| (p.line, p.start))
    };
    assert_eq!(at(saturating), Some((line(Side::New, 8), None)));
    assert!(!saturating.outdated);
    assert_eq!(saturating.comments.len(), 1);
    assert!(bodies(saturating)[0].starts_with("Should this saturate"));

    // The reply is a second comment on the same line, and joins the thread.
    assert_eq!(at(checked), Some((line(Side::New, 16), None)));
    assert!(!checked.outdated);
    assert_eq!(
        bodies(checked),
        [
            "Nice simplification. Can you add a test for `b == 0`?",
            "Will do, tracking it in a follow-up."
        ]
    );
    assert!(checked.comments[0].created_at <= checked.comments[1].created_at);

    // The removed line `a - b` is old line 8.
    assert_eq!(at(removed), Some((line(Side::Old, 8), None)));
    assert!(!removed.outdated);
    assert!(bodies(removed)[0].starts_with("Why drop the plain subtraction"));

    // Commit two rewrote `is_even`, so the comment on it is outdated and
    // keeps only where it was written.
    assert!(is_even.outdated);
    let anchor = anchor_of(is_even);
    assert!(anchor.position.is_none());
    let original = anchor.original.as_ref().unwrap();
    assert_eq!(original.line, line(Side::New, 24));
    assert_eq!(original.commit.as_ref().unwrap().short(), "5bbf11d");
    assert!(bodies(is_even)[0].starts_with("Prefer `% 2`"));

    // A pull request without reviews or comments has no threads.
    let none = forgejo.threads(&sandbox(), PrNumber(1)).await.unwrap();
    assert!(none.is_empty());
    all_used(replay);
}

#[tokio::test]
async fn checks_without_ci_are_empty() {
    let (forgejo, replay) = connect("checks").await;
    let checks = forgejo.checks(&sandbox(), PrNumber(4)).await.unwrap();
    assert!(checks.is_empty(), "{checks:#?}");
    all_used(replay);
}

#[tokio::test]
async fn stack_follows_base_branches() {
    let (forgejo, replay) = connect("stack").await;
    for n in [1, 2, 3] {
        let stack = forgejo.stack(&sandbox(), PrNumber(n)).await.unwrap();
        assert_eq!(stack.source, StackSource::BaseBranch);
        let linked: Vec<u64> = stack.links.iter().map(|l| l.number.0).collect();
        assert_eq!(linked, [1, 2, 3], "asked from #{n}");
        assert_eq!(stack.position_of(PrNumber(n)), Some(n as usize - 1));
        let bases: Vec<&str> = stack.links.iter().map(|l| l.base.as_str()).collect();
        assert_eq!(bases, ["main", "stack-1", "stack-2"]);
    }
    let alone = forgejo.stack(&sandbox(), PrNumber(4)).await.unwrap();
    assert_eq!(alone.links.len(), 1);
    // A merged pull request is not in the open list, and is a stack of one.
    let merged = forgejo.stack(&sandbox(), PrNumber(6)).await.unwrap();
    assert_eq!(merged.links.len(), 1);
    assert_eq!(merged.links[0].state, PrState::Merged);
    all_used(replay);
}

#[tokio::test]
async fn synthetic_threads_cover_what_the_sandbox_could_not_seed() {
    let (forgejo, replay) = connect("synthetic_threads").await;
    let threads = forgejo.threads(&sandbox(), PrNumber(4)).await.unwrap();

    let summary = |id: &str| threads.iter().find(|t| t.id.as_str() == id).unwrap();
    let by_first_comment = |id: &str| {
        threads
            .iter()
            .find(|t| t.comments[0].id.as_str() == id)
            .unwrap()
    };

    // A review with a body is an anchorless thread that carries its verdict.
    let approved = summary("review-10");
    assert!(approved.anchor.is_none());
    assert_eq!(approved.resolution, Resolution::NotResolvable);
    assert_eq!(approved.comments[0].verdict, Some(ReviewState::Approved));
    assert_eq!(approved.comments[0].body, "Looks good overall");
    let changes = summary("review-11");
    assert_eq!(
        changes.comments[0].verdict,
        Some(ReviewState::ChangesRequested)
    );
    let dismissed = summary("review-14");
    assert_eq!(dismissed.comments[0].verdict, Some(ReviewState::Dismissed));
    // A comment-only review with no body adds no summary, and the viewer's
    // pending review is left out together with its comments.
    assert!(threads.iter().all(|t| t.id.as_str() != "review-13"));
    assert!(threads.iter().all(|t| t.id.as_str() != "review-12"));

    // Conversation comment.
    let remark = summary("200");
    assert!(remark.anchor.is_none() && !remark.is_resolvable());
    assert_eq!(remark.comments[0].body, "General remark");
    assert!(remark.comments[0].edited_at.is_some());

    // Resolved range: the reply joins it, `resolver` on one comment resolves
    // the thread, and `extra_lines_count` extends the range after line 10.
    let range = by_first_comment("101");
    assert_eq!(bodies(range).len(), 2);
    let Resolution::Resolved { by } = &range.resolution else {
        panic!("resolved, got {:?}", range.resolution)
    };
    assert_eq!(by.as_ref().unwrap().login, "mira");
    let position = anchor_of(range).position.as_ref().unwrap();
    assert_eq!(position.line, line(Side::New, 12));
    assert_eq!(position.start, Some(line(Side::New, 10)));
    assert!(!range.outdated);

    // Old side stays on its own commit.
    let old = by_first_comment("103");
    assert_eq!(
        anchor_of(old).position.as_ref().unwrap().line,
        line(Side::Old, 5)
    );
    assert_eq!(old.resolution, Resolution::Unresolved);

    // Written on an older commit: current when the line still reads the
    // same in the head's file, outdated when it does not.
    let same = by_first_comment("104");
    assert!(!same.outdated);
    assert_eq!(
        anchor_of(same).position.as_ref().unwrap().line,
        line(Side::New, 3)
    );
    let rewritten = by_first_comment("105");
    assert!(rewritten.outdated);
    assert!(anchor_of(rewritten).position.is_none());
    assert_eq!(
        anchor_of(rewritten).original.as_ref().unwrap().line,
        line(Side::New, 5)
    );

    assert_eq!(threads.len(), 8, "{threads:#?}");
    all_used(replay);
}

#[tokio::test]
async fn synthetic_checks_join_statuses_runs_and_jobs() {
    let (forgejo, replay) = connect("synthetic_checks").await;
    let checks = forgejo.checks(&sandbox(), PrNumber(4)).await.unwrap();
    let names: Vec<&str> = checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "build (pull_request)",
            "test (pull_request)",
            "codecov/patch",
            "deploy"
        ]
    );
    let [build, test, codecov, deploy] = &checks[..] else {
        unreachable!()
    };

    // `pending` status, running job: running. Linked to its run and job.
    assert_eq!(build.state, CiState::Running);
    assert_eq!(build.group.as_deref(), Some("ci"));
    assert_eq!(build.job.as_ref().unwrap().as_str(), "700");
    assert_eq!(build.run.as_ref().unwrap().as_str(), "70");
    assert_eq!(
        build.url.as_deref(),
        Some("https://forgejo.example.com/cora/review-sandbox/actions/runs/7/jobs/0")
    );
    assert!(build.steps.is_empty());
    assert!(build.completed_at.is_none());

    // Two statuses for one context: the newer one wins.
    assert_eq!(test.state, CiState::Pass);
    assert_eq!(test.job.as_ref().unwrap().as_str(), "701");
    assert!(test.completed_at.is_some());

    // A status from another service is not a job.
    assert_eq!(codecov.state, CiState::Pass);
    assert!(codecov.group.is_none() && codecov.job.is_none() && codecov.run.is_none());

    // A job the forge has not reported a status for yet still shows.
    assert_eq!(deploy.state, CiState::Queued);
    assert_eq!(deploy.job.as_ref().unwrap().as_str(), "702");
    assert_eq!(deploy.group.as_deref(), Some("ci.yaml"));
    all_used(replay);
}
