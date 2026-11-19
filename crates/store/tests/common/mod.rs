#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use forge::model::{
    BranchRef, CheckRun, CheckRunId, CiState, DiffStat, Label, PrNumber, PrState, PullRequest,
    PullRequestId, RepoRef, ReviewDecision, Sha, Snapshot, Thread, Timestamp, User,
};
use store::{ManualClock, Options, Store};

/// A directory under the workspace's `.scratch/` (never the system temp
/// dir), removed on drop.
pub struct TestDir(PathBuf);

impl TestDir {
    pub fn new(name: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/store-tests");
        let dir = root.join(format!("{name}-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        Self(dir)
    }

    pub fn db(&self) -> PathBuf {
        self.0.join("cache.db")
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn ts(secs: i64) -> Timestamp {
    Timestamp::from_timestamp(secs, 0).expect("valid timestamp")
}

/// 2026-11-17 00:00:00 UTC.
pub const T0: i64 = 1_794_873_600;

pub const DAY: i64 = 24 * 60 * 60;

pub fn repo() -> RepoRef {
    RepoRef::new("github.com", "acme", "widgets")
}

pub fn pr(number: u64, state: PrState, updated: i64) -> PullRequest {
    let branch = |name: &str, sha: &str| BranchRef {
        repo: Some(repo()),
        name: name.to_owned(),
        sha: Sha::new(sha),
    };
    PullRequest {
        id: PullRequestId::new(format!("PR_{number}")),
        repo: repo(),
        number: PrNumber(number),
        title: format!("Pull request {number}"),
        body: "Fixes the thing.\n\nLonger description with some `markdown` and a list:\n- one\n- two\n- three\n"
            .repeat(4),
        author: User::new("octocat"),
        state,
        base: branch("main", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
        head: branch(&format!("feature-{number}"), &format!("{number:040x}")),
        merge_base: Some(Sha::new("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")),
        created_at: ts(updated - 1000),
        updated_at: ts(updated),
        review_decision: ReviewDecision::ReviewRequired,
        reviewers: vec![],
        assignees: vec![],
        milestone: None,
        ci: Some(CiState::Pass),
        comment_count: None,
        unresolved_threads: None,
        labels: vec![Label {
            name: "bug".to_owned(),
            color: Some("d73a4a".to_owned()),
            description: None,
        }],
        diff_stat: Some(DiffStat {
            additions: 10,
            deletions: 2,
            changed_files: 3,
        }),
        web_url: format!("https://github.com/acme/widgets/pull/{number}"),
        head_writable: false,
    }
}

pub fn snapshot(number: u64, updated: i64) -> Snapshot {
    Snapshot {
        pull_request: pr(number, PrState::Open, updated),
        threads: Vec::<Thread>::new(),
        checks: vec![CheckRun {
            id: CheckRunId::new("check-1"),
            name: "build".to_owned(),
            group: None,
            state: CiState::Pass,
            url: None,
            job: None,
            run: None,
            started_at: None,
            completed_at: None,
            steps: vec![],
        }],
    }
}

pub fn key(number: u64) -> store::PrKey {
    store::PrKey::new(repo(), PrNumber(number))
}

/// A store on a manual clock starting at [`T0`].
pub fn open(dir: &TestDir) -> (Store, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::new(ts(T0)));
    let store = Store::open_with(
        dir.db(),
        Options {
            clock: clock.clone(),
            ..Options::default()
        },
    )
    .expect("open store");
    (store, clock)
}
