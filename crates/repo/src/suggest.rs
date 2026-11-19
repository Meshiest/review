//! Applying review suggestions where the forge has no API for it (GitHub,
//! Forgejo): a commit on the pull request's head built from git objects
//! alone (no working tree, no checkout), pushed to the head branch as a
//! fast-forward only.
//!
//! The commit is made by `git commit-tree`, so it carries the user's own
//! identity and is signed when their git config signs commits.

use std::collections::BTreeMap;
use std::time::Duration;

use gix::ObjectId;
use gix::objs::tree::EntryKind;

use crate::error::{Error, Result};
use crate::git::GitCommand;
use crate::store::Store;

/// Git's binary heuristic, as in [`crate::Blob::is_binary`].
const BINARY_PROBE: usize = 8000;
const COMMIT_TIMEOUT: Duration = Duration::from_secs(60);

/// One suggestion: lines `first..=last` (1-based, inclusive) of `path` at
/// the head commit become `lines`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineEdit {
    /// Repository-relative, `/`-separated.
    pub path: String,
    pub first: u32,
    pub last: u32,
    /// The new lines without line endings; empty deletes the range.
    pub lines: Vec<String>,
}

/// Where a suggestion commit goes.
#[derive(Clone, Debug)]
pub struct PushTarget {
    /// The head repository's URL (or path).
    pub remote_url: String,
    /// The pull request's head branch, without `refs/heads/`.
    pub branch: String,
    pub timeout: Duration,
}

impl Store {
    /// A commit whose parent is `head` and whose tree is `head`'s with every
    /// edit applied, in one commit however many files and suggestions.
    /// Blocks on git.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] when an edit names a path that is not a
    /// text file at `head`, lines it does not have, or overlaps another
    /// edit, or when the edits change nothing; git's own failure otherwise.
    pub fn commit_edits(
        &self,
        head: ObjectId,
        edits: &[LineEdit],
        message: &str,
    ) -> Result<ObjectId> {
        if edits.is_empty() {
            return Err(Error::InvalidArgument("no suggestions to apply".to_owned()));
        }
        let repo = self.repo();
        let commit = repo.find_commit(head).map_err(Error::gix)?;
        let tree_id = commit.tree_id().map_err(Error::gix)?.detach();
        let tree = repo.find_tree(tree_id).map_err(Error::gix)?;
        let mut by_path: BTreeMap<&str, Vec<&LineEdit>> = BTreeMap::new();
        for edit in edits {
            by_path.entry(edit.path.as_str()).or_default().push(edit);
        }
        let mut editor = repo.edit_tree(tree_id).map_err(Error::gix)?;
        for (path, mut edits) in by_path {
            check_path(path)?;
            let entry = tree
                .lookup_entry(path.split('/').map(str::as_bytes))
                .map_err(Error::gix)?
                .ok_or_else(|| invalid(format!("`{path}` is not in the head commit")))?;
            let kind = entry.mode().kind();
            if !matches!(kind, EntryKind::Blob | EntryKind::BlobExecutable) {
                return Err(invalid(format!("`{path}` is not a regular file")));
            }
            let blob = repo.find_object(entry.object_id()).map_err(Error::gix)?;
            edits.sort_by_key(|edit| edit.first);
            let bytes = apply_edits(path, &blob.data, &edits)?;
            let id = repo.write_blob(&bytes).map_err(Error::gix)?.detach();
            editor.upsert(path, kind, id).map_err(Error::gix)?;
        }
        let new_tree = editor.write().map_err(Error::gix)?.detach();
        if new_tree == tree_id {
            return Err(invalid("the suggestions change nothing".to_owned()));
        }
        let output = GitCommand::new(self.git_dir(), COMMIT_TIMEOUT)
            .args(["commit-tree".to_owned(), new_tree.to_string()])
            .args([
                "-p".to_owned(),
                head.to_string(),
                "-m".to_owned(),
                message.to_owned(),
            ])
            .capture_stdout()
            .run_checked()?;
        ObjectId::from_hex(output.stdout.trim().as_bytes()).map_err(|_| {
            invalid(format!(
                "git commit-tree answered `{}`",
                output.stdout.trim()
            ))
        })
    }

    /// The commit `target`'s branch points at on the remote now, `None` when
    /// the branch does not exist. Blocks on the network.
    ///
    /// # Errors
    /// An unusable URL or branch name, or git's failure.
    pub fn remote_head(&self, target: &PushTarget) -> Result<Option<ObjectId>> {
        let (url, branch_ref) = push_names(target)?;
        let output = GitCommand::new(self.git_dir(), target.timeout)
            .args(["ls-remote", "--", url.as_str(), branch_ref.as_str()])
            .capture_stdout()
            .run_checked()?;
        let line = output
            .stdout
            .lines()
            .find(|line| line.split('\t').nth(1) == Some(branch_ref.as_str()));
        line.map(|line| {
            let sha = line.split('\t').next().unwrap_or_default();
            ObjectId::from_hex(sha.as_bytes())
                .map_err(|_| invalid(format!("git ls-remote answered `{line}`")))
        })
        .transpose()
    }

    /// Pushes `commit` to `target`'s branch, which must still be at
    /// `expected` (the head the commit was built on): a plain push, never a
    /// forced one, so the remote also refuses anything but a fast-forward.
    /// Blocks on the network.
    ///
    /// # Errors
    /// [`Error::RemoteMoved`] when the branch is no longer at `expected`,
    /// [`Error::InvalidArgument`] when `commit` does not descend from
    /// `expected`, git's failure otherwise (a rejected push among them).
    pub fn push_fast_forward(
        &self,
        target: &PushTarget,
        expected: ObjectId,
        commit: ObjectId,
    ) -> Result<()> {
        if self.merge_base(expected, commit)? != expected {
            return Err(invalid(format!(
                "{commit} does not build on {expected}; refusing to push"
            )));
        }
        let found = self.remote_head(target)?;
        if found != Some(expected) {
            return Err(Error::RemoteMoved {
                branch: target.branch.clone(),
                expected: expected.to_string(),
                found: found.map_or_else(|| "nothing".to_owned(), |id| id.to_string()),
            });
        }
        let (url, branch_ref) = push_names(target)?;
        GitCommand::new(self.git_dir(), target.timeout)
            .args([
                "-c",
                "push.followTags=false",
                "push",
                "--porcelain",
                "--no-signed",
                "--",
                url.as_str(),
            ])
            .args([format!("{commit}:{branch_ref}")])
            .run_checked()?;
        Ok(())
    }
}

fn invalid(message: String) -> Error {
    Error::InvalidArgument(message)
}

/// A path from forge data names a file in the tree, nothing else: no empty,
/// `.` or `..` component, no `.git`, no backslash or NUL.
fn check_path(path: &str) -> Result<()> {
    let bad = path.is_empty()
        || path.contains(['\\', '\0'])
        || path.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
        });
    if bad {
        return Err(invalid(format!("unusable path `{path}`")));
    }
    Ok(())
}

fn push_names(target: &PushTarget) -> Result<(String, String)> {
    let url = target.remote_url.trim();
    if url.is_empty() || url.starts_with('-') || url.contains(char::is_control) {
        return Err(invalid(format!(
            "unusable remote url `{}`",
            target.remote_url
        )));
    }
    let branch_ref = format!("refs/heads/{}", target.branch);
    if target.branch.starts_with('-')
        || gix::validate::reference::name(branch_ref.as_str().into()).is_err()
    {
        return Err(invalid(format!("unusable branch name `{}`", target.branch)));
    }
    Ok((url.to_owned(), branch_ref))
}

/// `bytes` with `edits` (sorted by first line) applied. A replacement takes
/// the line ending the file uses at that spot, and keeps a missing final
/// newline missing.
fn apply_edits(path: &str, bytes: &[u8], edits: &[&LineEdit]) -> Result<Vec<u8>> {
    if bytes[..bytes.len().min(BINARY_PROBE)].contains(&0) {
        return Err(invalid(format!("`{path}` is binary")));
    }
    let lines = split_lines(bytes);
    let count = u32::try_from(lines.len()).unwrap_or(u32::MAX);
    let mut previous_last = 0;
    for edit in edits {
        if edit.first == 0 || edit.first > edit.last || edit.last > count {
            return Err(invalid(format!(
                "`{path}` has no lines {}-{} (it has {count})",
                edit.first, edit.last
            )));
        }
        if edit.first <= previous_last {
            return Err(invalid(format!(
                "suggestions on `{path}` overlap at line {}",
                edit.first
            )));
        }
        previous_last = edit.last;
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut next = 0usize;
    for edit in edits {
        let first = edit.first as usize - 1;
        let last = edit.last as usize - 1;
        for line in &lines[next..first] {
            out.extend_from_slice(line);
        }
        let ending = line_ending(lines[last]);
        let separator: &[u8] = if line_ending(lines[first]) == b"\r\n" {
            b"\r\n"
        } else {
            b"\n"
        };
        for (ix, line) in edit.lines.iter().enumerate() {
            out.extend_from_slice(line.as_bytes());
            out.extend_from_slice(if ix + 1 == edit.lines.len() {
                ending
            } else {
                separator
            });
        }
        next = last + 1;
    }
    for line in &lines[next..] {
        out.extend_from_slice(line);
    }
    Ok(out)
}

/// The lines of `bytes`, each with its ending; no empty line after a final
/// newline.
fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (ix, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' {
            lines.push(&bytes[start..=ix]);
            start = ix + 1;
        }
    }
    if start < bytes.len() {
        lines.push(&bytes[start..]);
    }
    lines
}

fn line_ending(line: &[u8]) -> &'static [u8] {
    if line.ends_with(b"\r\n") {
        b"\r\n"
    } else if line.ends_with(b"\n") {
        b"\n"
    } else {
        b""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(first: u32, last: u32, lines: &[&str]) -> LineEdit {
        LineEdit {
            path: "a.txt".to_owned(),
            first,
            last,
            lines: lines.iter().map(|&l| l.to_owned()).collect(),
        }
    }

    fn apply(text: &str, edits: &[LineEdit]) -> Result<String> {
        let edits: Vec<&LineEdit> = edits.iter().collect();
        apply_edits("a.txt", text.as_bytes(), &edits).map(|b| String::from_utf8(b).unwrap())
    }

    #[test]
    fn edits_replace_their_lines_and_keep_the_rest() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(apply(text, &[edit(2, 2, &["B"])]).unwrap(), "a\nB\nc\nd\n");
        assert_eq!(
            apply(text, &[edit(2, 3, &["x", "y", "z"])]).unwrap(),
            "a\nx\ny\nz\nd\n"
        );
        assert_eq!(
            apply(text, &[edit(1, 1, &[]), edit(4, 4, &["D"])]).unwrap(),
            "b\nc\nD\n"
        );
    }

    #[test]
    fn line_endings_and_a_missing_final_newline_are_kept() {
        assert_eq!(
            apply("a\r\nb\r\n", &[edit(1, 1, &["x", "y"])]).unwrap(),
            "x\r\ny\r\nb\r\n"
        );
        assert_eq!(
            apply("a\nb", &[edit(2, 2, &["x", "y"])]).unwrap(),
            "a\nx\ny"
        );
    }

    #[test]
    fn bad_ranges_overlaps_and_binaries_are_refused() {
        let text = "a\nb\nc\n";
        assert!(apply(text, &[edit(0, 1, &["x"])]).is_err());
        assert!(apply(text, &[edit(3, 4, &["x"])]).is_err());
        assert!(apply(text, &[edit(2, 1, &["x"])]).is_err());
        assert!(apply(text, &[edit(1, 2, &["x"]), edit(2, 3, &["y"])]).is_err());
        let binary = [b'a', 0, b'\n'];
        assert!(apply_edits("b", &binary, &[&edit(1, 1, &["x"])]).is_err());
    }

    #[test]
    fn paths_from_the_forge_name_only_tree_files() {
        for path in ["src/a.rs", "a", "docs/intro.md", "a/.gitignore"] {
            assert!(check_path(path).is_ok(), "{path}");
        }
        for path in [
            "",
            "/a",
            "a//b",
            "../a",
            "a/./b",
            ".git/config",
            "a\\b",
            "a/.GIT/x",
        ] {
            assert!(check_path(path).is_err(), "{path}");
        }
    }
}
