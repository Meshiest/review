use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, unbounded};
use forge::model::{CommentId, InboxSection, PullRequest, Sha, Snapshot};
use rusqlite::Connection;

use crate::clock::{Clock, SystemClock};
use crate::db;
use crate::draft::{self, Draft, DraftAnchor, NewQueued, QueuedComment};
use crate::error::Result;
use crate::inbox::{self, CachedInbox};
use crate::keys::PrKey;
use crate::maintenance::{self, Limits, MaintenanceReport};
use crate::marks::{self, ReviewMark};
use crate::meta::{self, FetchMeta, StoredFetchMeta};
use crate::pending::Pending;
use crate::snapshot::{self, CachedSnapshot};
use crate::subscription::{self, Subscription, SubscriptionTarget};
use crate::viewed::{self, ViewedSet};

type WriteJob = Box<dyn FnOnce(&mut Connection, i64) + Send>;
type ReadJob = Box<dyn FnOnce(&Connection) + Send>;

#[derive(Clone)]
pub struct Options {
    /// Time source for write stamps, retention and moved-aside file names.
    pub clock: Arc<dyn Clock>,
    /// Read connections, each on its own thread.
    pub read_connections: usize,
    /// Closed or merged pull requests unseen for this long are dropped.
    pub retention: Duration,
    /// Over this many bytes of pages, startup maintenance drops snapshots
    /// (oldest first) until the database fits.
    pub max_bytes: u64,
    /// Files up to this size get an integrity check when opened; bigger
    /// ones are trusted until a read fails.
    pub quick_check_max_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            clock: Arc::new(SystemClock),
            read_connections: 3,
            retention: Duration::from_secs(30 * 24 * 60 * 60),
            max_bytes: 512 * 1024 * 1024,
            quick_check_max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// The local cache. Cheap to clone; every clone shares the same threads.
///
/// Writes go through one writer thread, one transaction per call, in the
/// order they were issued. Reads run on a small pool of read-only
/// connections and see the last committed state. Every method returns a
/// [`Pending`] at once, so the UI thread never touches a connection. To
/// read what you just wrote, await the write first.
///
/// When the last clone is dropped it waits for queued writes to finish.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    path: PathBuf,
    moved_aside: Option<PathBuf>,
    writer: Option<Sender<WriteJob>>,
    readers: Option<Sender<ReadJob>>,
    limits: Limits,
    threads: Vec<JoinHandle<()>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.writer.take();
        self.readers.take();
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

impl Store {
    /// Opens or creates the database at `path` with default [`Options`].
    /// Blocks while it migrates (call it at startup, off the paint path); a
    /// file that is corrupt or from a newer version is renamed aside first.
    ///
    /// # Errors
    /// The file or its directory cannot be created, opened or migrated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with(path, Options::default())
    }

    /// # Errors
    /// As [`Self::open`].
    pub fn open_with(path: impl AsRef<Path>, options: Options) -> Result<Self> {
        let path = path.as_ref().to_owned();
        if let Some(dir) = path.parent().filter(|dir| !dir.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let (writer_conn, moved_aside) =
            db::open_or_replace(&path, options.clock.as_ref(), options.quick_check_max_bytes)?;
        let readers = (0..options.read_connections.max(1))
            .map(|_| db::open_reader(&path))
            .collect::<Result<Vec<_>>>()?;

        let (write_tx, write_rx) = unbounded::<WriteJob>();
        let (read_tx, read_rx) = unbounded::<ReadJob>();
        let mut threads = Vec::with_capacity(readers.len() + 1);
        threads.push(spawn("store-writer", {
            let clock = Arc::clone(&options.clock);
            move || run_writer(writer_conn, &write_rx, clock.as_ref())
        })?);
        for (index, conn) in readers.into_iter().enumerate() {
            let jobs = read_rx.clone();
            threads.push(spawn(&format!("store-reader-{index}"), move || {
                run_reader(&conn, &jobs);
            })?);
        }

        let store = Self {
            inner: Arc::new(Inner {
                path,
                moved_aside,
                writer: Some(write_tx),
                readers: Some(read_tx),
                limits: Limits {
                    retention: options.retention,
                    max_bytes: options.max_bytes,
                },
                threads,
            }),
        };
        store.maintain().detach();
        Ok(store)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Where the previous database file went when it was corrupt or newer
    /// than this build, if that happened on open.
    #[must_use]
    pub fn moved_aside(&self) -> Option<&Path> {
        self.inner.moved_aside.as_deref()
    }

    fn write<T: Send + 'static>(
        &self,
        job: impl FnOnce(&mut Connection, i64) -> Result<T> + Send + 'static,
    ) -> Pending<T> {
        let (reply, pending) = Pending::channel();
        if let Some(writer) = &self.inner.writer {
            // A failed send drops the job and with it `reply`, which the
            // `Pending` reports as a shutdown.
            let _ = writer.send(Box::new(move |conn, now| {
                let _ = reply.send(job(conn, now));
            }));
        }
        pending
    }

    fn read<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Connection) -> Result<T> + Send + 'static,
    ) -> Pending<T> {
        let (reply, pending) = Pending::channel();
        if let Some(readers) = &self.inner.readers {
            let _ = readers.send(Box::new(move |conn| {
                if !reply.is_closed() {
                    let _ = reply.send(job(conn));
                }
            }));
        }
        pending
    }

    /// Replaces the list of one inbox section of `host` and stores each
    /// pull request in it.
    pub fn put_inbox(
        &self,
        host: &str,
        section: &InboxSection,
        prs: impl Into<Arc<[PullRequest]>>,
    ) -> Pending<()> {
        let (host, section, prs) = (host.to_owned(), section.clone(), prs.into());
        self.write(move |conn, now| inbox::put_inbox(conn, now, &host, &section, &prs))
    }

    /// The section as last stored; `None` if it never was.
    pub fn inbox(&self, host: &str, section: &InboxSection) -> Pending<Option<CachedInbox>> {
        let (host, section) = (host.to_owned(), section.clone());
        self.read(move |conn| inbox::read_inbox(conn, &host, &section))
    }

    /// One cached pull request, from any section or snapshot.
    pub fn pull_request(&self, key: &PrKey) -> Pending<Option<PullRequest>> {
        let key = key.clone();
        self.read(move |conn| inbox::read_pr(conn, &key))
    }

    /// Stores a snapshot, and its pull request row alongside it.
    pub fn put_snapshot(&self, snapshot: impl Into<Arc<Snapshot>>) -> Pending<()> {
        let snapshot = snapshot.into();
        self.write(move |conn, now| snapshot::put_snapshot(conn, now, &snapshot))
    }

    pub fn snapshot(&self, key: &PrKey) -> Pending<Option<CachedSnapshot>> {
        let key = key.clone();
        self.read(move |conn| snapshot::read_snapshot(conn, &key))
    }

    /// Marks `path` at `blob_id` viewed, or not viewed.
    pub fn set_viewed(&self, key: &PrKey, path: &str, blob_id: &str, viewed: bool) -> Pending<()> {
        self.set_viewed_many(key, vec![(path.to_owned(), blob_id.to_owned())], viewed)
    }

    /// Marks several `(path, blob_id)` pairs in one transaction.
    pub fn set_viewed_many(
        &self,
        key: &PrKey,
        files: Vec<(String, String)>,
        viewed: bool,
    ) -> Pending<()> {
        let key = key.clone();
        self.write(move |conn, now| viewed::set_viewed(conn, now, &key, &files, viewed))
    }

    pub fn viewed_for(&self, key: &PrKey) -> Pending<ViewedSet> {
        let key = key.clone();
        self.read(move |conn| viewed::read_viewed(conn, &key))
    }

    /// Replaces the review marks of `key`; an empty mark removes them.
    pub fn put_mark(&self, key: &PrKey, mark: ReviewMark) -> Pending<()> {
        let key = key.clone();
        self.write(move |conn, now| marks::put_mark(conn, now, &key, &mark))
    }

    /// The review marks of every pull request that has any.
    pub fn marks(&self) -> Pending<Vec<(PrKey, ReviewMark)>> {
        self.read(marks::read_marks)
    }

    /// Saves the draft text (call on every edit). `commit` is the head the
    /// draft's line refers to.
    pub fn put_draft(
        &self,
        pr: &PrKey,
        anchor: &DraftAnchor,
        body: &str,
        commit: Option<&Sha>,
    ) -> Pending<()> {
        let (pr, anchor, body, commit) =
            (pr.clone(), anchor.clone(), body.to_owned(), commit.cloned());
        self.write(move |conn, now| {
            draft::put_draft(conn, now, &pr, &anchor, &body, commit.as_ref())
        })
    }

    pub fn draft(&self, pr: &PrKey, anchor: &DraftAnchor) -> Pending<Option<Draft>> {
        let (pr, anchor) = (pr.clone(), anchor.clone());
        self.read(move |conn| draft::read_draft(conn, &pr, &anchor))
    }

    /// Every draft of the pull request, oldest edit first.
    pub fn drafts_for(&self, pr: &PrKey) -> Pending<Vec<Draft>> {
        let pr = pr.clone();
        self.read(move |conn| draft::read_drafts(conn, &pr))
    }

    pub fn delete_draft(&self, pr: &PrKey, anchor: &DraftAnchor) -> Pending<()> {
        let (pr, anchor) = (pr.clone(), anchor.clone());
        self.write(move |conn, _| draft::delete_draft(conn, &pr, &anchor))
    }

    /// Adds the comment to the pending review and removes the draft at its
    /// anchor, in one transaction.
    pub fn queue_comment(&self, pr: &PrKey, new: NewQueued) -> Pending<QueuedComment> {
        let pr = pr.clone();
        self.write(move |conn, now| draft::queue_comment(conn, now, &pr, &new))
    }

    /// Records the forge's pending comments that are not queued here yet
    /// (matched by forge id), and returns every queued comment after.
    pub fn adopt_queued(&self, pr: &PrKey, found: Vec<NewQueued>) -> Pending<Vec<QueuedComment>> {
        let pr = pr.clone();
        self.write(move |conn, now| draft::adopt_queued(conn, now, &pr, &found))
    }

    /// Drops one queued comment by its id.
    pub fn delete_queued(&self, id: i64) -> Pending<()> {
        self.write(move |conn, _| draft::delete_queued(conn, id))
    }

    /// Changes a queued comment's text, and its forge id when given.
    pub fn update_queued(&self, id: i64, body: &str, forge_id: Option<&CommentId>) -> Pending<()> {
        let (body, forge_id) = (body.to_owned(), forge_id.cloned());
        self.write(move |conn, _| draft::update_queued(conn, id, &body, forge_id.as_ref()))
    }

    /// The pending review's comments, oldest first.
    pub fn queued_for(&self, pr: &PrKey) -> Pending<Vec<QueuedComment>> {
        let pr = pr.clone();
        self.read(move |conn| draft::read_queued(conn, &pr))
    }

    /// The review was submitted: drops its queued comments and its summary
    /// draft.
    pub fn finish_review(&self, pr: &PrKey) -> Pending<()> {
        let pr = pr.clone();
        self.write(move |conn, _| draft::finish_review(conn, &pr))
    }

    /// Adds a subscription, or updates the poller's `state` of an existing one.
    pub fn put_subscription(
        &self,
        target: &SubscriptionTarget,
        state: Option<&str>,
    ) -> Pending<()> {
        let (target, state) = (target.clone(), state.map(str::to_owned));
        self.write(move |conn, now| {
            subscription::put_subscription(conn, now, &target, state.as_deref())
        })
    }

    pub fn subscriptions(&self) -> Pending<Vec<Subscription>> {
        self.read(subscription::read_subscriptions)
    }

    pub fn delete_subscription(&self, target: &SubscriptionTarget) -> Pending<()> {
        let target = target.clone();
        self.write(move |conn, _| subscription::delete_subscription(conn, &target))
    }

    /// ETag, last-modified and watermark of an endpoint (any stable string
    /// the caller picks, such as `github.com inbox review_requested`).
    pub fn meta(&self, endpoint: &str) -> Pending<Option<StoredFetchMeta>> {
        let endpoint = endpoint.to_owned();
        self.read(move |conn| meta::read_meta(conn, &endpoint))
    }

    pub fn set_meta(&self, endpoint: &str, meta: &FetchMeta) -> Pending<()> {
        let (endpoint, meta) = (endpoint.to_owned(), meta.clone());
        self.write(move |conn, now| meta::set_meta(conn, now, &endpoint, &meta))
    }

    /// Retention pruning and the size check, which also run once in the
    /// background when the store opens.
    pub fn maintain(&self) -> Pending<MaintenanceReport> {
        let limits = self.inner.limits;
        self.write(move |conn, now| maintenance::maintain(conn, now, &limits))
    }
}

fn spawn(name: &str, body: impl FnOnce() + Send + 'static) -> Result<JoinHandle<()>> {
    Ok(thread::Builder::new().name(name.to_owned()).spawn(body)?)
}

/// Runs jobs until every sender is gone. A panicking job drops its reply
/// channel, so its caller sees a shutdown error, and the thread carries on.
fn run_writer(mut conn: Connection, jobs: &Receiver<WriteJob>, clock: &dyn Clock) {
    while let Ok(job) = jobs.recv() {
        let now = clock.now_ms();
        let _ = catch_unwind(AssertUnwindSafe(|| job(&mut conn, now)));
    }
}

fn run_reader(conn: &Connection, jobs: &Receiver<ReadJob>) {
    while let Ok(job) = jobs.recv() {
        let _ = catch_unwind(AssertUnwindSafe(|| job(conn)));
    }
}
