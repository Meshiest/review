//! Worktrees on demand, through the `git` CLI: in a store on this machine,
//! or in a repository on another one over `ssh` ([`RemoteRepo`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::fetch::PrRefs;
use crate::git::GitCommand;
use crate::shell;
use crate::store::{Store, pr_head_ref};

const TIMEOUT: Duration = Duration::from_secs(120);
/// A clone or fetch over the network may take a while on a large repository.
const NETWORK_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RemoveMode {
    /// Fails when the worktree has local changes.
    Clean,
    /// Removes the worktree whatever it holds.
    Discard,
}

/// One entry of `git worktree list --porcelain`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Worktree {
    /// As git prints it: forward slashes, also on Windows.
    pub path: String,
    /// The checked-out commit; `None` for a bare repository's own entry.
    pub head: Option<String>,
    /// `refs/heads/...` when a branch is checked out.
    pub branch: Option<String>,
    pub detached: bool,
    /// The repository itself, when it is bare.
    pub bare: bool,
}

impl Worktree {
    /// Whether this entry is the worktree at `path`, however each spells it.
    #[must_use]
    pub fn is_at(&self, path: &str) -> bool {
        same_path(&self.path, path)
    }
}

/// Forward slashes, `.` and `..` resolved, no trailing slash; lowercase on
/// Windows, whose paths ignore case.
fn normalized(path: &str) -> String {
    let path = path.replace('\\', "/");
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "." => {}
            ".." if parts
                .last()
                .is_some_and(|last| !last.is_empty() && *last != "..") =>
            {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let path = parts.join("/");
    let path = path.trim_end_matches('/');
    if cfg!(windows) {
        path.to_lowercase()
    } else {
        path.to_owned()
    }
}

fn same_path(a: &str, b: &str) -> bool {
    normalized(a) == normalized(b)
}

/// Parses `git worktree list --porcelain`: blocks of `key value` lines, one
/// per worktree, separated by blank lines.
#[must_use]
pub fn parse_worktree_list(text: &str) -> Vec<Worktree> {
    let mut list = Vec::new();
    let mut current: Option<Worktree> = None;
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "worktree" => {
                list.extend(current.take());
                current = Some(Worktree {
                    path: value.to_owned(),
                    ..Worktree::default()
                });
            }
            _ => {
                let Some(entry) = current.as_mut() else {
                    continue;
                };
                match key {
                    "HEAD" => entry.head = Some(value.to_owned()),
                    "branch" => entry.branch = Some(value.to_owned()),
                    "detached" => entry.detached = true,
                    "bare" => entry.bare = true,
                    _ => {}
                }
            }
        }
    }
    list.extend(current);
    list
}

/// Checks `pr_ref` (for example `refs/review/pr/7`) out detached at `path`.
///
/// Inside a registered clone, `path` must be git-ignored there so the clone's
/// status stays clean; otherwise this refuses before touching anything.
/// Blocks on git: call it off the UI thread.
///
/// # Errors
/// [`Error::WorktreeNotIgnored`], or git's own failure (path exists, ref
/// missing).
pub fn add_worktree(store: &Store, pr_ref: &str, path: &Path) -> Result<()> {
    check_ref(pr_ref)?;
    let path = absolute(path)?;
    if let Some(clone) = store.work_dir() {
        require_ignored(clone, &path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    GitCommand::new(&control_dir(store), TIMEOUT)
        .args(["worktree", "add", "--detach", "--"])
        .args([path.as_os_str(), pr_ref.as_ref()])
        .run_checked()?;
    Ok(())
}

/// Clones `url` into `path`, a missing or empty directory, with a working
/// tree. Blocks on the network: call it off the UI thread.
///
/// # Errors
/// If `url` is unusable or git fails (`path` holds files).
pub fn clone_repo(url: &str, path: &Path) -> Result<()> {
    check_url(url)?;
    let path = absolute(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| Error::InvalidArgument(format!("no parent for {}", path.display())))?;
    std::fs::create_dir_all(parent)?;
    GitCommand::new(parent, NETWORK_TIMEOUT)
        .args(["-c", "gc.auto=0", "clone", "--no-tags", "--", url])
        .args([path.as_os_str()])
        .run_checked()?;
    Ok(())
}

/// The directory a worktree shares with its repository (`info/exclude`
/// lives there), absolute.
///
/// # Errors
/// If `path` is not in a repository.
pub fn common_dir(path: &Path) -> Result<PathBuf> {
    let output = GitCommand::new(path, TIMEOUT)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .capture_stdout()
        .run_checked()?;
    Ok(PathBuf::from(output.stdout.trim()))
}

/// Removes a worktree made by [`add_worktree`].
///
/// # Errors
/// If the worktree has local changes and `mode` is [`RemoveMode::Clean`], or
/// git fails.
pub fn remove_worktree(store: &Store, path: &Path, mode: RemoveMode) -> Result<()> {
    let path = absolute(path)?;
    let mut command = GitCommand::new(&control_dir(store), TIMEOUT).args(["worktree", "remove"]);
    if mode == RemoveMode::Discard {
        command = command.args(["--force"]);
    }
    command
        .args(["--"])
        .args([path.as_os_str()])
        .run_checked()?;
    Ok(())
}

/// The store's worktrees, its own entry first.
///
/// # Errors
/// If git fails.
pub fn list_worktrees(store: &Store) -> Result<Vec<Worktree>> {
    let output = GitCommand::new(&control_dir(store), TIMEOUT)
        .args(["worktree", "list", "--porcelain"])
        .capture_stdout()
        .run_checked()?;
    Ok(parse_worktree_list(&output.stdout))
}

/// Moves the worktree at `path` to `rev`, detached.
///
/// # Errors
/// If the worktree has changes git will not carry over, or git fails.
pub fn checkout_detached(path: &Path, rev: &str) -> Result<()> {
    check_ref(rev)?;
    GitCommand::new(path, TIMEOUT)
        .args(["checkout", "--detach", rev, "--"])
        .run_checked()?;
    Ok(())
}

/// Whether the worktree at `path` has uncommitted changes or untracked files.
///
/// # Errors
/// If git fails.
pub fn has_changes(path: &Path) -> Result<bool> {
    let output = GitCommand::new(path, TIMEOUT)
        .args(["status", "--porcelain", "--untracked-files=normal"])
        .capture_stdout()
        .run_checked()?;
    Ok(!output.stdout.trim().is_empty())
}

fn check_ref(rev: &str) -> Result<()> {
    if rev.is_empty() || rev.starts_with('-') || rev.contains(char::is_control) {
        return Err(Error::InvalidArgument(format!("unusable ref `{rev}`")));
    }
    Ok(())
}

fn control_dir(store: &Store) -> PathBuf {
    store
        .work_dir()
        .unwrap_or_else(|| store.git_dir())
        .to_owned()
}

fn absolute(path: &Path) -> Result<PathBuf> {
    Ok(std::path::absolute(path)?)
}

/// Whether git would ignore `path`, which need not exist yet: a directory
/// pattern like `.claude/worktrees/` matches by its parent directory.
fn require_ignored(clone: &Path, path: &Path) -> Result<()> {
    let Ok(relative) = path.strip_prefix(clone) else {
        return Ok(());
    };
    let probe = relative
        .join("probe")
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    let status = GitCommand::new(clone, TIMEOUT)
        .args(["check-ignore", "-q", "--no-index", "--"])
        .args([probe])
        .run()?
        .status;
    ignored_status(status.code(), path.display().to_string(), clone)
}

fn ignored_status(code: Option<i32>, path: String, clone: &Path) -> Result<()> {
    match code {
        Some(0) => Ok(()),
        Some(1) => Err(Error::WorktreeNotIgnored {
            path: PathBuf::from(path),
            clone: clone.to_owned(),
        }),
        other => Err(Error::GitFailed {
            args: "check-ignore".into(),
            status: other.map_or_else(|| "killed".to_owned(), |code| code.to_string()),
            stderr: String::new(),
        }),
    }
}

/// A host name from the user's SSH config, safe to put on `ssh`'s command
/// line: no leading `-` (an option) and nothing a shell or `ssh` would split.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SshHost(String);

impl SshHost {
    /// # Errors
    /// [`Error::InvalidArgument`] for a name `ssh` could read as an option or
    /// as more than one word.
    pub fn new(name: &str) -> Result<Self> {
        let usable = !name.is_empty()
            && !name.starts_with('-')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':'));
        if !usable {
            return Err(Error::InvalidArgument(format!(
                "unusable host name `{name}`"
            )));
        }
        Ok(Self(name.to_owned()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SshHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A repository on another machine, worked on with plain `git` through
/// `ssh <host> -- git -C <dir> ...`. Nothing is installed there. Paths are
/// POSIX paths on that machine; `~/` is its home. Every call blocks on the
/// network: run it off the UI thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteRepo {
    host: SshHost,
    dir: String,
}

impl RemoteRepo {
    #[must_use]
    pub fn new(host: SshHost, dir: &str) -> Self {
        Self {
            host,
            dir: dir.trim_end_matches('/').to_owned(),
        }
    }

    #[must_use]
    pub fn host(&self) -> &SshHost {
        &self.host
    }

    /// The repository's directory, as given.
    #[must_use]
    pub fn dir(&self) -> &str {
        &self.dir
    }

    fn git_in(&self, dir: &str, timeout: Duration) -> GitCommand {
        GitCommand::over_ssh(self.host.as_str(), shell::home_relative(dir), timeout)
    }

    fn git(&self, timeout: Duration) -> GitCommand {
        self.git_in(&self.dir, timeout)
    }

    /// Whether a repository is there. `false` also when the directory is
    /// missing; an `ssh` failure is an error.
    ///
    /// # Errors
    /// When the host cannot be reached.
    pub fn exists(&self) -> Result<bool> {
        let output = self
            .git(TIMEOUT)
            .args(["rev-parse", "--git-dir"])
            .capture_stdout()
            .run()?;
        match output.status.code() {
            Some(0) => Ok(true),
            // ssh's own failures (unreachable, refused key) exit 255.
            Some(255) => Err(Error::GitFailed {
                args: format!("rev-parse (on {})", self.host),
                status: output.status.to_string(),
                stderr: output.stderr,
            }),
            _ => Ok(false),
        }
    }

    /// Clones `url` there as a bare repository, creating its parents.
    ///
    /// # Errors
    /// If `url` is unusable or git fails.
    pub fn clone_bare(&self, url: &str) -> Result<()> {
        check_url(url)?;
        let target = shell::home_relative(&self.dir).to_owned();
        self.git_in("~", NETWORK_TIMEOUT)
            .args(["-c", "gc.auto=0", "clone", "--bare", "--no-tags", "--", url])
            .args([target])
            .run_checked()?;
        Ok(())
    }

    /// Clones `url` there with a working tree, creating its parents.
    ///
    /// # Errors
    /// If `url` is unusable or git fails.
    pub fn clone_into(&self, url: &str) -> Result<()> {
        check_url(url)?;
        let target = shell::home_relative(&self.dir).to_owned();
        self.git_in("~", NETWORK_TIMEOUT)
            .args(["-c", "gc.auto=0", "clone", "--no-tags", "--", url])
            .args([target])
            .run_checked()?;
        Ok(())
    }

    /// Fetches pull request `number`'s head from `url` into
    /// `refs/review/pr/N` and returns the commit it names.
    ///
    /// # Errors
    /// If `url` is unusable or git fails.
    pub fn fetch_pr_head(&self, url: &str, refs: PrRefs, number: u64) -> Result<String> {
        check_url(url)?;
        let local = pr_head_ref(number);
        self.git(NETWORK_TIMEOUT)
            .args([
                "-c",
                "gc.auto=0",
                "-c",
                "maintenance.auto=false",
                "fetch",
                "--no-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--",
                url,
            ])
            .args([format!("+{}:{local}", refs.head_source(number))])
            .run_checked()?;
        self.resolve(&local)
    }

    /// The commit `rev` names there.
    ///
    /// # Errors
    /// If it does not resolve.
    pub fn resolve(&self, rev: &str) -> Result<String> {
        check_ref(rev)?;
        let output = self
            .git(TIMEOUT)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options"])
            .args([format!("{rev}^{{commit}}")])
            .capture_stdout()
            .run()?;
        let id = output.stdout.trim();
        if !output.status.success() || id.is_empty() {
            return Err(Error::RevisionNotFound(rev.to_owned()));
        }
        Ok(id.to_owned())
    }

    /// Whether this is a bare repository.
    ///
    /// # Errors
    /// If git fails.
    pub fn is_bare(&self) -> Result<bool> {
        let output = self
            .git(TIMEOUT)
            .args(["rev-parse", "--is-bare-repository"])
            .capture_stdout()
            .run_checked()?;
        Ok(output.stdout.trim() == "true")
    }

    /// Refuses a worktree at `relative`, a path inside this clone's working
    /// tree (such as `.claude/worktrees/pr-4`), that git does not ignore.
    ///
    /// # Errors
    /// [`Error::WorktreeNotIgnored`], or git's failure.
    pub fn require_ignored(&self, relative: &str) -> Result<()> {
        let probe = format!("{}/probe", relative.trim_end_matches('/'));
        let status = self
            .git(TIMEOUT)
            .args(["check-ignore", "-q", "--no-index", "--"])
            .args([probe])
            .run()?
            .status;
        ignored_status(
            status.code(),
            format!("{}/{relative}", self.dir),
            Path::new(&self.dir),
        )
    }

    /// Checks `pr_ref` out detached at `path` there.
    ///
    /// # Errors
    /// If git fails (path exists, ref missing).
    pub fn add_worktree(&self, pr_ref: &str, path: &str) -> Result<()> {
        check_ref(pr_ref)?;
        self.git(TIMEOUT)
            .args(["worktree", "add", "--detach", "--"])
            .args([shell::home_relative(path), pr_ref])
            .run_checked()?;
        Ok(())
    }

    /// Its worktrees, its own entry first. Paths are absolute, as git
    /// prints them.
    ///
    /// # Errors
    /// If git fails.
    pub fn worktrees(&self) -> Result<Vec<Worktree>> {
        let output = self
            .git(TIMEOUT)
            .args(["worktree", "list", "--porcelain"])
            .capture_stdout()
            .run_checked()?;
        Ok(parse_worktree_list(&output.stdout))
    }

    /// Removes the worktree at `path` (as [`Self::worktrees`] lists it).
    ///
    /// # Errors
    /// If it has changes and `mode` is [`RemoveMode::Clean`], or git fails.
    pub fn remove_worktree(&self, path: &str, mode: RemoveMode) -> Result<()> {
        let mut command = self.git(TIMEOUT).args(["worktree", "remove"]);
        if mode == RemoveMode::Discard {
            command = command.args(["--force"]);
        }
        command
            .args(["--", shell::home_relative(path)])
            .run_checked()?;
        Ok(())
    }

    /// Deletes a ref, such as a fetched pull request head.
    ///
    /// # Errors
    /// If git fails.
    pub fn delete_ref(&self, name: &str) -> Result<()> {
        check_ref(name)?;
        self.git(TIMEOUT)
            .args(["update-ref", "-d", "--", name])
            .run_checked()?;
        Ok(())
    }

    /// Moves the worktree at `path` there to `rev`, detached.
    ///
    /// # Errors
    /// If it has changes git will not carry over, or git fails.
    pub fn checkout_detached(&self, path: &str, rev: &str) -> Result<()> {
        check_ref(rev)?;
        self.git_in(path, TIMEOUT)
            .args(["checkout", "--detach", rev, "--"])
            .run_checked()?;
        Ok(())
    }

    /// Whether the worktree at `path` there has uncommitted changes.
    ///
    /// # Errors
    /// If git fails.
    pub fn has_changes(&self, path: &str) -> Result<bool> {
        let output = self
            .git_in(path, TIMEOUT)
            .args(["status", "--porcelain", "--untracked-files=normal"])
            .capture_stdout()
            .run_checked()?;
        Ok(!output.stdout.trim().is_empty())
    }
}

fn check_url(url: &str) -> Result<()> {
    if url.is_empty() || url.starts_with('-') || url.contains(char::is_control) {
        return Err(Error::InvalidArgument(format!(
            "unusable remote url `{url}`"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_lists_parse() {
        let text = "worktree /home/u/.cache/review/github.com/o/r\nbare\n\n\
                    worktree /home/u/.cache/review/github.com/o/r.worktrees/pr-4\n\
                    HEAD b4c0f065a76a078b65e2203939f82c430b7f4888\ndetached\n\n\
                    worktree C:/src/r\nHEAD 0123\nbranch refs/heads/main\nprunable gone\n";
        let list = parse_worktree_list(text);
        assert_eq!(list.len(), 3);
        assert!(list[0].bare && list[0].head.is_none());
        assert_eq!(
            list[1].head.as_deref(),
            Some("b4c0f065a76a078b65e2203939f82c430b7f4888")
        );
        assert!(list[1].detached && !list[1].bare);
        assert_eq!(list[2].branch.as_deref(), Some("refs/heads/main"));
        assert!(parse_worktree_list("").is_empty());
    }

    #[test]
    fn paths_compare_across_spellings() {
        let entry = Worktree {
            path: "C:/src/r/.claude/worktrees/pr-4".into(),
            ..Worktree::default()
        };
        assert!(entry.is_at("C:/src/r/.claude/worktrees/pr-4/"));
        if cfg!(windows) {
            assert!(entry.is_at(r"c:\src\r\.claude\worktrees\pr-4"));
        }
        assert!(!entry.is_at("C:/src/r/.claude/worktrees/pr-40"));
        assert!(entry.is_at("C:/src/app/../r/./.claude/worktrees/pr-4"));
    }

    #[test]
    fn host_names_that_ssh_could_misread_are_refused() {
        for good in ["devbox", "git.example.org", "user@box", "box_2"] {
            assert!(SshHost::new(good).is_ok(), "{good}");
        }
        for bad in ["", "-oProxyCommand=x", "a b", "a;b", "a$(b)", "a\nb"] {
            assert!(SshHost::new(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn refs_and_urls_that_git_could_misread_are_refused() {
        assert!(check_ref("refs/review/pr/4").is_ok());
        assert!(check_ref("--upload-pack=x").is_err());
        assert!(check_url("git@github.com:o/r.git").is_ok());
        assert!(check_url("-oProxyCommand").is_err());
    }
}
