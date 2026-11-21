//! Git access: object stores, fetching pull request heads, commit ranges,
//! tree and blob diffs, worktrees here and on other machines over `ssh`.
//!
//! Everything here blocks (disk, git, child processes). Call it from a
//! background thread, never from the UI thread; [`Store::fetch_pr`] already
//! does that itself and reports on a channel.

mod blob;
pub mod cancel;
mod error;
mod fetch;
mod files;
mod git;
mod hunks;
mod log;
pub mod shell;
mod store;
mod suggest;
mod worktree;

pub use blob::Blob;
pub use error::{Error, Result};
pub use fetch::{FetchEvent, FetchHandle, FetchRequest, Fetched, PrRefs};
pub use files::{
    ChangeKind, ContentKind, Entry, FileChange, FileContent, FileList, FileListOptions, FileMode,
};
pub use gix::ObjectId;
pub use hunks::{CONTEXT, compute_hunks, render_hunk};
pub use log::{CommitInfo, CommitLog};
pub use store::{Store, StoreKind, pr_base_ref, pr_head_ref};
pub use suggest::{LineEdit, PushTarget};
pub use worktree::{
    RemoteRepo, RemoveMode, SshHost, Worktree, add_worktree, checkout_detached, clone_repo,
    common_dir, has_changes, list_worktrees, parse_worktree_list, remove_worktree,
};
