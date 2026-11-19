mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{DAY, T0, TestDir, key, open, pr, repo, snapshot, ts};
use forge::model::{CiState, CommentId, InboxSection, LineRef, PrState, Sha, Side, ThreadId};
use store::{
    DraftAnchor, FetchMeta, NewQueued, Options, ReviewMark, Reviewed, Seen, Store,
    SubscriptionTarget, ViewedProgress,
};

const HOST: &str = "github.com";

fn secs(n: i64) -> Duration {
    Duration::from_secs(u64::try_from(n).expect("non-negative"))
}

fn titles(prs: &[forge::model::PullRequest]) -> Vec<String> {
    prs.iter().map(|pr| pr.title.clone()).collect()
}

#[test]
fn inbox_round_trip_keeps_order_and_section_identity() {
    let dir = TestDir::new("inbox");
    let (store, clock) = open(&dir);
    let review = InboxSection::ReviewRequested;
    let mine = InboxSection::AuthoredByMe;

    assert_eq!(store.inbox(HOST, &review).wait().unwrap(), None);

    let list = vec![
        pr(7, PrState::Open, T0 - 10),
        pr(3, PrState::Draft, T0 - 20),
        pr(9, PrState::Open, T0 - 30),
    ];
    store.put_inbox(HOST, &review, list.clone()).wait().unwrap();
    store
        .put_inbox(HOST, &mine, vec![pr(3, PrState::Draft, T0 - 20)])
        .wait()
        .unwrap();

    let cached = store.inbox(HOST, &review).wait().unwrap().unwrap();
    assert_eq!(cached.prs, list);
    assert_eq!(cached.fetched_at, ts(T0));
    assert_eq!(
        store.inbox(HOST, &mine).wait().unwrap().unwrap().prs.len(),
        1
    );
    assert_eq!(store.inbox("gitlab.com", &review).wait().unwrap(), None);

    clock.advance(secs(60));
    let shorter = vec![pr(9, PrState::Open, T0 - 30)];
    store
        .put_inbox(HOST, &review, shorter.clone())
        .wait()
        .unwrap();
    let cached = store.inbox(HOST, &review).wait().unwrap().unwrap();
    assert_eq!(cached.prs, shorter);
    assert_eq!(cached.fetched_at, ts(T0 + 60));

    let by_repo = InboxSection::OpenInRepo(repo());
    let query = InboxSection::Query("is:open label:bug".to_owned());
    store.put_inbox(HOST, &by_repo, vec![]).wait().unwrap();
    store
        .put_inbox(HOST, &query, vec![pr(1, PrState::Open, T0)])
        .wait()
        .unwrap();
    assert_eq!(
        store.inbox(HOST, &by_repo).wait().unwrap().unwrap().prs,
        vec![]
    );
    assert_eq!(
        store.inbox(HOST, &query).wait().unwrap().unwrap().prs.len(),
        1
    );
}

#[test]
fn a_long_inbox_keeps_its_order() {
    let dir = TestDir::new("long");
    let (store, _) = open(&dir);
    let section = InboxSection::Query("long".to_owned());
    let list: Vec<_> = (1..=1000u64)
        .rev()
        .map(|n| pr(n, PrState::Open, T0 - i64::try_from(n).unwrap()))
        .collect();
    store
        .put_inbox(HOST, &section, list.clone())
        .wait()
        .unwrap();
    let cached = store.inbox(HOST, &section).wait().unwrap().unwrap();
    assert_eq!(cached.prs, list);
}

#[test]
fn pull_request_rows_ignore_stale_writes_and_keep_unreported_fields() {
    let dir = TestDir::new("pr");
    let (store, _) = open(&dir);
    let section = InboxSection::ReviewRequested;

    let mut fresh = pr(5, PrState::Open, T0);
    fresh.title = "fresh".to_owned();
    store
        .put_inbox(HOST, &section, vec![fresh.clone()])
        .wait()
        .unwrap();

    let mut stale = pr(5, PrState::Open, T0 - 500);
    stale.title = "stale".to_owned();
    store.put_inbox(HOST, &section, vec![stale]).wait().unwrap();
    assert_eq!(
        store.pull_request(&key(5)).wait().unwrap().unwrap().title,
        "fresh"
    );

    // A list endpoint that does not report CI must not erase what a
    // snapshot learned, while the head commit is the same.
    let mut listed = fresh.clone();
    listed.ci = None;
    listed.diff_stat = None;
    listed.updated_at = ts(T0 + 5);
    store
        .put_inbox(HOST, &section, vec![listed.clone()])
        .wait()
        .unwrap();
    let cached = store.pull_request(&key(5)).wait().unwrap().unwrap();
    assert_eq!(cached.ci, Some(CiState::Pass));
    assert_eq!(cached.diff_stat, fresh.diff_stat);
    assert_eq!(cached.updated_at, ts(T0 + 5));

    // A new head commit means the old CI says nothing about it.
    let mut pushed = listed;
    pushed.ci = None;
    pushed.head.sha = Sha::new("f".repeat(40));
    pushed.updated_at = ts(T0 + 10);
    store
        .put_inbox(HOST, &section, vec![pushed])
        .wait()
        .unwrap();
    assert_eq!(
        store.pull_request(&key(5)).wait().unwrap().unwrap().ci,
        None
    );

    assert_eq!(store.pull_request(&key(404)).wait().unwrap(), None);
}

#[test]
fn snapshot_round_trip_records_head_and_refreshes_the_pull_request() {
    let dir = TestDir::new("snapshot");
    let (store, _) = open(&dir);
    assert_eq!(store.snapshot(&key(8)).wait().unwrap(), None);

    let snap = snapshot(8, T0);
    store.put_snapshot(snap.clone()).wait().unwrap();
    let cached = store.snapshot(&key(8)).wait().unwrap().unwrap();
    assert_eq!(cached.snapshot, snap);
    assert_eq!(cached.head_sha, snap.pull_request.head.sha);
    assert_eq!(cached.fetched_at, ts(T0));
    assert_eq!(
        store.pull_request(&key(8)).wait().unwrap().unwrap(),
        snap.pull_request
    );

    let mut pushed = snapshot(8, T0 + 100);
    pushed.pull_request.head.sha = Sha::new("b".repeat(40));
    store.put_snapshot(Arc::new(pushed.clone())).wait().unwrap();
    let cached = store.snapshot(&key(8)).wait().unwrap().unwrap();
    assert_eq!(cached.head_sha.as_str(), "b".repeat(40));
    assert_eq!(cached.snapshot, pushed);
}

#[test]
fn viewed_state_follows_the_blob() {
    let dir = TestDir::new("viewed");
    let (store, clock) = open(&dir);
    let pr = key(2);

    assert!(store.viewed_for(&pr).wait().unwrap().is_empty());
    store
        .set_viewed(&pr, "src/lib.rs", "blob-a", true)
        .wait()
        .unwrap();
    store
        .set_viewed_many(
            &pr,
            vec![
                ("README.md".to_owned(), "blob-r".to_owned()),
                ("src/main.rs".to_owned(), "blob-m".to_owned()),
            ],
            true,
        )
        .wait()
        .unwrap();

    let viewed = store.viewed_for(&pr).wait().unwrap();
    assert_eq!(viewed.len(), 3);
    assert!(viewed.is_viewed("src/lib.rs", "blob-a"));
    assert_eq!(viewed.viewed_at("src/lib.rs", "blob-a"), Some(ts(T0)));
    // The file changed: a new blob id is not viewed.
    assert!(!viewed.is_viewed("src/lib.rs", "blob-b"));
    assert!(!viewed.is_viewed("src/other.rs", "blob-a"));

    // Viewing the new blob keeps the old mark, so a revert is viewed again.
    clock.advance(secs(5));
    store
        .set_viewed(&pr, "src/lib.rs", "blob-b", true)
        .wait()
        .unwrap();
    let viewed = store.viewed_for(&pr).wait().unwrap();
    assert!(viewed.is_viewed("src/lib.rs", "blob-a") && viewed.is_viewed("src/lib.rs", "blob-b"));
    assert_eq!(viewed.viewed_at("src/lib.rs", "blob-b"), Some(ts(T0 + 5)));

    store
        .set_viewed(&pr, "src/lib.rs", "blob-b", false)
        .wait()
        .unwrap();
    let viewed = store.viewed_for(&pr).wait().unwrap();
    assert!(!viewed.is_viewed("src/lib.rs", "blob-b"));
    assert!(viewed.is_viewed("src/lib.rs", "blob-a"));

    assert!(store.viewed_for(&key(3)).wait().unwrap().is_empty());
}

fn line_anchor(path: &str, line: u32) -> DraftAnchor {
    DraftAnchor::Line {
        path: path.to_owned(),
        line: LineRef {
            side: Side::New,
            line,
        },
        start: None,
    }
}

#[test]
fn drafts_round_trip_per_anchor() {
    let dir = TestDir::new("drafts");
    let (store, clock) = open(&dir);
    let pr = key(4);
    let line = line_anchor("a.rs", 10);
    let reply = DraftAnchor::Reply {
        thread: ThreadId::new("T1"),
    };
    let head = Sha::new("c".repeat(40));

    assert_eq!(store.draft(&pr, &line).wait().unwrap(), None);
    store
        .put_draft(&pr, &line, "first", Some(&head))
        .wait()
        .unwrap();
    clock.advance(secs(1));
    store
        .put_draft(&pr, &line, "first, edited", Some(&head))
        .wait()
        .unwrap();
    clock.advance(secs(1));
    store.put_draft(&pr, &reply, "thanks", None).wait().unwrap();
    store
        .put_draft(&pr, &DraftAnchor::Review, "overall", None)
        .wait()
        .unwrap();
    store
        .put_draft(&key(5), &line, "other pr", None)
        .wait()
        .unwrap();

    let got = store.draft(&pr, &line).wait().unwrap().unwrap();
    assert_eq!(got.body, "first, edited");
    assert_eq!(got.commit.as_ref(), Some(&head));
    assert_eq!(got.updated_at, ts(T0 + 1));
    assert_eq!(got.pr, pr);

    let all = store.drafts_for(&pr).wait().unwrap();
    let bodies: Vec<_> = all.iter().map(|d| d.body.as_str()).collect();
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0], "first, edited");

    store.delete_draft(&pr, &line).wait().unwrap();
    assert_eq!(store.draft(&pr, &line).wait().unwrap(), None);
    assert_eq!(store.drafts_for(&pr).wait().unwrap().len(), 2);
    assert_eq!(store.drafts_for(&key(5)).wait().unwrap().len(), 1);
}

fn queued_at(anchor: &DraftAnchor, body: &str, commit: Option<&Sha>) -> NewQueued {
    NewQueued {
        anchor: anchor.clone(),
        body: body.to_owned(),
        commit: commit.cloned(),
        forge_id: None,
        thread: None,
    }
}

#[test]
fn queued_comments_keep_their_forge_ids_and_can_be_edited_or_deleted() {
    let dir = TestDir::new("queued-forge");
    let (store, _clock) = open(&dir);
    let pr = key(4);
    let line = line_anchor("a.rs", 10);
    store
        .put_draft(&pr, &line, "draft on the same line", None)
        .wait()
        .unwrap();
    let on_forge = |id: &str, body: &str| NewQueued {
        forge_id: Some(CommentId::new(id)),
        thread: Some(ThreadId::new(format!("T_{id}"))),
        ..queued_at(&line, body, None)
    };
    let mine = store
        .queue_comment(&pr, on_forge("C_1", "mine"))
        .wait()
        .unwrap();
    assert_eq!(mine.forge_id, Some(CommentId::new("C_1")));
    assert_eq!(mine.thread, Some(ThreadId::new("T_C_1")));

    store
        .put_draft(&pr, &line, "typed again", None)
        .wait()
        .unwrap();
    // The forge's pending review also has one made on the web: it is added
    // once, the known one is not doubled, and the draft stays.
    let all = store
        .adopt_queued(
            &pr,
            vec![on_forge("C_1", "mine"), on_forge("C_2", "from the web")],
        )
        .wait()
        .unwrap();
    let bodies: Vec<_> = all.iter().map(|q| q.body.as_str()).collect();
    assert_eq!(bodies, ["mine", "from the web"]);
    let again = store
        .adopt_queued(&pr, vec![on_forge("C_2", "from the web")])
        .wait()
        .unwrap();
    assert_eq!(again.len(), 2);
    assert!(store.draft(&pr, &line).wait().unwrap().is_some());

    store
        .update_queued(mine.id, "mine, better", None)
        .wait()
        .unwrap();
    store
        .update_queued(all[1].id, "re-added", Some(&CommentId::new("C_3")))
        .wait()
        .unwrap();
    let edited = store.queued_for(&pr).wait().unwrap();
    assert_eq!(edited[0].body, "mine, better");
    assert_eq!(edited[0].forge_id, Some(CommentId::new("C_1")));
    assert_eq!(edited[1].forge_id, Some(CommentId::new("C_3")));

    store.delete_queued(mine.id).wait().unwrap();
    let left = store.queued_for(&pr).wait().unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].body, "re-added");
}

#[test]
fn queuing_a_comment_moves_it_from_drafts_to_the_pending_review() {
    let dir = TestDir::new("queued");
    let (store, clock) = open(&dir);
    let pr = key(4);
    let line = line_anchor("a.rs", 10);
    let head = Sha::new("c".repeat(40));
    store
        .put_draft(&pr, &line, "needs a test", Some(&head))
        .wait()
        .unwrap();
    store
        .put_draft(&pr, &DraftAnchor::Review, "summary", None)
        .wait()
        .unwrap();
    store
        .put_draft(&pr, &DraftAnchor::Conversation, "hello", None)
        .wait()
        .unwrap();

    clock.advance(secs(5));
    let queued = store
        .queue_comment(&pr, queued_at(&line, "needs a test", Some(&head)))
        .wait()
        .unwrap();
    // A second comment on the same line is its own entry.
    store
        .queue_comment(&pr, queued_at(&line, "and docs", None))
        .wait()
        .unwrap();
    store
        .queue_comment(&key(5), queued_at(&line, "other pr", None))
        .wait()
        .unwrap();
    assert_eq!(queued.queued_at, ts(T0 + 5));

    assert_eq!(store.draft(&pr, &line).wait().unwrap(), None);
    let pending = store.queued_for(&pr).wait().unwrap();
    let bodies: Vec<_> = pending.iter().map(|q| q.body.as_str()).collect();
    assert_eq!(bodies, ["needs a test", "and docs"]);
    assert_eq!(pending[0], queued);
    assert_eq!(pending[0].commit.as_ref(), Some(&head));
    assert_eq!(pending[0].anchor, line);

    store.finish_review(&pr).wait().unwrap();
    assert_eq!(store.queued_for(&pr).wait().unwrap(), vec![]);
    assert_eq!(store.queued_for(&key(5)).wait().unwrap().len(), 1);
    let left: Vec<_> = store
        .drafts_for(&pr)
        .wait()
        .unwrap()
        .into_iter()
        .map(|d| d.anchor)
        .collect();
    assert_eq!(left, [DraftAnchor::Conversation]);
}

#[test]
fn drafts_viewed_and_subscriptions_survive_a_reopen() {
    let dir = TestDir::new("reopen");
    let anchor = line_anchor("lib.rs", 3);
    let target = SubscriptionTarget::pull_request(&key(6));
    {
        let (store, _) = open(&dir);
        // Fire and forget, as the composer does on every keystroke; closing
        // the store must still flush them.
        store
            .put_draft(&key(6), &anchor, "typed just before exit", None)
            .detach();
        store.set_viewed(&key(6), "lib.rs", "blob", true).detach();
        store.put_subscription(&target, Some("running")).detach();
        store
            .put_inbox(
                HOST,
                &InboxSection::AuthoredByMe,
                vec![pr(6, PrState::Open, T0)],
            )
            .detach();
    }
    let (store, _) = open(&dir);
    assert_eq!(
        store.draft(&key(6), &anchor).wait().unwrap().unwrap().body,
        "typed just before exit"
    );
    assert!(
        store
            .viewed_for(&key(6))
            .wait()
            .unwrap()
            .is_viewed("lib.rs", "blob")
    );
    let subs = store.subscriptions().wait().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].target, target);
    assert_eq!(subs[0].state.as_deref(), Some("running"));
    let cached = store
        .inbox(HOST, &InboxSection::AuthoredByMe)
        .wait()
        .unwrap()
        .unwrap();
    assert_eq!(titles(&cached.prs), ["Pull request 6"]);
    assert!(store.moved_aside().is_none());
}

#[test]
fn subscriptions_round_trip() {
    let dir = TestDir::new("subs");
    let (store, clock) = open(&dir);
    let pr_target = SubscriptionTarget::pull_request(&key(1));
    let run_target = SubscriptionTarget::Run {
        repo: repo(),
        run: forge::model::RunId::new("run-9"),
    };
    store.put_subscription(&pr_target, None).wait().unwrap();
    clock.advance(secs(10));
    store
        .put_subscription(&run_target, Some("queued"))
        .wait()
        .unwrap();
    clock.advance(secs(10));
    store
        .put_subscription(&pr_target, Some("pass"))
        .wait()
        .unwrap();

    let subs = store.subscriptions().wait().unwrap();
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].target, pr_target);
    assert_eq!(subs[0].created_at, ts(T0));
    assert_eq!(subs[0].state.as_deref(), Some("pass"));
    assert_eq!(subs[1].target, run_target);

    store.delete_subscription(&pr_target).wait().unwrap();
    let subs = store.subscriptions().wait().unwrap();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].target, run_target);
}

#[test]
fn fetch_meta_round_trip() {
    let dir = TestDir::new("meta");
    let (store, clock) = open(&dir);
    assert_eq!(store.meta("github.com inbox").wait().unwrap(), None);

    let meta = FetchMeta {
        etag: Some("W/\"abc\"".to_owned()),
        last_modified: Some("Tue, 17 Nov 2026 00:00:00 GMT".to_owned()),
        watermark: Some(ts(T0 - 99)),
    };
    store.set_meta("github.com inbox", &meta).wait().unwrap();
    let stored = store.meta("github.com inbox").wait().unwrap().unwrap();
    assert_eq!(stored.meta, meta);
    assert_eq!(stored.fetched_at, ts(T0));

    clock.advance(secs(30));
    store
        .set_meta("github.com inbox", &FetchMeta::default())
        .wait()
        .unwrap();
    let stored = store.meta("github.com inbox").wait().unwrap().unwrap();
    assert_eq!(stored.meta, FetchMeta::default());
    assert_eq!(stored.fetched_at, ts(T0 + 30));
    assert_eq!(store.meta("other").wait().unwrap(), None);
}

fn user_version(path: &std::path::Path) -> i64 {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn an_empty_file_is_migrated_to_the_current_schema() {
    let dir = TestDir::new("empty");
    std::fs::write(dir.db(), b"").unwrap();
    let (store, _) = open(&dir);
    assert!(store.moved_aside().is_none());
    store.set_meta("e", &FetchMeta::default()).wait().unwrap();
    drop(store);

    assert!(user_version(&dir.db()) >= 1);
    let conn = rusqlite::Connection::open(dir.db()).unwrap();
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let vacuum: i64 = conn
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))
        .unwrap();
    assert_eq!(vacuum, 2, "incremental auto-vacuum is chosen at creation");
    for table in [
        "pull_requests",
        "inbox_sections",
        "inbox_items",
        "snapshots",
        "viewed_files",
        "drafts",
        "subscriptions",
        "fetch_meta",
        "review_marks",
    ] {
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(found, 1, "{table}");
    }
}

#[test]
fn a_missing_directory_and_file_are_created() {
    let dir = TestDir::new("fresh");
    let nested = dir.path().join("a/b/cache.db");
    let store = Store::open(&nested).unwrap();
    store.set_meta("x", &FetchMeta::default()).wait().unwrap();
    assert!(nested.exists());
}

fn aside_files(dir: &TestDir) -> Vec<String> {
    let mut names: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "cache.db" && !name.ends_with("-wal") && !name.ends_with("-shm"))
        .collect();
    names.sort();
    names
}

#[test]
fn a_corrupt_file_is_moved_aside_and_replaced() {
    let dir = TestDir::new("corrupt");
    let garbage: Vec<u8> = (0..8192u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.db(), &garbage).unwrap();

    let (store, _) = open(&dir);
    let aside = store.moved_aside().expect("moved aside").to_owned();
    let name = aside.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("cache.db.corrupt-"), "{name}");
    // The fake clock's time is in the name, and the old bytes are intact.
    assert!(name.contains("20261117T"), "{name}");
    assert_eq!(std::fs::read(&aside).unwrap(), garbage);
    assert_eq!(aside_files(&dir), [name]);

    store
        .put_draft(&key(1), &DraftAnchor::Review, "after", None)
        .wait()
        .unwrap();
    assert_eq!(store.drafts_for(&key(1)).wait().unwrap().len(), 1);
}

#[test]
fn damaged_pages_are_caught_by_the_integrity_check() {
    let dir = TestDir::new("damaged");
    {
        let (store, _) = open(&dir);
        let list: Vec<_> = (1..=50)
            .map(|n| pr(n, PrState::Open, T0 - i64::try_from(n).unwrap()))
            .collect();
        store
            .put_inbox(HOST, &InboxSection::ReviewRequested, list)
            .wait()
            .unwrap();
    }
    // Overwrite the middle of the file, leaving the header readable.
    let mut bytes = std::fs::read(dir.db()).unwrap();
    assert!(bytes.len() > 20_000, "{}", bytes.len());
    for byte in &mut bytes[8192..16384] {
        *byte = 0xA5;
    }
    std::fs::write(dir.db(), &bytes).unwrap();
    let wal = dir.path().join("cache.db-wal");
    let _ = std::fs::remove_file(&wal);

    let (store, _) = open(&dir);
    assert!(store.moved_aside().is_some());
    assert_eq!(
        store
            .inbox(HOST, &InboxSection::ReviewRequested)
            .wait()
            .unwrap(),
        None
    );
}

#[test]
fn a_newer_database_is_kept_aside_untouched() {
    let dir = TestDir::new("newer");
    {
        let conn = rusqlite::Connection::open(dir.db()).unwrap();
        conn.execute_batch("CREATE TABLE future (a); INSERT INTO future VALUES (42);")
            .unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
    }
    let (store, _) = open(&dir);
    let aside = store.moved_aside().expect("moved aside").to_owned();
    assert!(
        aside
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("cache.db.newer-"),
        "{aside:?}"
    );
    assert_eq!(user_version(&aside), 999);
    let conn = rusqlite::Connection::open(&aside).unwrap();
    let value: i64 = conn
        .query_row("SELECT a FROM future", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 42);
    assert!(user_version(&dir.db()) < 999);
    drop(store);
}

#[test]
fn two_moves_in_the_same_second_do_not_overwrite_each_other() {
    let dir = TestDir::new("twice");
    for round in 0..2u8 {
        std::fs::write(dir.db(), vec![round + 1; 4096]).unwrap();
        let (store, _) = open(&dir);
        assert!(store.moved_aside().is_some());
        drop(store);
        std::fs::remove_file(dir.db()).unwrap();
        for suffix in ["-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.path().join(format!("cache.db{suffix}")));
        }
    }
    assert_eq!(aside_files(&dir).len(), 2);
}

#[test]
fn pruning_drops_old_closed_pull_requests_by_the_fake_clock() {
    let dir = TestDir::new("prune");
    let (store, clock) = open(&dir);
    let listed = InboxSection::Query("is:closed".to_owned());

    // 1: closed, only ever seen through a snapshot, with viewed marks and a draft.
    let mut closed = snapshot(1, T0 - 5 * DAY);
    closed.pull_request.state = PrState::Merged;
    store.put_snapshot(closed).wait().unwrap();
    store
        .set_viewed(&key(1), "a.rs", "blob", true)
        .wait()
        .unwrap();
    store
        .put_draft(&key(1), &DraftAnchor::Review, "keep me", None)
        .wait()
        .unwrap();
    // 2: open, snapshot only: never pruned however old.
    store.put_snapshot(snapshot(2, T0)).wait().unwrap();
    // 3: closed and listed by a section that stays fresh.
    store
        .put_inbox(HOST, &listed, vec![pr(3, PrState::Closed, T0 - DAY)])
        .wait()
        .unwrap();

    clock.advance(secs(29 * DAY));
    let report = store.maintain().wait().unwrap();
    assert_eq!(
        (report.pruned_pull_requests, report.pruned_sections),
        (0, 0)
    );
    assert!(store.pull_request(&key(1)).wait().unwrap().is_some());

    // The section is refreshed on day 29; PR 1 was last seen on day 0.
    store
        .put_inbox(HOST, &listed, vec![pr(3, PrState::Closed, T0 - DAY)])
        .wait()
        .unwrap();
    clock.advance(secs(2 * DAY));
    let report = store.maintain().wait().unwrap();
    assert_eq!(report.pruned_pull_requests, 1);
    assert_eq!(report.pruned_sections, 0);

    assert_eq!(store.pull_request(&key(1)).wait().unwrap(), None);
    assert_eq!(store.snapshot(&key(1)).wait().unwrap(), None);
    assert!(store.viewed_for(&key(1)).wait().unwrap().is_empty());
    assert_eq!(
        store.drafts_for(&key(1)).wait().unwrap().len(),
        1,
        "drafts are user data"
    );
    assert!(store.snapshot(&key(2)).wait().unwrap().is_some());
    assert!(store.pull_request(&key(3)).wait().unwrap().is_some());
    assert_eq!(
        store
            .inbox(HOST, &listed)
            .wait()
            .unwrap()
            .unwrap()
            .prs
            .len(),
        1
    );

    // Nobody refreshed the section for 30 more days: it and its closed PR go.
    clock.advance(secs(31 * DAY));
    let report = store.maintain().wait().unwrap();
    assert_eq!(
        (report.pruned_sections, report.pruned_pull_requests),
        (1, 1)
    );
    assert_eq!(store.inbox(HOST, &listed).wait().unwrap(), None);
    assert_eq!(store.pull_request(&key(3)).wait().unwrap(), None);
    assert!(store.pull_request(&key(2)).wait().unwrap().is_some());
}

#[test]
fn a_database_over_its_size_limit_is_trimmed_on_open() {
    let dir = TestDir::new("size");
    let clock = Arc::new(store::ManualClock::new(ts(T0)));
    let options = |max_bytes| Options {
        clock: clock.clone(),
        max_bytes,
        ..Options::default()
    };
    {
        let store = Store::open_with(dir.db(), options(u64::MAX)).unwrap();
        for n in 1..=150 {
            let mut snap = snapshot(n, T0);
            snap.pull_request.state = PrState::Closed;
            snap.pull_request.body = "x".repeat(2_000);
            store.put_snapshot(snap).wait().unwrap();
            clock.advance(Duration::from_secs(1));
        }
        let report = store.maintain().wait().unwrap();
        assert_eq!(report.trimmed_snapshots, 0);
        assert!(report.bytes_after > 300_000, "{}", report.bytes_after);
    }

    let limit = 120_000;
    let store = Store::open_with(dir.db(), options(limit)).unwrap();
    // The startup pass is queued ahead of this one; the second finds nothing to do.
    let report = store.maintain().wait().unwrap();
    assert_eq!(report.trimmed_snapshots, 0);
    assert!(report.bytes_after <= limit, "{}", report.bytes_after);
    assert!(
        store.snapshot(&key(1)).wait().unwrap().is_none(),
        "the oldest snapshot goes first"
    );
}

#[test]
fn reads_see_whole_transactions_while_the_writer_is_busy() {
    let dir = TestDir::new("concurrent");
    let (store, _) = open(&dir);
    let section = InboxSection::ReviewRequested;
    let generation = |g: usize| -> Vec<_> {
        (1..=60u64)
            .map(|n| {
                let mut p = pr(n, PrState::Open, T0 + i64::try_from(g).unwrap());
                p.title = format!("gen-{g}");
                p
            })
            .collect()
    };
    store
        .put_inbox(HOST, &section, generation(0))
        .wait()
        .unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let writer = {
        let (store, done) = (store.clone(), done.clone());
        let section = section.clone();
        std::thread::spawn(move || {
            for g in 1..=150 {
                store
                    .put_inbox(HOST, &section, generation(g))
                    .wait()
                    .unwrap();
                store
                    .put_draft(&key(1), &DraftAnchor::Review, &format!("v{g}"), None)
                    .wait()
                    .unwrap();
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (store, done, section) = (store.clone(), done.clone(), section.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                let mut last_gen = 0;
                while !done.load(Ordering::SeqCst) {
                    let cached = store.inbox(HOST, &section).wait().unwrap().unwrap();
                    assert_eq!(cached.prs.len(), 60);
                    let first = cached.prs[0].title.clone();
                    assert!(cached.prs.iter().all(|p| p.title == first), "torn read");
                    let g: usize = first.trim_start_matches("gen-").parse().unwrap();
                    assert!(g >= last_gen, "went back in time");
                    last_gen = g;
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    writer.join().unwrap();
    for reader in readers {
        assert!(reader.join().unwrap() > 0);
    }
    let last = store.inbox(HOST, &section).wait().unwrap().unwrap();
    assert_eq!(last.prs[0].title, "gen-150");
}

#[test]
fn pending_can_be_polled_without_blocking() {
    let dir = TestDir::new("poll");
    let (store, _) = open(&dir);
    let mut pending = store.inbox(HOST, &InboxSection::AuthoredByMe);
    let result = loop {
        if let Some(result) = pending.try_take() {
            break result;
        }
        std::thread::yield_now();
    };
    assert_eq!(result.unwrap(), None);
}

fn full_mark() -> ReviewMark {
    ReviewMark {
        seen: Some(Seen {
            head: "abc1234".to_owned(),
            comments: 4,
            reviews: 2,
            at: ts(T0 - 60),
        }),
        unread: false,
        reviewed: Some(Reviewed {
            head: "abc1234".to_owned(),
            at: ts(T0 - 30),
        }),
        progress: Some(ViewedProgress {
            head: "abc1234".to_owned(),
            viewed: 12,
            total: 23,
        }),
    }
}

#[test]
fn review_marks_round_trip_and_an_empty_mark_is_removed() {
    let dir = TestDir::new("marks");
    let (store, _) = open(&dir);
    assert!(store.marks().wait().unwrap().is_empty());

    let nested = store::PrKey::new(
        forge::model::RepoRef::new("gitlab.com", "group/sub", "proj"),
        forge::model::PrNumber(9),
    );
    let unread = ReviewMark {
        unread: true,
        ..ReviewMark::default()
    };
    store.put_mark(&key(3), full_mark()).wait().unwrap();
    store.put_mark(&nested, unread.clone()).wait().unwrap();
    let mut marks = store.marks().wait().unwrap();
    marks.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(marks, [(key(3), full_mark()), (nested.clone(), unread)]);

    // Replacing keeps one row; clearing every mark drops it.
    let only_progress = ReviewMark {
        progress: full_mark().progress,
        ..ReviewMark::default()
    };
    store
        .put_mark(&key(3), only_progress.clone())
        .wait()
        .unwrap();
    store
        .put_mark(&nested, ReviewMark::default())
        .wait()
        .unwrap();
    assert_eq!(store.marks().wait().unwrap(), [(key(3), only_progress)]);
}

#[test]
fn review_marks_survive_a_reopen() {
    let dir = TestDir::new("marks-reopen");
    {
        let (store, _) = open(&dir);
        store.put_mark(&key(5), full_mark()).detach();
    }
    let (store, _) = open(&dir);
    assert_eq!(store.marks().wait().unwrap(), [(key(5), full_mark())]);
}
