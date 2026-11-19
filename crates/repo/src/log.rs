//! The commits of a range, as `git log base..head` lists them, and the file
//! list of one commit.

use gix::ObjectId;
use gix::revision::walk::Sorting;
use gix::traverse::commit::simple::CommitTimeOrder;

use crate::error::{Error, Result};
use crate::files::{FileList, FileListOptions};
use crate::store::Store;

/// One commit of a range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitInfo {
    pub id: ObjectId,
    /// In order; a merge's diff is against the first.
    pub parents: Vec<ObjectId>,
    /// The message's first line.
    pub title: String,
    pub author: String,
    /// When it was authored, in seconds since the Unix epoch.
    pub authored: i64,
}

impl CommitInfo {
    #[must_use]
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

impl Store {
    /// The commits reachable from `head` and not from `base`, newest first,
    /// in the order `git log base..head` prints them.
    ///
    /// # Errors
    /// If a commit cannot be read.
    pub fn commits(&self, base: ObjectId, head: ObjectId) -> Result<Vec<CommitInfo>> {
        let repo = self.repo();
        let walk = repo
            .rev_walk([head])
            .with_hidden([base])
            .sorting(Sorting::ByCommitTime(CommitTimeOrder::NewestFirst))
            .all()
            .map_err(Error::gix)?;
        let mut commits = Vec::new();
        for info in walk {
            let info = info.map_err(Error::gix)?;
            let commit = info.object().map_err(Error::gix)?;
            let author = commit.author().map_err(Error::gix)?;
            let message = commit.message_raw().map_err(Error::gix)?;
            commits.push(CommitInfo {
                id: info.id,
                parents: commit.parent_ids().map(|id| id.detach()).collect(),
                title: first_line(message),
                author: author.name.to_string(),
                authored: author.seconds(),
            });
        }
        Ok(commits)
    }

    /// A pull request's commits: those on its head and not on its merge base.
    ///
    /// # Errors
    /// If the pull request was not fetched into this store.
    pub fn pr_commits(&self, number: u64) -> Result<Vec<CommitInfo>> {
        let (base, head) = self.pr_range(number)?;
        self.commits(base, head)
    }

    /// The file list of one commit: from its first parent (the empty tree
    /// for a root commit) to it, as `git show --first-parent` diffs it.
    ///
    /// # Errors
    /// If the commit or a tree is missing.
    pub fn commit_file_list(&self, commit: ObjectId, options: FileListOptions) -> Result<FileList> {
        let parent = {
            let repo = self.repo();
            let commit = repo.find_commit(commit).map_err(Error::gix)?;
            commit.parent_ids().next().map(|id| id.detach())
        };
        self.file_list(parent, commit, options)
    }
}

/// The message's first non-blank line, without its line ending.
fn first_line(message: &[u8]) -> String {
    let text = String::from_utf8_lossy(message);
    text.lines()
        .map(str::trim_end)
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::first_line;

    #[test]
    fn the_title_is_the_first_line_with_text() {
        assert_eq!(first_line(b"Fix the walk\n\nBody\n"), "Fix the walk");
        assert_eq!(first_line(b"\n\n  Indented\r\nmore"), "  Indented");
        assert_eq!(first_line(b"one\ntwo\n"), "one");
        assert_eq!(first_line(b""), "");
    }
}
