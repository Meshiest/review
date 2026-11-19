use std::path::PathBuf;
use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A `gix` call failed; the source is boxed because `gix` has one error type per operation.
    #[error("git object access failed: {0}")]
    Gix(Box<dyn std::error::Error + Send + Sync>),
    #[error("revision `{0}` not found")]
    RevisionNotFound(String),
    #[error("{0} has no merge base with {1}")]
    NoMergeBase(String, String),
    #[error("blob is {0} bytes, over the 4 GiB limit")]
    BlobTooLarge(u64),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("`git {args}` failed ({status}): {stderr}")]
    GitFailed {
        args: String,
        status: String,
        stderr: String,
    },
    #[error("`git {args}` timed out after {timeout:?}")]
    GitTimedOut { args: String, timeout: Duration },
    #[error("`git {args}` was canceled")]
    GitCanceled { args: String },
    #[error(
        "worktree path {} is not git-ignored in {}; add `.claude/worktrees/` to its .gitignore",
        path.display(),
        clone.display()
    )]
    WorktreeNotIgnored { path: PathBuf, clone: PathBuf },
    /// The branch a suggestion commit was built for moved on the remote.
    #[error(
        "{branch} moved on the remote (expected {expected}, found {found}); fetch and try again"
    )]
    RemoteMoved {
        branch: String,
        expected: String,
        found: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub(crate) fn gix(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Gix(Box::new(error))
    }
}
