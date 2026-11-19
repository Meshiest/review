//! Latency of the paths the UI waits on. Ignored by default; run in release,
//! one benchmark at a time:
//!
//! `cargo test -p store --release --test bench -- --ignored --nocapture`

mod common;

use std::time::Instant;

use common::{T0, TestDir, key, pr, snapshot};
use forge::model::{
    Anchor, Comment, CommentId, InboxSection, LineRef, Position, PrState, Resolution, Side, Thread,
    ThreadId, User,
};
use perf::Samples;
use store::Store;

const SAMPLES: usize = 300;
const WARMUP: usize = 20;
const INBOX_BUDGET_MS: f64 = 5.0;
/// One section of 2,000 rows is beyond what a forge call returns (100), so it
/// is reported against a looser bound than the inbox budget.
const WORST_CASE_INBOX_BUDGET_MS: f64 = 20.0;
const SNAPSHOT_WRITE_BUDGET_MS: f64 = 50.0;

fn prs(range: std::ops::RangeInclusive<u64>) -> Vec<forge::model::PullRequest> {
    range
        .map(|n| pr(n, PrState::Open, T0 - i64::try_from(n).unwrap()))
        .collect()
}

fn thread(n: u64) -> Thread {
    let comment = |i: u64| Comment {
        id: CommentId::new(format!("C{n}-{i}")),
        author: User::new("reviewer"),
        body: "Could this take a slice instead? It would avoid the allocation.\n".repeat(3),
        created_at: common::ts(T0),
        edited_at: None,
        web_url: None,
        review: None,
        verdict: None,
        suggestions: Vec::new(),
    };
    let line = LineRef {
        side: Side::New,
        line: u32::try_from(n).unwrap(),
    };
    Thread {
        id: ThreadId::new(format!("T{n}")),
        anchor: Some(Anchor {
            path: format!("src/module_{}/file.rs", n % 12),
            old_path: None,
            position: Some(Position {
                line,
                start: None,
                commit: None,
            }),
            original: None,
        }),
        resolution: Resolution::Unresolved,
        outdated: false,
        comments: vec![comment(0), comment(1), comment(2)],
    }
}

fn measure(mut run: impl FnMut()) -> Samples {
    for _ in 0..WARMUP {
        run();
    }
    let mut samples = Samples::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let start = Instant::now();
        run();
        samples.push(start.elapsed());
    }
    samples
}

#[test]
#[ignore = "benchmark; run in release under the bench lock"]
fn store_latency() {
    let dir = TestDir::new("bench");
    let store = Store::open(dir.db()).unwrap();
    let host = "github.com";

    // 2,000 cached pull requests: twenty sections of 100 (the forge's limit
    // per call), plus one section listing all 2,000.
    let all = prs(1..=2000);
    for (index, chunk) in all.chunks(100).enumerate() {
        let section = InboxSection::Query(format!("section {index}"));
        store
            .put_inbox(host, &section, chunk.to_vec())
            .wait()
            .unwrap();
    }
    let everything = InboxSection::Query("everything".to_owned());
    store.put_inbox(host, &everything, all).wait().unwrap();

    let first_hundred = InboxSection::Query("section 0".to_owned());
    measure(|| {
        let cached = store.inbox(host, &first_hundred).wait().unwrap().unwrap();
        assert_eq!(cached.prs.len(), 100);
    })
    .report("store_inbox_read_100_of_2000", 99.0, INBOX_BUDGET_MS)
    .with("pull_requests_cached", 2000)
    .print();

    measure(|| {
        let cached = store.inbox(host, &everything).wait().unwrap().unwrap();
        assert_eq!(cached.prs.len(), 2000);
    })
    .report("store_inbox_read_2000", 99.0, WORST_CASE_INBOX_BUDGET_MS)
    .with("pull_requests_cached", 2000)
    .print();

    let mut snap = snapshot(1, T0);
    snap.threads = (1..=60).map(thread).collect();
    let json_bytes = serde_json::to_string(&snap).unwrap().len();
    measure(|| {
        store.put_snapshot(snap.clone()).wait().unwrap();
    })
    .report("store_snapshot_write", 99.0, SNAPSHOT_WRITE_BUDGET_MS)
    .with("snapshot_json_bytes", json_bytes)
    .print();

    measure(|| {
        let cached = store.snapshot(&key(1)).wait().unwrap().unwrap();
        assert_eq!(cached.snapshot.threads.len(), 60);
    })
    .report("store_snapshot_read", 99.0, SNAPSHOT_WRITE_BUDGET_MS)
    .with("snapshot_json_bytes", json_bytes)
    .print();
}
