use std::collections::{HashMap, VecDeque};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use gix::ObjectId;

use crate::blob::Blob;
use crate::error::{Error, Result};

/// Object cache per thread-local repository handle, in bytes.
const OBJECT_CACHE: usize = 64 << 20;
/// Decoded blobs kept for reuse across views, in bytes.
const BLOB_CACHE: usize = 128 << 20;

/// Where a store's objects live.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    /// A bare repository the app created under its data dir.
    AppOwned,
    /// A repository the user already has, with a working tree. Only read and
    /// fetched into: no checkout, reset or HEAD change.
    Registered,
}

/// A git repository read through `gix`. Cheap to clone and safe to share
/// between threads.
#[derive(Clone)]
pub struct Store {
    inner: Arc<Inner>,
}

struct Inner {
    repo: gix::ThreadSafeRepository,
    kind: StoreKind,
    blobs: Mutex<BlobCache>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("git_dir", &self.git_dir())
            .field("kind", &self.inner.kind)
            .finish()
    }
}

impl Store {
    /// Opens an existing repository, bare or with a working tree. One with a
    /// working tree is a registered clone; a bare one is app-owned.
    ///
    /// # Errors
    /// If `path` is not a git repository.
    pub fn open(path: &Path) -> Result<Self> {
        let repo = gix::ThreadSafeRepository::open(path).map_err(Error::gix)?;
        let kind = if repo.work_dir().is_some() {
            StoreKind::Registered
        } else {
            StoreKind::AppOwned
        };
        Ok(Self::from_repo(repo, kind))
    }

    /// Opens the app-owned bare repository `<data_dir>/<name>.git`, creating
    /// it when missing.
    ///
    /// # Errors
    /// If `name` is not a single path component, or git cannot create or
    /// open the repository.
    pub fn open_or_create_bare(data_dir: &Path, name: &str) -> Result<Self> {
        let mut components = Path::new(name).components();
        let single_name = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        );
        if !single_name {
            return Err(Error::InvalidArgument(format!(
                "store name `{name}` must be one plain path component"
            )));
        }
        let path = data_dir.join(format!("{name}.git"));
        if path.join("HEAD").is_file() {
            return Self::open(&path);
        }
        std::fs::create_dir_all(&path)?;
        let repo = gix::init_bare(&path).map_err(Error::gix)?;
        Ok(Self::from_repo(repo.into_sync(), StoreKind::AppOwned))
    }

    fn from_repo(repo: gix::ThreadSafeRepository, kind: StoreKind) -> Self {
        Self {
            inner: Arc::new(Inner {
                repo,
                kind,
                blobs: Mutex::new(BlobCache::default()),
            }),
        }
    }

    #[must_use]
    pub fn kind(&self) -> StoreKind {
        self.inner.kind
    }

    #[must_use]
    pub fn git_dir(&self) -> &Path {
        self.inner.repo.git_dir()
    }

    /// The working tree of a registered clone.
    #[must_use]
    pub fn work_dir(&self) -> Option<&Path> {
        self.inner.repo.work_dir()
    }

    /// A handle for the calling thread.
    pub(crate) fn repo(&self) -> gix::Repository {
        let mut repo = self.inner.repo.to_thread_local();
        repo.object_cache_size_if_unset(OBJECT_CACHE);
        repo
    }

    /// The commit a revision (branch, ref, hex id, `HEAD~2`) names.
    ///
    /// # Errors
    /// If the revision does not resolve to a commit in this store.
    pub fn resolve_commit(&self, rev: &str) -> Result<ObjectId> {
        let repo = self.repo();
        let id = repo
            .rev_parse_single(rev)
            .map_err(|_| Error::RevisionNotFound(rev.to_owned()))?;
        let commit = id
            .object()
            .and_then(|object| object.peel_to_commit())
            .map_err(|_| Error::RevisionNotFound(rev.to_owned()))?;
        Ok(commit.id)
    }

    /// The best common ancestor of two commits.
    ///
    /// # Errors
    /// If there is none, or history cannot be walked.
    pub fn merge_base(&self, one: ObjectId, two: ObjectId) -> Result<ObjectId> {
        let repo = self.repo();
        let no_base = || Error::NoMergeBase(one.to_string(), two.to_string());
        repo.merge_base(one, two)
            .ok()
            .flatten()
            .map(|id| id.detach())
            .ok_or_else(no_base)
    }

    /// Reads a blob. Repeated reads of one id share the buffer.
    ///
    /// # Errors
    /// If the object is missing, or the blob is 4 GiB or larger.
    pub fn blob(&self, id: ObjectId) -> Result<Blob> {
        if let Some(blob) = self.cache().get(&id) {
            return Ok(blob);
        }
        let blob = self.read_blob(id)?;
        self.cache().insert(id, blob.clone());
        Ok(blob)
    }

    fn read_blob(&self, id: ObjectId) -> Result<Blob> {
        let repo = self.repo();
        let object = repo.find_object(id).map_err(Error::gix)?;
        Blob::new(object.data.as_slice())
    }

    /// Every symbolic link in `commit`'s tree: its path and the target it
    /// names, as git stores it.
    ///
    /// # Errors
    /// If the commit, its tree or a link's blob cannot be read.
    pub fn symlinks(&self, commit: ObjectId) -> Result<Vec<(String, String)>> {
        let repo = self.repo();
        let tree = repo
            .find_commit(commit)
            .map_err(Error::gix)?
            .tree()
            .map_err(Error::gix)?;
        let mut recorder = gix::traverse::tree::Recorder::default();
        tree.traverse()
            .breadthfirst(&mut recorder)
            .map_err(Error::gix)?;
        recorder
            .records
            .into_iter()
            .filter(|entry| entry.mode.is_link())
            .map(|entry| {
                let object = repo.find_object(entry.oid).map_err(Error::gix)?;
                Ok((
                    entry.filepath.to_string(),
                    String::from_utf8_lossy(&object.data).into_owned(),
                ))
            })
            .collect()
    }

    /// Size in bytes of an object, without reading it.
    pub(crate) fn object_size(&self, id: ObjectId) -> Result<u64> {
        let header = self.repo().find_header(id).map_err(Error::gix)?;
        Ok(header.size())
    }

    fn cache(&self) -> MutexGuard<'_, BlobCache> {
        self.inner
            .blobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Insertion-ordered blob cache bounded by total bytes.
#[derive(Default)]
struct BlobCache {
    blobs: HashMap<ObjectId, Blob>,
    order: VecDeque<ObjectId>,
    bytes: usize,
}

impl BlobCache {
    fn get(&self, id: &ObjectId) -> Option<Blob> {
        self.blobs.get(id).cloned()
    }

    fn insert(&mut self, id: ObjectId, blob: Blob) {
        let size = blob.as_bytes().len();
        if size > BLOB_CACHE / 4 || self.blobs.contains_key(&id) {
            return;
        }
        while self.bytes + size > BLOB_CACHE {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(evicted) = self.blobs.remove(&oldest) {
                self.bytes -= evicted.as_bytes().len();
            }
        }
        self.bytes += size;
        self.order.push_back(id);
        self.blobs.insert(id, blob);
    }
}

/// Paths used for a pull request's refs inside a store.
#[must_use]
pub fn pr_head_ref(number: u64) -> String {
    format!("refs/review/pr/{number}")
}

#[must_use]
pub fn pr_base_ref(number: u64) -> String {
    format!("refs/review/base/{number}")
}

impl Store {
    /// A pull request's diff base: the merge base of its fetched base branch
    /// and head. Returns `(merge_base, head)`.
    ///
    /// # Errors
    /// If the refs were not fetched yet, or share no history.
    pub fn pr_range(&self, number: u64) -> Result<(ObjectId, ObjectId)> {
        let head = self.resolve_commit(&pr_head_ref(number))?;
        let base = self.resolve_commit(&pr_base_ref(number))?;
        Ok((self.merge_base(base, head)?, head))
    }

    /// Where a worktree for pull request `number` goes: inside a registered
    /// clone at `.claude/worktrees/pr-N`, for an app-owned store next to its
    /// repository at `<name>.worktrees/pr-N`.
    #[must_use]
    pub fn worktree_path(&self, number: u64) -> PathBuf {
        let leaf = format!("pr-{number}");
        match self.work_dir() {
            Some(clone) => clone.join(".claude").join("worktrees").join(leaf),
            None => {
                let git_dir = self.git_dir();
                let stem = git_dir
                    .file_stem()
                    .map_or_else(|| "repo".into(), |s| s.to_string_lossy().into_owned());
                git_dir
                    .with_file_name(format!("{stem}.worktrees"))
                    .join(leaf)
            }
        }
    }
}
