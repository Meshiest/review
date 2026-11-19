//! What the live app talks to: the config, the SQLite cache, the forge hub,
//! the git object stores and the fetch queue. Independent of gpui, so the
//! open-pull-request flow can be driven in tests with a fake hub.
//!
//! Nothing here blocks its caller except [`ObjectStores::resolve`], which
//! callers run through [`Services::run_blocking`].

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use forge::model::{PrNumber, RepoRef, Snapshot, Stack};
use forge::{Capabilities, ForgeHub};
use futures::FutureExt as _;
use futures::channel::{mpsc, oneshot};
use futures::future::{BoxFuture, Shared};
use repo::{FetchEvent, FetchRequest, Fetched, PrRefs};
use store::PrKey;

use crate::config::Config;

/// Runs a future to completion somewhere other than the caller.
pub type Spawn = Arc<dyn Fn(BoxFuture<'static, ()>) + Send + Sync>;

type SnapshotRequests = HashMap<PrKey, (Instant, SharedResult<Arc<Snapshot>>)>;

/// A result many waiters share; errors are text so they can be cloned.
pub type SharedResult<T> = Shared<BoxFuture<'static, Result<T, String>>>;

/// Git fetches that run at once; the rest wait in the queue.
const FETCH_SLOTS: usize = 3;
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
/// A fetch or snapshot this recent is reused instead of asked for again, so
/// opening right after a hover prefetch does not go to the network twice.
const FRESH: Duration = Duration::from_secs(30);

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Counts for the status bar, and a hook that fires when one changes.
#[derive(Clone, Default)]
pub struct Activity {
    inner: Arc<ActivityInner>,
}

#[derive(Default)]
struct ActivityInner {
    forge: AtomicUsize,
    git_active: AtomicUsize,
    git_queued: AtomicUsize,
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// What is in flight right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityCounts {
    pub forge: usize,
    pub git_active: usize,
    pub git_queued: usize,
}

impl Activity {
    pub fn counts(&self) -> ActivityCounts {
        ActivityCounts {
            forge: self.inner.forge.load(Ordering::Relaxed),
            git_active: self.inner.git_active.load(Ordering::Relaxed),
            git_queued: self.inner.git_queued.load(Ordering::Relaxed),
        }
    }

    pub fn set_on_change(&self, hook: impl Fn() + Send + Sync + 'static) {
        *lock(&self.inner.on_change) = Some(Arc::new(hook));
    }

    fn changed(&self) {
        let hook = lock(&self.inner.on_change).clone();
        if let Some(hook) = hook {
            hook();
        }
    }

    fn bump(&self, counter: fn(&ActivityInner) -> &AtomicUsize, up: bool) {
        let counter = counter(&self.inner);
        if up {
            counter.fetch_add(1, Ordering::Relaxed);
        } else {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        self.changed();
    }

    /// Counts one forge request until the guard drops.
    fn forge_request(&self) -> ActivityGuard {
        self.bump(|a| &a.forge, true);
        ActivityGuard {
            activity: self.clone(),
            counter: |a| &a.forge,
        }
    }
}

struct ActivityGuard {
    activity: Activity,
    counter: fn(&ActivityInner) -> &AtomicUsize,
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.activity.bump(self.counter, false);
    }
}

/// The git object store of each repository: a registered clone from the
/// config, or an app-owned bare repository under the data dir.
#[derive(Clone)]
pub struct ObjectStores {
    config: Arc<Config>,
    root: PathBuf,
    open: Arc<Mutex<HashMap<RepoRef, repo::Store>>>,
}

impl ObjectStores {
    pub fn new(config: Arc<Config>) -> Self {
        let root = config.data_dir().join("repos");
        Self {
            config,
            root,
            open: Arc::default(),
        }
    }

    /// Opens the store for `repo` if it already exists, so the first open of
    /// one of its pull requests does not pay for it. Blocks on disk.
    pub fn warm(&self, repo: &RepoRef) {
        if lock(&self.open).contains_key(repo) {
            return;
        }
        let exists = self
            .config
            .repo(repo)
            .is_some_and(|entry| entry.clone.is_some())
            || {
                let (dir, name) = bare_location(&self.root, repo);
                dir.join(format!("{name}.git")).join("HEAD").is_file()
            };
        if exists {
            let _ = self.resolve(repo);
        }
    }

    /// Opens (or on first use creates) the store for `repo`. Blocks on disk.
    ///
    /// # Errors
    /// If the registered clone is not a repository, or the bare one cannot
    /// be created.
    pub fn resolve(&self, repo: &RepoRef) -> Result<repo::Store, String> {
        if let Some(store) = lock(&self.open).get(repo) {
            return Ok(store.clone());
        }
        let clone = self.config.repo(repo).and_then(|entry| entry.clone.clone());
        let store = match clone {
            Some(path) => repo::Store::open(&path)
                .map_err(|error| format!("registered clone {}: {error}", path.display()))?,
            None => {
                let (dir, name) = bare_location(&self.root, repo);
                repo::Store::open_or_create_bare(&dir, &name)
                    .map_err(|error| format!("object store for {}: {error}", repo.path()))?
            }
        };
        lock(&self.open).insert(repo.clone(), store.clone());
        Ok(store)
    }

    /// The app-owned bare repository for `repo` under the data dir, even
    /// when a registered clone exists: where an agent's worktree goes, away
    /// from the folders the user trusts. Blocks on disk.
    ///
    /// # Errors
    /// If the bare repository cannot be created or opened.
    pub fn app_owned(&self, repo: &RepoRef) -> Result<repo::Store, String> {
        if let Some(store) = lock(&self.open)
            .get(repo)
            .filter(|store| store.kind() == repo::StoreKind::AppOwned)
        {
            return Ok(store.clone());
        }
        let (dir, name) = bare_location(&self.root, repo);
        repo::Store::open_or_create_bare(&dir, &name)
            .map_err(|error| format!("object store for {}: {error}", repo.path()))
    }
}

/// `<root>/<host>/<owner parts>/` and the repository name, each part reduced
/// to characters that are safe in a path, so no forge data can climb out of
/// the data dir.
pub fn bare_location(root: &Path, repo: &RepoRef) -> (PathBuf, String) {
    let mut dir = root.join(path_part(&repo.host));
    for part in repo.owner.split('/') {
        dir.push(path_part(part));
    }
    (dir, path_part(&repo.name))
}

fn path_part(text: &str) -> String {
    let part: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if part.trim_matches('.').is_empty() {
        "_".repeat(part.len().max(1))
    } else {
        part
    }
}

struct FetchEntry {
    result: SharedResult<Fetched>,
    listeners: Arc<Mutex<Vec<mpsc::UnboundedSender<String>>>>,
    finished: Arc<Mutex<Option<Instant>>>,
}

/// Git fetches, at most [`FETCH_SLOTS`] at once, one per pull request: a
/// second ask while one runs (a hover, then a click) joins it.
#[derive(Clone)]
pub struct FetchQueue {
    entries: Arc<Mutex<HashMap<(PathBuf, u64), FetchEntry>>>,
    slots: Arc<(Mutex<usize>, Condvar)>,
    activity: Activity,
}

impl FetchQueue {
    pub fn new(activity: Activity) -> Self {
        Self {
            entries: Arc::default(),
            slots: Arc::new((Mutex::new(FETCH_SLOTS), Condvar::new())),
            activity,
        }
    }

    /// Fetches the pull request into `objects`, or joins the fetch that is
    /// running or just finished. `progress` gets git's progress lines and
    /// closes when the fetch ends. Does not block: the fetch runs on its own
    /// thread.
    pub fn fetch(
        &self,
        objects: &repo::Store,
        request: FetchRequest,
        progress: Option<mpsc::UnboundedSender<String>>,
    ) -> SharedResult<Fetched> {
        let key = (objects.git_dir().to_owned(), request.number);
        let mut entries = lock(&self.entries);
        entries.retain(|_, entry| {
            lock(&entry.finished).is_none_or(|at| at.elapsed() < Duration::from_secs(600))
        });
        if let Some(entry) = entries.get(&key) {
            let finished = *lock(&entry.finished);
            let reusable = match finished {
                None => true,
                Some(at) => at.elapsed() < FRESH && matches!(entry.result.peek(), Some(Ok(_))),
            };
            if reusable {
                if finished.is_none()
                    && let Some(progress) = progress
                {
                    lock(&entry.listeners).push(progress);
                }
                return entry.result.clone();
            }
        }
        let (tx, rx) = oneshot::channel();
        let listeners = Arc::new(Mutex::new(progress.into_iter().collect::<Vec<_>>()));
        let finished = Arc::new(Mutex::new(None));
        let result = rx
            .map(|received: Result<Result<Fetched, String>, _>| {
                received.unwrap_or_else(|_| Err("the fetch stopped".to_owned()))
            })
            .boxed()
            .shared();
        entries.insert(
            key,
            FetchEntry {
                result: result.clone(),
                listeners: listeners.clone(),
                finished: finished.clone(),
            },
        );
        drop(entries);

        let queue = self.clone();
        let objects = objects.clone();
        self.activity.bump(|a| &a.git_queued, true);
        let spawned = thread::Builder::new()
            .name(format!("fetch-queue-{}", request.number))
            .spawn(move || {
                queue.acquire();
                queue.activity.bump(|a| &a.git_queued, false);
                queue.activity.bump(|a| &a.git_active, true);
                let outcome = run_fetch(&objects, &request, &listeners);
                lock(&listeners).clear();
                *lock(&finished) = Some(Instant::now());
                let _ = tx.send(outcome);
                queue.release();
                queue.activity.bump(|a| &a.git_active, false);
            });
        if spawned.is_err() {
            self.activity.bump(|a| &a.git_queued, false);
        }
        result
    }

    /// Drops the finished fetch of pull request `number` into `objects`, so
    /// the next ask fetches again (after a push moved its head).
    pub fn forget(&self, objects: &repo::Store, number: u64) {
        let key = (objects.git_dir().to_owned(), number);
        let mut entries = lock(&self.entries);
        if entries
            .get(&key)
            .is_some_and(|entry| lock(&entry.finished).is_some())
        {
            entries.remove(&key);
        }
    }

    fn acquire(&self) {
        let (count, ready) = &*self.slots;
        let mut free = lock(count);
        while *free == 0 {
            free = ready.wait(free).unwrap_or_else(PoisonError::into_inner);
        }
        *free -= 1;
    }

    fn release(&self) {
        let (count, ready) = &*self.slots;
        *lock(count) += 1;
        ready.notify_one();
    }
}

fn run_fetch(
    objects: &repo::Store,
    request: &FetchRequest,
    listeners: &Mutex<Vec<mpsc::UnboundedSender<String>>>,
) -> Result<Fetched, String> {
    let handle = objects
        .fetch_pr(request)
        .map_err(|error| error.to_string())?;
    for event in &handle.events {
        match event {
            FetchEvent::Progress(line) => {
                lock(listeners).retain(|listener| listener.unbounded_send(line.clone()).is_ok());
            }
            FetchEvent::Finished(result) => return result.map_err(|error| error.to_string()),
        }
    }
    Err("the fetch stopped without a result".to_owned())
}

/// Everything the live app reaches outside the UI thread. Cheap to clone.
#[derive(Clone)]
pub struct Services {
    pub config: Arc<Config>,
    pub store: Option<store::Store>,
    pub hub: Option<Arc<ForgeHub>>,
    pub objects: ObjectStores,
    pub fetches: FetchQueue,
    pub activity: Activity,
    spawn: Spawn,
    snapshots: Arc<Mutex<SnapshotRequests>>,
    /// What each connected forge reported it can do, by host.
    capabilities: Arc<Mutex<HashMap<String, Capabilities>>>,
}

impl Services {
    pub fn new(
        config: Arc<Config>,
        store: Option<store::Store>,
        hub: Option<Arc<ForgeHub>>,
        spawn: Spawn,
    ) -> Self {
        let activity = Activity::default();
        Self {
            objects: ObjectStores::new(config.clone()),
            fetches: FetchQueue::new(activity.clone()),
            config,
            store,
            hub,
            activity,
            spawn,
            snapshots: Arc::default(),
            capabilities: Arc::default(),
        }
    }

    /// What the backend for `host` reported once connected (Forgejo's
    /// depends on the version it probed); `None` until then.
    pub fn capabilities(&self, host: &str) -> Option<Capabilities> {
        lock(&self.capabilities).get(host).copied()
    }

    pub fn set_capabilities(&self, host: &str, capabilities: Capabilities) {
        lock(&self.capabilities).insert(host.to_owned(), capabilities);
    }

    /// A snapshot patched after a write: the cache and the in-flight answer
    /// both become it, so opening the pull request again shows the write.
    pub fn replace_snapshot(&self, key: &PrKey, snapshot: Arc<Snapshot>) {
        if let Some(store) = &self.store {
            store.put_snapshot(snapshot.clone()).detach();
        }
        let ready = futures::future::ready(Ok(snapshot)).boxed().shared();
        lock(&self.snapshots).insert(key.clone(), (Instant::now(), ready));
    }

    /// Forgets the answered snapshot of `key`, so the next ask goes to the
    /// forge.
    pub fn forget_snapshot(&self, key: &PrKey) {
        lock(&self.snapshots).remove(key);
    }

    /// Runs `job` on another thread. `None` if it panicked.
    pub fn run_blocking<T: Send + 'static>(
        &self,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> impl Future<Output = Option<T>> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        (self.spawn)(
            async move {
                let _ = tx.send(job());
            }
            .boxed(),
        );
        rx.map(Result::ok)
    }

    /// Runs `op` on the forge thread for `host`, counted in the status bar.
    pub fn forge<T, F, Fut>(
        &self,
        host: &str,
        op: F,
    ) -> impl Future<Output = forge::Result<T>> + Send + 'static
    where
        T: Send + 'static,
        F: FnOnce(Arc<dyn forge::Forge>) -> Fut + Send + 'static,
        Fut: Future<Output = forge::Result<T>> + Send + 'static,
    {
        let pending = self.hub.as_ref().map(|hub| hub.spawn(host, op));
        let guard = pending.as_ref().map(|_| self.activity.forge_request());
        async move {
            let _guard = guard;
            match pending {
                Some(pending) => pending.await,
                None => Err(forge::Error::Shutdown),
            }
        }
    }

    /// The pull request's snapshot from the forge, stored in the cache when
    /// it arrives. Joins a request that is running or just finished. Runs to
    /// the end even if nobody awaits the result (a prefetch).
    pub fn snapshot(&self, key: &PrKey) -> SharedResult<Arc<Snapshot>> {
        let mut snapshots = lock(&self.snapshots);
        if let Some((at, shared)) = snapshots.get(key) {
            let running = shared.peek().is_none();
            if running || (at.elapsed() < FRESH && matches!(shared.peek(), Some(Ok(_)))) {
                return shared.clone();
            }
        }
        let (tx, rx) = oneshot::channel();
        let (repo, number) = (key.repo.clone(), key.number);
        let call = self.forge(&key.repo.host, move |forge| async move {
            forge.snapshot(&repo, number).await
        });
        let store = self.store.clone();
        (self.spawn)(
            async move {
                let result = call.await.map(Arc::new).map_err(|error| error.to_string());
                if let (Ok(snapshot), Some(store)) = (&result, &store) {
                    store.put_snapshot(snapshot.clone()).detach();
                }
                let _ = tx.send(result);
            }
            .boxed(),
        );
        let shared = rx
            .map(|received: Result<Result<Arc<Snapshot>, String>, _>| {
                received.unwrap_or_else(|_| Err("the request stopped".to_owned()))
            })
            .boxed()
            .shared();
        snapshots.insert(key.clone(), (Instant::now(), shared.clone()));
        shared
    }

    pub fn stack(
        &self,
        key: &PrKey,
    ) -> impl Future<Output = forge::Result<Stack>> + Send + 'static {
        let (repo, number) = (key.repo.clone(), key.number);
        self.forge(&key.repo.host, move |forge| async move {
            forge.stack(&repo, number).await
        })
    }

    /// The fetch request for a pull request whose base branch is `base`.
    pub fn fetch_request(&self, repo: &RepoRef, number: PrNumber, base: &str) -> FetchRequest {
        let refs = match self.config.forge(&repo.host).map(|forge| forge.kind) {
            Some(forge::model::ForgeKind::GitLab) => PrRefs::MergeRequests,
            _ => PrRefs::Pull,
        };
        FetchRequest {
            remote_url: self.config.remote_url(repo),
            refs,
            number: number.0,
            base_branch: base.to_owned(),
            timeout: FETCH_TIMEOUT,
        }
    }

    /// Starts the snapshot and the git fetch of a pull request the user is
    /// likely to open, so opening it finds both ready.
    pub fn prefetch(&self, key: &PrKey, base: Option<&str>) {
        drop(self.snapshot(key));
        let Some(base) = base else {
            return;
        };
        let request = self.fetch_request(&key.repo, key.number, base);
        let (objects, fetches, repo) =
            (self.objects.clone(), self.fetches.clone(), key.repo.clone());
        let resolved = self.run_blocking(move || objects.resolve(&repo));
        (self.spawn)(
            async move {
                if let Some(Ok(objects)) = resolved.await {
                    drop(fetches.fetch(&objects, request, None));
                }
            }
            .boxed(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_stores_stay_under_the_data_dir() {
        let root = Path::new("data");
        let (dir, name) = bare_location(root, &RepoRef::new("gitlab.com", "group/sub", "proj"));
        assert_eq!(dir, root.join("gitlab.com").join("group").join("sub"));
        assert_eq!(name, "proj");
        let (dir, name) = bare_location(root, &RepoRef::new("evil.host", "../..", ".."));
        assert_eq!(dir, root.join("evil.host").join("__").join("__"));
        assert_eq!(name, "__");
        let (_, name) = bare_location(root, &RepoRef::new("h", "o", "a b:c"));
        assert_eq!(name, "a_b_c");
    }
}
