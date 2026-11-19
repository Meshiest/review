//! The file list of a diff: a tree diff, available before any hunk is computed.

use std::thread;

use diff::Hunk;
use gix::ObjectId;
use gix::object::tree::EntryKind;

use crate::blob::{self, Blob};
use crate::error::{Error, Result};
use crate::hunks::compute_hunks;
use crate::store::Store;

/// Blobs above this size are not diffed; git itself stops at 512 MiB.
const MAX_DIFF_BYTES: u64 = 128 << 20;

/// The file list decodes blobs on many threads at once just to probe for
/// binary content, so it skips large ones; their hunks pass decides later.
const MAX_PROBE_BYTES: u64 = 16 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileMode {
    Regular,
    Executable,
    Symlink,
    Submodule,
}

/// One side of a change: the object and how it is stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: ObjectId,
    pub mode: FileMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Deleted,
    Modified,
    /// Same or similar content under a new path; `FileChange::similarity` says how similar.
    Renamed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileChange {
    pub kind: ChangeKind,
    /// Path on the new side (the old side for a deletion).
    pub path: String,
    /// Path on the old side, for a rename.
    pub old_path: Option<String>,
    pub old: Option<Entry>,
    pub new: Option<Entry>,
    /// Percent similarity of a rename.
    pub similarity: Option<u8>,
    /// Whether git would call the content binary. `None` until binary
    /// detection ran (see [`FileListOptions::detect_binary`]).
    pub binary: Option<bool>,
}

impl FileChange {
    #[must_use]
    pub fn mode_changed(&self) -> bool {
        matches!((self.old, self.new), (Some(o), Some(n)) if o.mode != n.mode)
    }

    #[must_use]
    pub fn content_changed(&self) -> bool {
        match (self.old, self.new) {
            (Some(o), Some(n)) => o.id != n.id,
            _ => true,
        }
    }

    #[must_use]
    pub fn is_submodule(&self) -> bool {
        [self.old, self.new]
            .into_iter()
            .flatten()
            .any(|e| e.mode == FileMode::Submodule)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct FileListOptions {
    /// Pair deletions with additions of similar content (git's default 50%).
    /// Off matches `git diff --no-renames`.
    pub renames: bool,
    /// Read each blob's first bytes to fill `FileChange::binary`. Costs one
    /// object read per file, spread over several threads.
    pub detect_binary: bool,
}

impl Default for FileListOptions {
    fn default() -> Self {
        Self {
            renames: true,
            detect_binary: false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileList {
    /// The old side; `None` diffs against the empty tree.
    pub old_commit: Option<ObjectId>,
    pub new_commit: ObjectId,
    pub files: Vec<FileChange>,
}

/// What the view should do with a file's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentKind {
    Text,
    /// No hunks; the view shows a "binary file" row.
    Binary,
    /// Over the size limit; no hunks.
    TooLarge,
}

/// A file's two blobs and its hunks, ready for the diff view.
#[derive(Clone, Debug)]
pub struct FileContent {
    pub kind: ContentKind,
    /// Empty for an added file or a binary or too-large one.
    pub old: Blob,
    /// Empty for a deleted file or a binary or too-large one.
    pub new: Blob,
    pub hunks: Vec<Hunk>,
}

impl Store {
    /// Tree diff from `old` to `new` (commits or trees; `None` is the empty
    /// tree), like `git diff old new`. A change of file type (file to
    /// symlink, file to submodule) is a deletion followed by an addition,
    /// as git prints it with `--no-renames`.
    ///
    /// # Errors
    /// If an object is missing.
    pub fn file_list(
        &self,
        old: Option<ObjectId>,
        new: ObjectId,
        options: FileListOptions,
    ) -> Result<FileList> {
        let repo = self.repo();
        let tree_of = |id: ObjectId| {
            repo.find_object(id)
                .and_then(|object| object.peel_to_tree())
                .map_err(Error::gix)
        };
        let old_tree = old.map(tree_of).transpose()?;
        let new_tree = tree_of(new)?;
        let rewrites = options.renames.then(gix::diff::Rewrites::default);
        let diff_options = gix::diff::Options::default().with_rewrites(rewrites);
        let changes = repo
            .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), diff_options)
            .map_err(Error::gix)?;

        let mut files = Vec::with_capacity(changes.len());
        for change in changes {
            push_change(&mut files, change);
        }
        // git lists files in byte order of the full path; the tree walk yields
        // a directory's files before its subdirectories. Stable, so a type
        // change keeps its deletion before its addition.
        files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        if options.detect_binary {
            self.detect_binary(&mut files)?;
        }
        Ok(FileList {
            old_commit: old,
            new_commit: new,
            files,
        })
    }

    /// The file list of a pull request: merge base of the fetched base
    /// branch and head, to head.
    ///
    /// # Errors
    /// If the pull request was not fetched into this store.
    pub fn pr_file_list(&self, number: u64, options: FileListOptions) -> Result<FileList> {
        let (base, head) = self.pr_range(number)?;
        self.file_list(Some(base), head, options)
    }

    /// Reads both blobs of a change and computes its hunks.
    ///
    /// # Errors
    /// If a blob is missing.
    pub fn file_content(&self, change: &FileChange) -> Result<FileContent> {
        let skipped = |kind| FileContent {
            kind,
            old: Blob::default(),
            new: Blob::default(),
            hunks: Vec::new(),
        };
        for entry in [change.old, change.new].into_iter().flatten() {
            if entry.mode != FileMode::Submodule && self.object_size(entry.id)? > MAX_DIFF_BYTES {
                return Ok(skipped(ContentKind::TooLarge));
            }
        }
        let old = self.side_blob(change.old)?;
        let new = match (change.old, change.new) {
            (Some(o), Some(n)) if o.id == n.id => old.clone(),
            (_, entry) => self.side_blob(entry)?,
        };
        if change.content_changed() && (old.is_binary() || new.is_binary()) {
            return Ok(skipped(ContentKind::Binary));
        }
        let hunks = compute_hunks(&old, &new);
        Ok(FileContent {
            kind: ContentKind::Text,
            old,
            new,
            hunks,
        })
    }

    /// A submodule entry has no blob; git diffs a `Subproject commit` line.
    fn side_blob(&self, entry: Option<Entry>) -> Result<Blob> {
        match entry {
            None => Ok(Blob::default()),
            Some(Entry {
                id,
                mode: FileMode::Submodule,
            }) => Blob::new(format!("Subproject commit {id}\n").into_bytes()),
            Some(entry) => self.blob(entry.id),
        }
    }

    fn detect_binary(&self, files: &mut [FileChange]) -> Result<()> {
        let workers = thread::available_parallelism()
            .map_or(4, usize::from)
            .min(8);
        let chunk = files.len().div_ceil(workers).max(64);
        thread::scope(|scope| {
            let handles: Vec<_> = files
                .chunks_mut(chunk)
                .map(|slice| scope.spawn(move || self.detect_binary_slice(slice)))
                .collect();
            handles
                .into_iter()
                .try_for_each(|h| h.join().expect("binary detection does not panic"))
        })
    }

    fn detect_binary_slice(&self, files: &mut [FileChange]) -> Result<()> {
        let repo = self.repo();
        for file in files {
            let mut binary = false;
            for entry in [file.old, file.new].into_iter().flatten() {
                if entry.mode == FileMode::Submodule || binary {
                    continue;
                }
                if self.object_size(entry.id)? > MAX_PROBE_BYTES {
                    continue;
                }
                let object = repo.find_object(entry.id).map_err(Error::gix)?;
                binary = blob::is_binary(&object.data);
            }
            file.binary = Some(binary);
        }
        Ok(())
    }
}

fn entry(id: ObjectId, mode: gix::objs::tree::EntryMode) -> Option<Entry> {
    let mode = match mode.kind() {
        EntryKind::Blob => FileMode::Regular,
        EntryKind::BlobExecutable => FileMode::Executable,
        EntryKind::Link => FileMode::Symlink,
        EntryKind::Commit => FileMode::Submodule,
        EntryKind::Tree => return None,
    };
    Some(Entry { id, mode })
}

fn path_of(location: &gix::bstr::BStr) -> String {
    location.to_string()
}

fn push_change(files: &mut Vec<FileChange>, change: gix::diff::tree_with_rewrites::Change) {
    use gix::diff::tree_with_rewrites::Change;
    match change {
        Change::Addition {
            location,
            entry_mode,
            id,
            ..
        } => files.extend(entry(id, entry_mode).map(|new| FileChange {
            kind: ChangeKind::Added,
            path: path_of(location.as_ref()),
            old_path: None,
            old: None,
            new: Some(new),
            similarity: None,
            binary: None,
        })),
        Change::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => files.extend(entry(id, entry_mode).map(|old| FileChange {
            kind: ChangeKind::Deleted,
            path: path_of(location.as_ref()),
            old_path: None,
            old: Some(old),
            new: None,
            similarity: None,
            binary: None,
        })),
        Change::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => {
            let (Some(old), Some(new)) = (
                entry(previous_id, previous_entry_mode),
                entry(id, entry_mode),
            ) else {
                return;
            };
            let path = path_of(location.as_ref());
            if is_type_change(old.mode, new.mode) {
                push_change(
                    files,
                    Change::Deletion {
                        location: location.clone(),
                        relation: None,
                        entry_mode: previous_entry_mode,
                        id: previous_id,
                    },
                );
                push_change(
                    files,
                    Change::Addition {
                        location,
                        relation: None,
                        entry_mode,
                        id,
                    },
                );
                return;
            }
            files.push(FileChange {
                kind: ChangeKind::Modified,
                path,
                old_path: None,
                old: Some(old),
                new: Some(new),
                similarity: None,
                binary: None,
            });
        }
        Change::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            diff,
            entry_mode,
            id,
            location,
            ..
        } => {
            let (Some(old), Some(new)) =
                (entry(source_id, source_entry_mode), entry(id, entry_mode))
            else {
                return;
            };
            let similarity = diff.map_or(100, |stats| (stats.similarity * 100.0).round() as u8);
            files.push(FileChange {
                kind: ChangeKind::Renamed,
                path: path_of(location.as_ref()),
                old_path: Some(path_of(source_location.as_ref())),
                old: Some(old),
                new: Some(new),
                similarity: Some(similarity),
                binary: None,
            });
        }
    }
}

/// Executable and regular files are one type; anything else crossing is a type change.
fn is_type_change(old: FileMode, new: FileMode) -> bool {
    let regular = |m| matches!(m, FileMode::Regular | FileMode::Executable);
    !(old == new || (regular(old) && regular(new)))
}
