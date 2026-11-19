#![allow(dead_code, reason = "each test binary uses a different subset")]

use chrono::{TimeZone, Utc};
use forge::model::{
    BranchRef, CiState, DiffStat, Label, PrNumber, PrState, PullRequest, PullRequestId, RepoRef,
    ReviewDecision, ReviewState, Reviewer, Sha, Timestamp, User,
};

pub fn repo() -> RepoRef {
    RepoRef::new("github.com", "Lovelace", "review-sandbox")
}

pub fn at(minute: u32) -> Timestamp {
    Utc.with_ymd_and_hms(2026, 11, 16, 12, minute, 0).unwrap()
}

/// A pull request in `repo()` from `head` onto `base`.
pub fn pr(number: u64, head: &str, base: &str) -> PullRequest {
    let repo = repo();
    PullRequest {
        id: PullRequestId::new(format!("PR_{number}")),
        repo: repo.clone(),
        number: PrNumber(number),
        title: format!("Change {number}"),
        body: "Body with a ```suggestion\nfence\n```".to_owned(),
        author: User::new("cora"),
        state: PrState::Open,
        base: BranchRef {
            repo: Some(repo.clone()),
            name: base.to_owned(),
            sha: Sha::new("a".repeat(40)),
        },
        head: BranchRef {
            repo: Some(repo),
            name: head.to_owned(),
            sha: Sha::new("b".repeat(40)),
        },
        merge_base: None,
        created_at: at(0),
        updated_at: at(5),
        review_decision: ReviewDecision::ReviewRequired,
        reviewers: vec![Reviewer::new(User::new("mira"), ReviewState::Requested)],
        ci: Some(CiState::Running),
        labels: vec![Label {
            name: "stack".to_owned(),
            color: Some("ededed".to_owned()),
            description: None,
        }],
        assignees: vec![User::new("cora")],
        milestone: None,
        diff_stat: Some(DiffStat {
            additions: 10,
            deletions: 2,
            changed_files: 3,
        }),
        comment_count: Some(4),
        unresolved_threads: Some(1),
        web_url: format!("https://github.com/Lovelace/review-sandbox/pull/{number}"),
        head_writable: true,
    }
}
