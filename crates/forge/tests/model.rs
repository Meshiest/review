mod common;

use std::fmt::Debug;

use forge::model::{
    AccountKind, Anchor, CheckRun, CheckRunId, CheckStep, CiJob, CiRun, CiState, Comment,
    CommentId, ForgeKind, InboxSection, JobId, LineRef, LogChunk, Position, PrNumber, PrState,
    PullRequest, Resolution, ReviewId, ReviewState, RunId, Sha, Side, Snapshot, Stack, StackSource,
    Thread, ThreadId, User, UserId, ViewedFile, ViewedState,
};
use forge::model::{MergeBlocker, MergeMethod, MergeOptions, MergeSettings, Mergeability};
use forge::{Capabilities, stack_by_base_branch};
use serde::Serialize;
use serde::de::DeserializeOwned;

use common::{at, pr, repo};

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T) {
    let json = serde_json::to_string(value).unwrap();
    let back: T = serde_json::from_str(&json).unwrap();
    assert_eq!(&back, value, "{json}");
}

fn thread() -> Thread {
    let mira = User {
        id: Some(UserId::new("U_1")),
        name: Some("Mira".to_owned()),
        avatar_url: None,
        kind: AccountKind::User,
        ..User::new("mira")
    };
    Thread {
        id: ThreadId::new("PRRT_kwDO"),
        anchor: Some(Anchor {
            path: "src/lib.rs".to_owned(),
            old_path: Some("src/old.rs".to_owned()),
            position: None,
            original: Some(Position {
                line: LineRef {
                    side: Side::New,
                    line: 14,
                },
                start: Some(LineRef {
                    side: Side::Old,
                    line: 10,
                }),
                commit: Some(Sha::new("c".repeat(40))),
            }),
        }),
        resolution: Resolution::Resolved {
            by: Some(mira.clone()),
        },
        outdated: true,
        comments: vec![Comment {
            id: CommentId::new("123"),
            author: mira,
            body: "```suggestion\nlet x = 1;\n```".to_owned(),
            created_at: at(1),
            edited_at: Some(at(2)),
            web_url: None,
            review: Some(ReviewId::new("PRR_1")),
            verdict: Some(ReviewState::ChangesRequested),
            suggestions: Vec::new(),
        }],
    }
}

fn check() -> CheckRun {
    CheckRun {
        id: CheckRunId::new("CR_1"),
        name: "test (linux)".to_owned(),
        group: Some("ci".to_owned()),
        state: CiState::Fail,
        url: Some("https://github.com/x/actions/runs/1/job/2".to_owned()),
        job: Some(JobId::new("2")),
        run: Some(RunId::new("1")),
        started_at: Some(at(3)),
        completed_at: None,
        steps: vec![CheckStep {
            number: 1,
            name: "cargo test".to_owned(),
            state: CiState::Fail,
            started_at: Some(at(3)),
            completed_at: Some(at(4)),
        }],
    }
}

#[test]
fn every_model_type_round_trips() {
    let snapshot = Snapshot {
        pull_request: pr(38, "cora/pack-reader", "main"),
        threads: vec![
            thread(),
            Thread {
                anchor: None,
                resolution: Resolution::NotResolvable,
                outdated: false,
                ..thread()
            },
        ],
        checks: vec![check()],
    };
    round_trip(&snapshot);
    round_trip(&stack_by_base_branch(&snapshot.pull_request, &[]));
    round_trip(&ViewedFile {
        path: "a.rs".to_owned(),
        state: ViewedState::Dismissed,
    });
    for section in [
        InboxSection::ReviewRequested,
        InboxSection::AuthoredByMe,
        InboxSection::OpenInRepo(repo()),
        InboxSection::Query("is:open label:bug".to_owned()),
    ] {
        round_trip(&section);
    }
    for kind in ForgeKind::ALL {
        round_trip(&kind);
        round_trip(&Capabilities::baseline(kind));
    }
    let check = check();
    round_trip(&CiRun {
        id: RunId::new("1"),
        name: "ci".to_owned(),
        event: Some("pull_request".to_owned()),
        state: CiState::Fail,
        number: Some(4),
        head_sha: Sha::new("b".repeat(40)),
        branch: Some("feature".to_owned()),
        started_at: Some(at(1)),
        completed_at: Some(at(2)),
        url: check.url.clone(),
        jobs: vec![CiJob {
            id: JobId::new("2"),
            name: check.name,
            stage: None,
            state: CiState::Fail,
            attempt: Some(2),
            started_at: check.started_at,
            completed_at: check.completed_at,
            url: check.url,
            steps: check.steps,
        }],
    });
    round_trip(&LogChunk {
        offset: 3,
        bytes: b"[31mred[0m".to_vec(),
        next_offset: 15,
        complete: true,
    });
}

#[test]
fn wire_names_are_stable() {
    // The store caches these; renaming a variant breaks existing caches.
    let json = serde_json::to_value(thread()).unwrap();
    assert_eq!(json["resolution"]["state"], "resolved");
    assert_eq!(json["anchor"]["original"]["line"]["side"], "new");
    assert_eq!(json["id"], "PRRT_kwDO");
    assert_eq!(serde_json::to_value(ForgeKind::GitLab).unwrap(), "gitlab");
    assert_eq!(serde_json::to_value(CiState::Canceled).unwrap(), "canceled");
    assert_eq!(serde_json::to_value(PrNumber(7)).unwrap(), 7);
}

#[test]
fn cached_pull_requests_without_counts_still_load() {
    let mut json = serde_json::to_value(pr(1, "head", "main")).unwrap();
    let object = json.as_object_mut().unwrap();
    object.remove("comment_count");
    object.remove("unresolved_threads");
    let back: PullRequest = serde_json::from_value(json).unwrap();
    assert_eq!((back.comment_count, back.unresolved_threads), (None, None));
}

#[test]
fn cached_pull_requests_without_people_still_load() {
    let mut json = serde_json::to_value(pr(1, "head", "main")).unwrap();
    let object = json.as_object_mut().unwrap();
    object.remove("assignees");
    object.remove("milestone");
    for label in object["labels"].as_array_mut().unwrap() {
        label.as_object_mut().unwrap().remove("description");
    }
    let back: PullRequest = serde_json::from_value(json).unwrap();
    assert!(back.assignees.is_empty() && back.milestone.is_none());
    assert_eq!(back.labels[0].description, None);
}

#[test]
fn ci_rollup_prefers_failure_then_activity() {
    use CiState::{Canceled, Fail, None, Pass, Queued, Running, Skipped};
    let cases: [(&[CiState], CiState); 7] = [
        (&[], None),
        (&[Skipped, Skipped], Skipped),
        (&[Pass, Skipped], Pass),
        (&[Pass, Canceled], Canceled),
        (&[Canceled, Queued], Queued),
        (&[Queued, Running, Pass], Running),
        (&[Running, Fail, Queued], Fail),
    ];
    for (states, expected) in cases {
        assert_eq!(
            CiState::rollup(states.iter().copied()),
            expected,
            "{states:?}"
        );
    }
}

#[test]
fn short_sha_and_open_states() {
    assert_eq!(Sha::new("0123456789abcdef").short(), "0123456");
    assert_eq!(Sha::new("abc").short(), "abc");
    assert!(PrState::Draft.is_open() && PrState::Open.is_open());
    assert!(!PrState::Merged.is_open() && !PrState::Closed.is_open());
}

fn numbers(stack: &Stack) -> Vec<u64> {
    stack.links.iter().map(|link| link.number.0).collect()
}

#[test]
fn base_branch_rule_walks_both_ways() {
    let open = [
        pr(38, "pack", "main"),
        pr(39, "tree", "pack"),
        pr(40, "hunk", "tree"),
        pr(41, "split", "hunk"),
        pr(50, "other", "main"),
    ];
    let stack = stack_by_base_branch(&open[2], &open);
    assert_eq!(stack.source, StackSource::BaseBranch);
    assert_eq!(numbers(&stack), [38, 39, 40, 41]);
    assert_eq!(stack.position_of(PrNumber(40)), Some(2));
    assert_eq!(numbers(&stack_by_base_branch(&open[4], &open)), [50]);
}

#[test]
fn base_branch_rule_skips_forks_closed_and_cycles() {
    let mut fork = pr(60, "pack", "main");
    fork.head.repo = Some(forge::model::RepoRef::new(
        "github.com",
        "someone",
        "review-sandbox",
    ));
    let mut closed = pr(61, "tree", "pack");
    closed.state = PrState::Closed;
    let cycle_a = pr(70, "a", "b");
    let cycle_b = pr(71, "b", "a");
    let open = [
        fork,
        closed,
        pr(62, "pack", "main"),
        cycle_a.clone(),
        cycle_b,
    ];

    let child = pr(63, "tree", "pack");
    // The fork's `pack` branch is not this repo's, so 62 is the parent.
    assert_eq!(numbers(&stack_by_base_branch(&child, &open)), [62, 63]);
    assert_eq!(numbers(&stack_by_base_branch(&cycle_a, &open)), [71, 70]);
}

#[test]
fn lifecycle_types_round_trip_through_json() {
    let mut options = MergeOptions::new(MergeMethod::Squash, Sha::new("a".repeat(40)));
    options.commit_title = Some("Title".to_owned());
    options.delete_branch = true;
    round_trip(&options);
    round_trip(&Mergeability {
        mergeable: None,
        reasons: vec![MergeBlocker::Computing, MergeBlocker::ChecksPending],
        auto_merge: true,
    });
    round_trip(&MergeSettings::new(MergeMethod::ALL, MergeMethod::Rebase));
    assert_eq!(
        serde_json::to_string(&MergeBlocker::BranchProtection).unwrap(),
        "\"branch_protection\""
    );
}

#[test]
fn merge_settings_keep_the_default_among_the_allowed_methods() {
    use MergeMethod::{Merge, Rebase, Squash};
    let settings = MergeSettings::new([Rebase, Squash], Merge);
    assert_eq!(settings.allowed, [Squash, Rebase]);
    assert_eq!(
        settings.default, Squash,
        "a default that is not allowed gives way"
    );
    let settings = MergeSettings::new([Rebase, Squash], Rebase);
    assert_eq!(settings.default, Rebase);
    let none = MergeSettings::new([], Squash);
    assert_eq!((none.allowed, none.default), (vec![Merge], Merge));
}

#[test]
fn commit_text_joins_the_title_and_body() {
    let mut options = MergeOptions::new(MergeMethod::Merge, Sha::new("a"));
    assert_eq!(options.commit_text(), None);
    options.commit_message = Some("Body".to_owned());
    assert_eq!(options.commit_text().as_deref(), Some("Body"));
    options.commit_title = Some("Title".to_owned());
    assert_eq!(
        options.commit_text().as_deref(),
        Some(
            "Title

Body"
        )
    );
    options.commit_message = Some(String::new());
    assert_eq!(options.commit_text().as_deref(), Some("Title"));
}

#[test]
fn the_head_branch_of_a_fork_is_not_in_the_base_repository() {
    let mut pull = pr(1, "a", "main");
    assert!(pull.head_in_base_repo());
    pull.head.repo = Some(forge::model::RepoRef::new(
        "github.com",
        "someone",
        "review-sandbox",
    ));
    assert!(!pull.head_in_base_repo());
    pull.head.repo = None;
    assert!(!pull.head_in_base_repo());
    pull.head.repo = Some(forge::model::RepoRef::new(
        "github.com",
        "LOVELACE",
        "Review-Sandbox",
    ));
    assert!(
        pull.head_in_base_repo(),
        "forges answer in their own casing"
    );
}

#[test]
fn only_github_and_gitlab_can_auto_merge() {
    for kind in ForgeKind::ALL {
        let caps = Capabilities::baseline(kind);
        assert!(caps.merge && caps.set_draft && caps.close_reopen, "{kind}");
        assert_eq!(caps.auto_merge, kind != ForgeKind::Forgejo, "{kind}");
    }
}

#[test]
fn reviewers_decide_without_counting_stale_or_withdrawn_approvals() {
    use forge::model::{ReviewDecision, ReviewStatus, Reviewer};

    let row = |state| Reviewer::new(User::new("kit"), state);
    let stale = Reviewer {
        stale: true,
        ..row(ReviewState::Approved)
    };
    let again = Reviewer {
        requested_again: true,
        ..row(ReviewState::Requested)
    };
    let decide = |rows: &[Reviewer]| ReviewDecision::from_reviewers(rows);

    assert_eq!(
        decide(&[row(ReviewState::Approved)]),
        ReviewDecision::Approved
    );
    assert_eq!(
        decide(std::slice::from_ref(&stale)),
        ReviewDecision::StaleApproval
    );
    assert_eq!(
        decide(std::slice::from_ref(&again)),
        ReviewDecision::ReviewRequired
    );
    // One current approval is enough; a change request beats everything.
    assert_eq!(
        decide(&[stale.clone(), row(ReviewState::Approved)]),
        ReviewDecision::Approved
    );
    assert_eq!(
        decide(&[
            row(ReviewState::Approved),
            row(ReviewState::ChangesRequested)
        ]),
        ReviewDecision::ChangesRequested
    );
    assert_eq!(decide(&[]), ReviewDecision::None);

    assert_eq!(stale.status(), ReviewStatus::StaleApproval);
    assert_eq!(again.status(), ReviewStatus::ReReviewRequested);
    assert_eq!(
        row(ReviewState::Requested).status(),
        ReviewStatus::ReviewRequested
    );
    assert_eq!(stale.status().label(), "approved before last push");
    assert_eq!(again.status().label(), "re-review requested");
}

#[test]
fn a_reviewer_cached_before_the_new_fields_still_reads() {
    let old =
        r#"{"user":{"id":null,"login":"kit","name":null,"avatar_url":null},"state":"approved"}"#;
    let row: forge::model::Reviewer = serde_json::from_str(old).unwrap();
    assert!(!row.stale && !row.requested_again);
}
