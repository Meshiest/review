//! A pull request's worktree on a machine: created at the pull request's
//! head on demand, listed and removed. Where it goes is a [`Placement`]:
//! anything that starts an agent uses a repository the app owns (beside the
//! bare store under the data dir here, the scratch clone under
//! `~/.cache/review` over SSH), so the checkout is never inside a folder the
//! user trusts; a plain worktree may go in the user's clone instead
//! (`.claude/worktrees/pr-N`), and "Review with agent" puts it off the
//! directory its workspace names, once a terminal has cloned there.
//! Over SSH it is worked on with plain `git`.
//!
//! Everything here blocks on git, the disk and the network; the app runs it
//! on a background thread.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use forge::model::RepoRef;
use repo::{
    FetchEvent, FetchRequest, PrRefs, RemoteRepo, RemoveMode, Worktree, pr_head_ref, shell,
};

use crate::claude_review::command::{CONTEXT_DIR, CONTEXT_FILE};
use crate::machines::{self, Machine, Place};

/// Where a pull request comes from, as git fetches it.
#[derive(Clone, Debug)]
pub struct PrSource {
    pub repo: RepoRef,
    pub number: u64,
    pub fetch: FetchRequest,
}

impl PrSource {
    #[must_use]
    pub fn new(repo: RepoRef, fetch: FetchRequest) -> Self {
        Self {
            repo,
            number: fetch.number,
            fetch,
        }
    }

    fn url(&self) -> &str {
        &self.fetch.remote_url
    }

    fn refs(&self) -> PrRefs {
        self.fetch.refs
    }
}

/// A worktree checked out at a pull request's head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub machine: String,
    pub local: bool,
    /// Absolute, on that machine.
    pub path: String,
    /// The commit it has checked out.
    pub head: String,
    /// Why it is not at the head it was asked for (local changes kept).
    pub note: Option<String>,
}

impl WorktreeInfo {
    /// The head's first seven characters.
    #[must_use]
    pub fn short_head(&self) -> &str {
        self.head.get(..7).unwrap_or(&self.head)
    }
}

/// Where a pull request's worktree goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// In a repository the app owns: the caller's app-owned store here, the
    /// scratch clone over SSH. Whatever starts an agent in the worktree uses
    /// this, so it never inherits the trust of the user's own clone.
    AppOwned,
    /// In the user's clone when there is one (`.claude/worktrees/pr-N`), for
    /// a plain worktree the user asked for.
    InClone,
}

/// One step of making a worktree and starting a program in it, as the
/// launch tab's checklist shows them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// Reaching the machine over SSH.
    Connect,
    /// The app's own clone of the repository, made once.
    Clone,
    /// The workspace's directory has no clone: a terminal makes it.
    CloneInTerminal,
    Fetch,
    /// Adding the worktree, or moving it to the new head.
    Worktree,
    /// Comparing the pull request's agent config with its merge base.
    Config,
    /// The review's context file and MCP config.
    Context,
    StartAgent,
    StartCommand,
}

impl Step {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Connect => "Connecting over SSH",
            Self::Clone => "Cloning the repository",
            Self::CloneInTerminal => "Cloning the repository into its directory",
            Self::Fetch => "Fetching the pull request",
            Self::Worktree => "Creating the worktree",
            Self::Config => "Checking the agent config",
            Self::Context => "Writing the review context",
            Self::StartAgent => "Starting the agent",
            Self::StartCommand => "Starting the command",
        }
    }
}

/// Reports the step the work has reached.
pub type Progress<'a> = &'a mut dyn FnMut(Step);

/// The worktree's folder name in a repository's worktree directory.
#[must_use]
pub fn leaf(number: u64) -> String {
    format!("pr-{number}")
}

// ---- this machine

/// Fetches the pull request into `objects` and makes sure its worktree is
/// checked out at the fetched head: added when missing, moved when it has
/// no local changes, left alone (with a note) when it has.
///
/// # Errors
/// Fetch and git failures, as text for the header.
pub fn ensure_local(
    objects: &repo::Store,
    source: &PrSource,
    progress: Progress<'_>,
) -> Result<WorktreeInfo, String> {
    progress(Step::Fetch);
    let fetched = fetch_local(objects, &source.fetch)?;
    let head = fetched.head.to_string();
    let path = objects.worktree_path(source.number);
    let path_text = path.to_string_lossy().into_owned();
    let existing = repo::list_worktrees(objects)
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|entry| entry.is_at(&path_text));
    let pr_ref = pr_head_ref(source.number);
    let note = match existing {
        None => {
            progress(Step::Worktree);
            repo::add_worktree(objects, &pr_ref, &path).map_err(|e| e.to_string())?;
            None
        }
        Some(entry) if entry.head.as_deref() == Some(head.as_str()) => None,
        Some(_) => {
            if repo::has_changes(&path).map_err(|e| e.to_string())? {
                Some("has local changes, left at its commit".to_owned())
            } else {
                progress(Step::Worktree);
                repo::checkout_detached(&path, &pr_ref).map_err(|e| e.to_string())?;
                None
            }
        }
    };
    let head = match &note {
        Some(_) => worktree_head(
            &repo::list_worktrees(objects).map_err(|e| e.to_string())?,
            &path_text,
        )
        .unwrap_or(head),
        None => head,
    };
    Ok(WorktreeInfo {
        machine: machines::THIS_PC.to_owned(),
        local: true,
        path: std::path::absolute(&path)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned(),
        head,
        note,
    })
}

fn worktree_head(list: &[Worktree], path: &str) -> Option<String> {
    list.iter().find(|entry| entry.is_at(path))?.head.clone()
}

/// Fetches the pull request's head and base into `objects` and waits for it.
///
/// # Errors
/// Fetch failures, as text.
pub fn fetch_local(objects: &repo::Store, request: &FetchRequest) -> Result<repo::Fetched, String> {
    let handle = objects.fetch_pr(request).map_err(|e| e.to_string())?;
    loop {
        match handle.events.recv() {
            Ok(FetchEvent::Progress(_)) => {}
            Ok(FetchEvent::Finished(result)) => return result.map_err(|e| e.to_string()),
            Err(mpsc::RecvError) => return Err("the fetch stopped".to_owned()),
        }
    }
}

/// The worktrees of `objects` that hold pull requests (`pr-N`).
///
/// # Errors
/// If git fails.
pub fn list_local(objects: &repo::Store) -> Result<Vec<Worktree>, String> {
    Ok(repo::list_worktrees(objects)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(is_pr_worktree)
        .collect())
}

/// Removes pull request `number`'s worktree from `objects`, changes and all.
///
/// # Errors
/// If git fails.
pub fn remove_local(objects: &repo::Store, number: u64) -> Result<(), String> {
    repo::remove_worktree(objects, &objects.worktree_path(number), RemoveMode::Discard)
        .map_err(|e| e.to_string())
}

fn is_pr_worktree(entry: &Worktree) -> bool {
    !entry.bare
        && entry
            .path
            .rsplit(['/', '\\'])
            .next()
            .is_some_and(|name| name.starts_with("pr-"))
}

// ---- over SSH

/// The repository a machine works on for `repo`, and whether it is the
/// app's scratch clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteClone {
    pub repo: RemoteRepo,
    pub scratch: bool,
}

/// For [`Placement::InClone`] the machine's configured clone of `repo` when
/// it exists there; otherwise the scratch clone under `~/.cache/review`
/// (which may not exist yet).
///
/// # Errors
/// When the machine is this PC, or cannot be reached.
pub fn remote_clone(
    machine: &Machine,
    repo: &RepoRef,
    placement: Placement,
) -> Result<RemoteClone, String> {
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC has no remote clone".to_owned());
    };
    if placement == Placement::InClone
        && let Some(dir) = machine.configured_clone(repo)
    {
        let clone = RemoteRepo::new(host.clone(), &dir);
        if clone.exists().map_err(|e| e.to_string())? {
            return Ok(RemoteClone {
                repo: clone,
                scratch: false,
            });
        }
    }
    Ok(RemoteClone {
        repo: RemoteRepo::new(host.clone(), &machines::scratch_clone(repo)),
        scratch: true,
    })
}

/// Makes sure the pull request's worktree exists on `machine` at its head:
/// clones the scratch repository over the forge's URL when missing, fetches
/// the head, adds or moves the worktree. A worktree inside a configured
/// clone must be git-ignored there.
///
/// # Errors
/// Git and `ssh` failures, as text for the header.
pub fn ensure_remote(
    machine: &Machine,
    source: &PrSource,
    placement: Placement,
    progress: Progress<'_>,
) -> Result<WorktreeInfo, String> {
    let err = |e: repo::Error| e.to_string();
    progress(Step::Connect);
    let clone = remote_clone(machine, &source.repo, placement)?;
    if clone.scratch && !clone.repo.exists().map_err(err)? {
        progress(Step::Clone);
        clone.repo.clone_bare(source.url()).map_err(err)?;
    }
    ensure_remote_in(machine, &clone.repo, source, progress)
}

/// The worktree in `repo`, which exists: fetches the head, then adds or
/// moves the worktree.
fn ensure_remote_in(
    machine: &Machine,
    repo: &RemoteRepo,
    source: &PrSource,
    progress: Progress<'_>,
) -> Result<WorktreeInfo, String> {
    let err = |e: repo::Error| e.to_string();
    progress(Step::Fetch);
    let head = repo
        .fetch_pr_head(source.url(), source.refs(), source.number)
        .map_err(err)?;
    let list = repo.worktrees().map_err(err)?;
    let path = remote_worktree_path(repo, &list, source.number)?;
    if !repo.is_bare().map_err(err)? {
        repo.require_ignored(&format!(".claude/worktrees/{}", leaf(source.number)))
            .map_err(err)?;
    }
    let pr_ref = pr_head_ref(source.number);
    let note = match list.iter().find(|entry| entry.is_at(&path)) {
        None => {
            progress(Step::Worktree);
            repo.add_worktree(&pr_ref, &path).map_err(err)?;
            None
        }
        Some(entry) if entry.head.as_deref() == Some(head.as_str()) => None,
        Some(_) if repo.has_changes(&path).map_err(err)? => {
            Some("has local changes, left at its commit".to_owned())
        }
        Some(_) => {
            progress(Step::Worktree);
            repo.checkout_detached(&path, &pr_ref).map_err(err)?;
            None
        }
    };
    let head = match &note {
        Some(_) => worktree_head(&repo.worktrees().map_err(err)?, &path).unwrap_or(head),
        None => head,
    };
    Ok(WorktreeInfo {
        machine: machine.name.clone(),
        local: false,
        path,
        head,
        note,
    })
}

// ---- in a workspace's directory

/// A path on this PC with a leading `~` as the home directory.
#[must_use]
pub fn expand_home(path: &str) -> PathBuf {
    let rest = match path {
        "~" => Some(""),
        _ => path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")),
    };
    match (rest, dirs::home_dir()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(path),
    }
}

/// Whether `dir` is missing or empty, so the repository is cloned into it
/// first.
///
/// # Errors
/// When the directory cannot be read.
pub fn needs_clone_local(dir: &Path) -> Result<bool, String> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => Ok(entries.next().is_none()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("{}: {error}", dir.display())),
    }
}

/// Whether `dir` on `machine` is not a repository yet, so the repository is
/// cloned into it first.
///
/// # Errors
/// When the machine is this PC, or `ssh` fails.
pub fn needs_clone_remote(machine: &Machine, dir: &str) -> Result<bool, String> {
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC is not reached over SSH".to_owned());
    };
    let exists = RemoteRepo::new(host.clone(), dir)
        .exists()
        .map_err(|e| e.to_string())?;
    Ok(!exists)
}

/// The pull request's worktree in the clone at `dir`, as [`ensure_local`]
/// makes one: at `.claude/worktrees/pr-N`, which is added to the clone's
/// `info/exclude` first. The clone is made beforehand, in a terminal the
/// user sees (see [`needs_clone_local`]).
///
/// # Errors
/// Fetch and git failures, as text for the header.
pub fn ensure_in_dir_local(
    dir: &Path,
    source: &PrSource,
    progress: Progress<'_>,
) -> Result<(repo::Store, WorktreeInfo), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", dir.display());
    let clone = repo::Store::open(dir).map_err(|e| at(&e))?;
    if clone.work_dir().is_some() {
        let common = repo::common_dir(dir).map_err(|e| at(&e))?;
        exclude_local(&common, WORKTREES_PATTERN).map_err(|e| at(&e))?;
    }
    let info = ensure_local(&clone, source, progress)?;
    Ok((clone, info))
}

/// [`ensure_in_dir_local`] on another machine, over SSH, in the repository
/// at `dir` there (see [`needs_clone_remote`]).
///
/// # Errors
/// Fetch, git and `ssh` failures, as text for the header.
pub fn ensure_in_dir_remote(
    machine: &Machine,
    source: &PrSource,
    dir: &str,
    progress: Progress<'_>,
) -> Result<WorktreeInfo, String> {
    let err = |e: repo::Error| e.to_string();
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC is not reached over SSH".to_owned());
    };
    let repo = RemoteRepo::new(host.clone(), dir);
    if !repo.is_bare().map_err(err)? {
        let args = [shell::home_relative(dir), WORKTREES_PATTERN];
        remote_sh(host, EXCLUDE_SCRIPT, &args, None)?;
    }
    ensure_remote_in(machine, &repo, source, progress)
}

/// What keeps worktrees out of a clone's status.
const WORKTREES_PATTERN: &str = ".claude/worktrees/";
/// What keeps the context folder out of a worktree's status.
const CONTEXT_PATTERN: &str = ".review/";

/// Adds `pattern` to `<common>/info/exclude` unless a line already is it.
fn exclude_local(common: &Path, pattern: &str) -> std::io::Result<()> {
    let file = common.join("info").join("exclude");
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error),
    };
    if text.lines().any(|line| line.trim() == pattern) {
        return Ok(());
    }
    std::fs::create_dir_all(common.join("info"))?;
    let gap = if text.is_empty() || text.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    std::fs::write(&file, format!("{text}{gap}{pattern}\n"))
}

/// Writes `context` to the worktree's [`CONTEXT_FILE`], and with `exclude`
/// keeps its folder out of git.
///
/// # Errors
/// Disk and git failures, as text.
pub fn write_review_files_local(
    worktree: &Path,
    context: &str,
    exclude: bool,
) -> Result<(), String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", worktree.display());
    // The checkout is the pull request's: a `.review` it ships as a link
    // would send the writes somewhere else.
    let dir = worktree.join(CONTEXT_DIR);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(at(&format!(
                "{CONTEXT_DIR} in the checkout is not a folder"
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&dir).map_err(|e| at(&e))?;
        }
        Err(error) => return Err(at(&error)),
    }
    write_fresh(&worktree.join(CONTEXT_FILE), context, false).map_err(|e| at(&e))?;
    if exclude {
        let common = repo::common_dir(worktree).map_err(|e| at(&e))?;
        exclude_local(&common, CONTEXT_PATTERN).map_err(|e| at(&e))?;
    }
    Ok(())
}

/// Writes `text` to a new file at `path`, replacing a plain file there and
/// refusing anything else (a link the checkout ships). A `private` file is
/// readable by its owner only on Unix; on Windows it takes its folder's
/// permissions.
fn write_fresh(path: &Path, text: &str, private: bool) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => std::fs::remove_file(path)?,
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "{} in the checkout is not a file",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(not(unix))]
    let _ = private;
    options.open(path)?.write_all(text.as_bytes())
}

/// [`write_review_files_local`] on another machine: the text goes on
/// `ssh`'s stdin, never on its command line.
///
/// # Errors
/// `ssh` failures, as text.
pub fn write_review_files_remote(
    machine: &Machine,
    worktree: &str,
    context: &str,
) -> Result<(), String> {
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC is not reached over SSH".to_owned());
    };
    let worktree = shell::home_relative(worktree);
    let args = [worktree, CONTEXT_PATTERN, CONTEXT_DIR, CONTEXT_FILE];
    remote_sh(host, &context_script(), &args, Some(context.as_bytes()))?;
    Ok(())
}

/// Where MCP configs go under a remote user's home, a folder only they can
/// open.
const REMOTE_PRIVATE_DIR: &str = ".cache/review/mcp";

/// Writes stdin to a new `$HOME/$1/$2` that only its owner can read, in a
/// folder only its owner can open, and prints its path. `set -C` refuses a
/// file put there in between.
const PRIVATE_SCRIPT: &str = r#"umask 077; d="$HOME/$1" && mkdir -p "$d" && chmod 700 "$d" && f="$d/$2" && rm -f "$f" && set -C && cat > "$f" && printf '%s' "$f""#;

/// Removes `$HOME/$1/$2`.
const REMOVE_SCRIPT: &str = r#"rm -f "$HOME/$1/$2""#;

/// Writes `text` to a new file at `path` that only its owner can read:
/// `0600` in a `0700` folder on Unix; on Windows the folder's permissions,
/// which under the user's profile (where the app keeps its data) admit the
/// user, the system and the administrators only. Returns the path.
///
/// # Errors
/// When the file cannot be made there.
pub fn write_private_local(path: &Path, text: &str) -> Result<String, String> {
    let at = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| at(&e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| at(&e))?;
        }
    }
    write_fresh(path, text, true).map_err(|e| at(&e))?;
    Ok(path.to_string_lossy().into_owned())
}

/// [`write_private_local`] on another machine, at `~/.cache/review/mcp/name`
/// under `umask 077`, the text on `ssh`'s stdin. Returns the absolute path.
///
/// # Errors
/// `ssh` failures, as text.
pub fn write_private_remote(machine: &Machine, name: &str, text: &str) -> Result<String, String> {
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC is not reached over SSH".to_owned());
    };
    let args = [REMOTE_PRIVATE_DIR, name];
    let path = remote_sh(host, PRIVATE_SCRIPT, &args, Some(text.as_bytes()))?;
    let path = path.trim();
    if !path.starts_with('/') || path.contains(char::is_control) {
        return Err(format!("ssh {host}: no usable path for the MCP config"));
    }
    Ok(path.to_owned())
}

/// Removes a file [`write_private_remote`] wrote; failures are ignored (its
/// token is revoked anyway).
pub fn remove_private_remote(host: &repo::SshHost, name: &str) {
    let _ = remote_sh(host, REMOVE_SCRIPT, &[REMOTE_PRIVATE_DIR, name], None);
}

/// Removes the `.json` files in `dir` written before `before`: what a run
/// that ended without removing them left. Newer ones are this run's.
pub fn sweep_private_local(dir: &Path, before: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let old = meta.modified().is_ok_and(|modified| modified < before);
        if meta.is_file() && old && path.extension().is_some_and(|ext| ext == "json") {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Adds `$2` to the `info/exclude` of the repository at `$1` unless a line
/// already is it. Every value is a positional argument, never script text.
const EXCLUDE_SCRIPT: &str = r#"c=$(git -C "$1" rev-parse --path-format=absolute --git-common-dir) && mkdir -p "$c/info" && f="$c/info/exclude" && { grep -qxF -- "$2" "$f" 2>/dev/null || { if [ -s "$f" ] && [ -n "$(tail -c 1 "$f")" ]; then echo >> "$f"; fi; printf '%s\n' "$2" >> "$f"; }; }"#;

/// Writes stdin to `$1/$4` (making `$1/$3`), then excludes `$2` as
/// [`EXCLUDE_SCRIPT`] does.
fn context_script() -> String {
    // Refuses a `$3` or `$4` the checkout ships as a link, which would send
    // the write somewhere else.
    format!(
        r#"if [ -L "$1/$3" ] || [ -L "$1/$4" ]; then echo "review: $3 in the checkout is a link" >&2; exit 1; fi; mkdir -p "$1/$3" && rm -f "$1/$4" && cat > "$1/$4" && {EXCLUDE_SCRIPT}"#
    )
}

/// Most of a remote script's output kept: a path.
const REMOTE_OUTPUT: usize = 4096;
/// How long a small remote script may take.
const REMOTE_TIMEOUT: Duration = Duration::from_secs(120);

/// The command line `ssh` hands the remote shell: `/bin/sh` runs `script`
/// with `args` as `$1`, `$2`... In the remote home directory, never in a
/// checkout.
pub(crate) fn remote_line(script: &str, args: &[&str]) -> String {
    format!(
        "exec /bin/sh -c {} review {}",
        shell::quote(script),
        shell::join(args.iter().copied())
    )
}

/// Runs `script` with `/bin/sh` on `host` (see [`remote_line`]), `stdin` on
/// its standard input.
fn remote_sh(
    host: &repo::SshHost,
    script: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    remote_sh_limited(host, script, &RemoteRun::new(args, stdin))
}

/// One remote script run's inputs and limits.
#[derive(Clone, Copy)]
pub(crate) struct RemoteRun<'a> {
    pub args: &'a [&'a str],
    pub stdin: Option<&'a [u8]>,
    pub timeout: Duration,
    /// Bytes of standard output kept.
    pub output: usize,
}

impl<'a> RemoteRun<'a> {
    fn new(args: &'a [&'a str], stdin: Option<&'a [u8]>) -> Self {
        Self {
            args,
            stdin,
            timeout: REMOTE_TIMEOUT,
            output: REMOTE_OUTPUT,
        }
    }
}

/// [`remote_sh`] with its own time and output limits. Blocks until the
/// script ends, or the time is up or the thread's work is canceled
/// ([`repo::cancel`]), when `ssh` is killed.
pub(crate) fn remote_sh_limited(
    host: &repo::SshHost,
    script: &str,
    run: &RemoteRun<'_>,
) -> Result<String, String> {
    let RemoteRun {
        args,
        stdin,
        timeout,
        output: kept,
    } = *run;
    let failed = |e: &dyn std::fmt::Display| format!("ssh {host}: {e}");
    let ssh = term::program::resolve("ssh").map_err(|e| failed(&e))?;
    let mut command = Command::new(ssh);
    command
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=20"])
        .arg(host.as_str())
        .arg("--")
        .arg(remote_line(script, args))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    no_console_window(&mut command);
    let mut child = command.spawn().map_err(|e| failed(&e))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        pipe.write_all(bytes).map_err(|e| failed(&e))?;
    }
    let id = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    let deadline = std::time::Instant::now() + timeout;
    let ended = loop {
        match rx.recv_timeout(CANCEL_POLL) {
            Err(mpsc::RecvTimeoutError::Timeout)
                if !repo::cancel::canceled() && std::time::Instant::now() < deadline => {}
            ended => break ended,
        }
    };
    match ended {
        Ok(Ok(output)) if output.status.success() => {
            let end = output.stdout.len().min(kept);
            Ok(String::from_utf8_lossy(&output.stdout[..end]).into_owned())
        }
        Ok(Ok(output)) => Err(failed(&String::from_utf8_lossy(&output.stderr).trim())),
        Ok(Err(error)) => Err(failed(&error)),
        Err(_) => {
            kill(id);
            if repo::cancel::canceled() {
                Err(failed(&"canceled"))
            } else {
                Err(failed(&"took too long"))
            }
        }
    }
}

/// How often a remote script's wait looks at the cancel flag.
const CANCEL_POLL: Duration = Duration::from_millis(50);

fn no_console_window(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Without it every call flashes a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn kill(pid: u32) {
    let mut command = if cfg!(windows) {
        let mut command = Command::new("taskkill");
        command.args(["/F", "/T", "/PID", &pid.to_string()]);
        command
    } else {
        let mut command = Command::new("kill");
        command.args(["-9", &pid.to_string()]);
        command
    };
    no_console_window(&mut command);
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Where pull request `number`'s worktree goes, absolute on that machine:
/// `<clone>.worktrees/pr-N` beside a bare clone, `.claude/worktrees/pr-N`
/// inside a clone with a working tree. `list` is the repository's own
/// worktree list, whose first entry is the repository.
fn remote_worktree_path(
    repo: &RemoteRepo,
    list: &[Worktree],
    number: u64,
) -> Result<String, String> {
    let own = list
        .first()
        .ok_or_else(|| format!("no worktree list from {}", repo.host()))?;
    let root = own.path.trim_end_matches('/');
    Ok(if own.bare {
        format!("{root}.worktrees/{}", leaf(number))
    } else {
        format!("{root}/.claude/worktrees/{}", leaf(number))
    })
}

/// The pull request worktrees of the machine's clone of `repo`.
///
/// # Errors
/// Git and `ssh` failures.
pub fn list_remote(
    machine: &Machine,
    repo: &RepoRef,
    placement: Placement,
) -> Result<Vec<Worktree>, String> {
    let clone = remote_clone(machine, repo, placement)?;
    Ok(clone
        .repo
        .worktrees()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(is_pr_worktree)
        .collect())
}

/// Removes pull request `number`'s worktree from the machine's clone of
/// `repo`, and the head ref the fetch made.
///
/// # Errors
/// Git and `ssh` failures.
pub fn remove_remote(
    machine: &Machine,
    repo: &RepoRef,
    placement: Placement,
    number: u64,
) -> Result<(), String> {
    let err = |e: repo::Error| e.to_string();
    let clone = remote_clone(machine, repo, placement)?;
    let list = clone.repo.worktrees().map_err(err)?;
    let path = remote_worktree_path(&clone.repo, &list, number)?;
    clone
        .repo
        .remove_worktree(&path, RemoveMode::Discard)
        .map_err(err)?;
    clone.repo.delete_ref(&pr_head_ref(number)).map_err(err)
}

/// A worktree "Review with agent" made and the repository it was added
/// to, on its machine: what the finished review's "Clean up" removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewWorktree {
    pub machine: Machine,
    /// The repository's directory there, as its workspace names it.
    pub repo: String,
    /// The worktree's absolute path there.
    pub path: String,
}

/// After a canceled "Review with agent": removes pull request `number`'s
/// worktree from the clone at `dir` on `machine`, made or half made, when
/// git has it registered, as [`remove_review_worktree`] does. Its path when
/// it was removed; `None` when there was none.
///
/// # Errors
/// Why it stays registered, with its path; git and `ssh` failures.
pub fn remove_canceled(
    machine: &Machine,
    dir: &str,
    number: u64,
) -> Result<Option<String>, String> {
    let (list, path) = match &machine.place {
        Place::Local => {
            // No repository there: nothing was added to it.
            let Ok(store) = repo::Store::open(&expand_home(dir)) else {
                return Ok(None);
            };
            let path = store.worktree_path(number).to_string_lossy().into_owned();
            (
                repo::list_worktrees(&store).map_err(|e| e.to_string())?,
                path,
            )
        }
        Place::Ssh(host) => {
            let repo = RemoteRepo::new(host.clone(), dir);
            if !repo.exists().map_err(|e| e.to_string())? {
                return Ok(None);
            }
            let list = repo.worktrees().map_err(|e| e.to_string())?;
            let path = remote_worktree_path(&repo, &list, number)?;
            (list, path)
        }
    };
    if !list.iter().any(|entry| entry.is_at(&path)) {
        return Ok(None);
    }
    let worktree = ReviewWorktree {
        machine: machine.clone(),
        repo: dir.to_owned(),
        path: path.clone(),
    };
    match remove_review_worktree(&worktree) {
        Ok(()) => Ok(Some(path)),
        Err(error) => Err(format!("{path}: {error}")),
    }
}

/// Removes `worktree` from its repository unless it has changes: git
/// refuses a worktree with modified or untracked files, which then stays.
///
/// # Errors
/// Git's refusal, and git and `ssh` failures.
pub fn remove_review_worktree(worktree: &ReviewWorktree) -> Result<(), String> {
    match &worktree.machine.place {
        Place::Local => {
            let dir = expand_home(&worktree.repo);
            let store = repo::Store::open(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            repo::remove_worktree(&store, Path::new(&worktree.path), RemoveMode::Clean)
                .map_err(|e| e.to_string())
        }
        Place::Ssh(host) => RemoteRepo::new(host.clone(), &worktree.repo)
            .remove_worktree(&worktree.path, RemoveMode::Clean)
            .map_err(|e| e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use repo::SshHost;

    #[test]
    fn worktree_paths_follow_the_clone_kind() {
        let repo = RemoteRepo::new(SshHost::new("box").unwrap(), "~/.cache/review/h/o/r");
        let bare = [Worktree {
            path: "/home/u/.cache/review/h/o/r".into(),
            bare: true,
            ..Worktree::default()
        }];
        assert_eq!(
            remote_worktree_path(&repo, &bare, 4).unwrap(),
            "/home/u/.cache/review/h/o/r.worktrees/pr-4"
        );
        let clone = [Worktree {
            path: "/home/u/dev/r".into(),
            branch: Some("refs/heads/main".into()),
            ..Worktree::default()
        }];
        assert_eq!(
            remote_worktree_path(&repo, &clone, 4).unwrap(),
            "/home/u/dev/r/.claude/worktrees/pr-4"
        );
        assert!(remote_worktree_path(&repo, &[], 4).is_err());
    }

    #[test]
    fn the_context_file_never_follows_what_the_checkout_ships() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/worktrees-tests")
            .join(format!("context-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        write_review_files_local(&dir, "one", false).unwrap();
        write_review_files_local(&dir, "two", false).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(CONTEXT_FILE)).unwrap(),
            "two"
        );
        let private = dir.join("private").join("ab.json");
        assert_eq!(
            write_private_local(&private, "{}").unwrap(),
            private.to_string_lossy()
        );
        assert_eq!(write_private_local(&private, "{1}").map(drop), Ok(()));
        assert_eq!(std::fs::read_to_string(&private).unwrap(), "{1}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode(&private) & 0o077, 0, "only its owner reads the token");
            assert_eq!(mode(private.parent().unwrap()) & 0o077, 0);
        }
        std::fs::write(dir.join("private").join("keep.txt"), "x").unwrap();
        sweep_private_local(&dir.join("private"), std::time::UNIX_EPOCH);
        assert!(private.exists(), "newer than the cut");
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        sweep_private_local(&dir.join("private"), later);
        assert!(!private.exists());
        assert!(dir.join("private").join("keep.txt").exists());

        let file_dir = dir.join("as-file");
        std::fs::create_dir_all(&file_dir).unwrap();
        std::fs::write(file_dir.join(CONTEXT_DIR), "not a folder").unwrap();
        assert!(write_review_files_local(&file_dir, "x", false).is_err());
        #[cfg(unix)]
        {
            let outside = dir.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            let linked = dir.join("linked");
            std::fs::create_dir_all(&linked).unwrap();
            std::os::unix::fs::symlink(&outside, linked.join(CONTEXT_DIR)).unwrap();
            assert!(write_review_files_local(&linked, "x", false).is_err());
            assert!(!outside.join("context.md").exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_or_empty_directory_needs_a_clone() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/worktrees-tests")
            .join(format!("needs-clone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(needs_clone_local(&dir), Ok(true), "missing");
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(needs_clone_local(&dir), Ok(true), "empty");
        std::fs::write(dir.join("README"), "").unwrap();
        assert_eq!(needs_clone_local(&dir), Ok(false), "has files");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn excludes_are_added_once_on_their_own_line() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/worktrees-tests")
            .join(format!("exclude-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        exclude_local(&dir, ".review/").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("info/exclude")).unwrap(),
            ".review/\n"
        );
        std::fs::write(dir.join("info/exclude"), "# mine\n*.log").unwrap();
        exclude_local(&dir, ".claude/worktrees/").unwrap();
        exclude_local(&dir, ".claude/worktrees/").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("info/exclude")).unwrap(),
            "# mine\n*.log\n.claude/worktrees/\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The remote scripts, run by a real POSIX shell where there is one,
    /// with values that would break them if they were script text.
    #[test]
    fn remote_scripts_take_values_as_arguments_only() {
        assert!(PRIVATE_SCRIPT.starts_with("umask 077;"));
        for script in [
            EXCLUDE_SCRIPT,
            &context_script(),
            PRIVATE_SCRIPT,
            REMOVE_SCRIPT,
        ] {
            assert!(!script.contains(".review") && !script.contains(".claude"));
        }
        let line = remote_line(&context_script(), &["dir; rm -rf ~", ".review/"]);
        assert!(line.starts_with("exec /bin/sh -c '"), "{line}");
        assert!(line.ends_with(" review 'dir; rm -rf ~' .review/"), "{line}");
        assert_eq!(expand_home("/abs/x"), PathBuf::from("/abs/x"));
        if let Some(home) = dirs::home_dir() {
            assert_eq!(expand_home("~/src/r"), home.join("src/r"));
        }

        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/worktrees-tests")
            .join(format!("script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .ok()
                .filter(|out| out.status.success())
        };
        if git(&["init", "-q"]).is_none() {
            return;
        }
        let worktree = dir.to_string_lossy().replace('\\', "/");
        let run = |stdin: &str| {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(context_script())
                .args([
                    "review",
                    &worktree,
                    ".review/",
                    ".review",
                    ".review/context.md",
                ])
                .stdin(Stdio::piped())
                .spawn()
                .ok()?;
            child.stdin.take()?.write_all(stdin.as_bytes()).ok()?;
            child.wait().ok()
        };
        let Some(status) = run("$(touch pwned) `id`\n") else {
            return;
        };
        assert!(status.success());
        assert!(run("second\n").is_some_and(|s| s.success()));
        assert_eq!(
            std::fs::read_to_string(dir.join(".review/context.md")).unwrap(),
            "second\n"
        );
        let exclude = std::fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == ".review/").count(), 1);
        assert!(!dir.join("pwned").exists());

        // The MCP config: under the home folder, only its owner reads it.
        let private = |stdin: &str| {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(PRIVATE_SCRIPT)
                .args(["review", REMOTE_PRIVATE_DIR, "ab.json"])
                .env("HOME", &dir)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .ok()?;
            child.stdin.take()?.write_all(stdin.as_bytes()).ok()?;
            let out = child.wait_with_output().ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
        };
        let path = private("{token}").expect("written");
        assert!(path.ends_with("/.cache/review/mcp/ab.json"), "{path}");
        assert!(private("{again}").is_some(), "replaced");
        let file = dir.join(".cache/review/mcp/ab.json");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "{again}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_pull_request_worktrees_are_listed() {
        let entry = |path: &str, bare| Worktree {
            path: path.into(),
            bare,
            ..Worktree::default()
        };
        assert!(is_pr_worktree(&entry("/x/r.worktrees/pr-4", false)));
        assert!(is_pr_worktree(&entry(
            r"C:\x\.claude\worktrees\pr-12",
            false
        )));
        assert!(!is_pr_worktree(&entry("/x/r", true)));
        assert!(!is_pr_worktree(&entry("/x/feature", false)));
    }

    /// "Clean up" on a real worktree with a space and a quote in its path:
    /// kept, with git's refusal, while it has an untracked file; removed
    /// once it has none, the clone left as it was.
    #[test]
    fn a_review_worktree_is_removed_only_when_clean() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/worktrees-tests")
            .join(format!("remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let clone = dir.join("my clone");
        std::fs::create_dir_all(&clone).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&clone)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .ok()
                .filter(|out| out.status.success())
        };
        if git(&["init", "-q"]).is_none() {
            return;
        }
        std::fs::write(clone.join("a.txt"), "a").unwrap();
        assert!(git(&["add", "a.txt"]).is_some());
        assert!(git(&["commit", "-q", "-m", "a"]).is_some());
        let path = clone.join(".claude/worktrees/pr-'7 x");
        let path_text = path.to_string_lossy().into_owned();
        assert!(git(&["worktree", "add", "-q", "--detach", &path_text]).is_some());
        let worktree = ReviewWorktree {
            machine: Machine::this_pc(None),
            repo: clone.to_string_lossy().into_owned(),
            path: path_text,
        };

        std::fs::write(path.join("notes.txt"), "mine").unwrap();
        let error = remove_review_worktree(&worktree).unwrap_err();
        assert!(
            error.contains("untracked") || error.contains("modified"),
            "{error}"
        );
        assert!(path.join("notes.txt").is_file(), "kept with its changes");

        std::fs::remove_file(path.join("notes.txt")).unwrap();
        remove_review_worktree(&worktree).unwrap();
        assert!(!path.exists());
        assert!(clone.join("a.txt").is_file());

        // After a canceled launch: pr-7's worktree, while git lists it.
        let this_pc = Machine::this_pc(None);
        let clone_text = clone.to_string_lossy().into_owned();
        assert_eq!(remove_canceled(&this_pc, &clone_text, 7), Ok(None));
        let missing = dir.join("no clone").to_string_lossy().into_owned();
        assert_eq!(remove_canceled(&this_pc, &missing, 7), Ok(None));
        let pr7 = clone.join(".claude").join("worktrees").join("pr-7");
        let pr7_text = pr7.to_string_lossy().into_owned();
        assert!(git(&["worktree", "add", "-q", "--detach", &pr7_text]).is_some());
        std::fs::write(pr7.join("half.txt"), "x").unwrap();
        let error = remove_canceled(&this_pc, &clone_text, 7).unwrap_err();
        assert!(error.starts_with(&pr7_text), "{error}");
        assert!(pr7.is_dir(), "kept with its changes");
        std::fs::remove_file(pr7.join("half.txt")).unwrap();
        let removed = remove_canceled(&this_pc, &clone_text, 7).unwrap();
        assert_eq!(removed, Some(pr7_text));
        assert!(!pr7.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
