//! Starting a coding agent (Claude Code by default) in a pull request's
//! worktree. Anyone can open a pull request, so the checkout's agent config
//! is someone else's code: hooks in `.claude/settings.json` run shell
//! commands, `.mcp.json` starts servers, `CLAUDE.md` steers the agent.
//!
//! The policy, shared by "Open on machine" and anything else that hands a
//! pull request to an agent:
//! - the worktree is in a repository the app owns
//!   ([`crate::worktrees::Placement::AppOwned`]), never inside a clone the
//!   user trusts;
//! - Claude starts with the user's own settings only ([`SAFE_CLAUDE_FLAGS`]):
//!   no project or local settings, so no hooks, plugins, skills, agents or
//!   `CLAUDE.md` from the checkout, and no project MCP servers;
//! - any other program has no known way to keep the checkout's config out,
//!   so it never starts by itself: a shell opens there, and the program runs
//!   only after an explicit confirmation naming it and the worktree;
//! - when the pull request changes agent config ([`is_agent_config`]) against
//!   its merge base, the user sees the files first, and only an explicit
//!   confirmation naming them starts the agent with them loaded.
//!
//! "Review with agent" follows the same policy with one difference: the
//! worktree goes where the user's workspace for the repository says
//! ([`prepare_review_job`]), which may be the user's own clone. What keeps
//! the checkout's config out is then the launch alone: Claude starts with
//! [`SAFE_CLAUDE_FLAGS`] in the worktree, or in the user's own working
//! directory with the worktree as an `--add-dir` folder (whose `CLAUDE.md`
//! is not read, and which is used only when the pull request changes no
//! agent config, since Claude loads such a folder's skills), never with a
//! worktree inside that working directory. A checkout with symbolic links
//! that lead out of it is refused, since Claude reads the worktree freely.

use std::path::Path;

use repo::{ChangeKind, FileListOptions};

use crate::claude_review::McpLaunch;
use crate::config::WorkspaceEntry;
use crate::machines::{self, Machine};
use crate::model::{ChangedFile, FileStatus, PrKey};
use crate::session::Session;
use crate::terminals::{CommandLine, Outcome, Prepared, TerminalSpec};
use crate::worktrees::{self, Placement, PrSource, WorktreeInfo};

/// Claude Code flags that keep a checkout's config out: settings from the
/// user's own file only (project and local settings, and with them hooks,
/// `CLAUDE.md`, skills, agents and commands, are not read), and MCP servers
/// from `--mcp-config` only, of which none is given.
pub const SAFE_CLAUDE_FLAGS: [&str; 3] = ["--setting-sources", "user", "--strict-mcp-config"];

/// The program stem that takes [`SAFE_CLAUDE_FLAGS`].
const CLAUDE: &str = "claude";

/// Folders whose whole content an agent reads as config, at any depth.
const CONFIG_DIRS: [&str; 5] = [".claude", ".cursor", ".gemini", ".codex", ".continue"];
/// Files an agent reads as config or instructions, at any depth.
const CONFIG_FILES: [&str; 5] = [
    ".mcp.json",
    "claude.md",
    "claude.local.md",
    "agents.md",
    "gemini.md",
];
/// Name prefixes of files and folders agents (or the shell around them, as
/// with `.envrc`) read: `.aider.conf.yml`, `.windsurfrules`, `.env.local`.
const CONFIG_PREFIXES: [&str; 3] = [".aider", ".windsurf", ".env"];

/// Whether a path in a repository is something an agent reads as config or
/// instructions: anything under `.claude/`, `.cursor/`, `.gemini/`, `.codex/`
/// or `.continue/`; `.mcp.json`, `CLAUDE.md`, `CLAUDE.local.md`, `AGENTS.md`
/// and `GEMINI.md`; names starting `.aider`, `.windsurf` or `.env`, all at
/// any depth; `.github/copilot-instructions.md` and `.vscode/settings.json`.
/// Case is ignored, as Windows and macOS file systems ignore it. A list to
/// warn with: only Claude has a launch that keeps them out.
#[must_use]
pub fn is_agent_config(path: &str) -> bool {
    let path = path.replace('\\', "/").to_ascii_lowercase();
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let Some((file, dirs)) = parts.split_last() else {
        return false;
    };
    let named = |part: &str| {
        CONFIG_DIRS.contains(&part)
            || CONFIG_PREFIXES
                .iter()
                .any(|prefix| part.starts_with(prefix))
    };
    if parts.iter().any(|part| named(part)) || CONFIG_FILES.contains(file) {
        return true;
    }
    matches!(
        (dirs.last().copied(), *file),
        (Some(".github"), "copilot-instructions.md") | (Some(".vscode"), "settings.json")
    )
}

/// An agent config file the pull request changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigChange {
    pub path: String,
    /// The old path of a rename.
    pub old_path: Option<String>,
    pub kind: ChangeKind,
}

impl ConfigChange {
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            ChangeKind::Added => "added",
            ChangeKind::Deleted => "deleted",
            ChangeKind::Modified => "changed",
            ChangeKind::Renamed => "renamed",
        }
    }
}

/// What comparing the pull request's head with its merge base found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigCheck {
    /// No agent config changed: the checkout's config is the base branch's.
    Clean,
    Changed(Vec<ConfigChange>),
    /// The comparison could not be made; treated like a change.
    Unchecked(String),
}

impl ConfigCheck {
    /// Whether the user chooses how to start before anything runs.
    #[must_use]
    pub fn needs_choice(&self) -> bool {
        !matches!(self, Self::Clean)
    }

    #[must_use]
    pub fn changes(&self) -> &[ConfigChange] {
        match self {
            Self::Changed(changes) => changes,
            Self::Clean | Self::Unchecked(_) => &[],
        }
    }

    fn of(changes: Vec<ConfigChange>) -> Self {
        if changes.is_empty() {
            Self::Clean
        } else {
            Self::Changed(changes)
        }
    }
}

/// The agent config pull request `number` changes between its merge base
/// and `head` (the commit its worktree has checked out), from a store the
/// pull request was fetched into. Blocks on git.
#[must_use]
pub fn check_store(store: &repo::Store, number: u64, head: &str) -> ConfigCheck {
    match changed_in_store(store, number, head) {
        Ok(changes) => ConfigCheck::of(changes),
        Err(error) => ConfigCheck::Unchecked(error),
    }
}

fn changed_in_store(
    store: &repo::Store,
    number: u64,
    head: &str,
) -> Result<Vec<ConfigChange>, String> {
    let short = head.get(..7).unwrap_or(head);
    let head = store
        .resolve_commit(head)
        .map_err(|_| format!("the worktree's commit {short} is not in this PC's copy"))?;
    let base = store
        .resolve_commit(&repo::pr_base_ref(number))
        .map_err(|e| format!("the base branch: {e}"))?;
    let merge_base = store.merge_base(base, head).map_err(|e| e.to_string())?;
    let list = store
        .file_list(Some(merge_base), head, FileListOptions::default())
        .map_err(|e| e.to_string())?;
    Ok(list
        .files
        .into_iter()
        .filter(|file| {
            is_agent_config(&file.path) || file.old_path.as_deref().is_some_and(is_agent_config)
        })
        .map(|file| ConfigChange {
            path: file.path,
            old_path: file.old_path,
            kind: file.kind,
        })
        .collect())
}

/// The symbolic links in `head`'s tree that lead outside the checkout. A
/// tool that checks paths by their text (Claude Code's permission rules)
/// sees `docs/key` as inside the worktree even when it links to
/// `~/.ssh/id_ed25519`, so a run that must stay in the worktree refuses a
/// checkout with any. Blocks on git.
///
/// # Errors
/// When the commit or a link cannot be read.
pub fn escaping_links(store: &repo::Store, head: &str) -> Result<Vec<String>, String> {
    let short = head.get(..7).unwrap_or(head);
    let head = store
        .resolve_commit(head)
        .map_err(|_| format!("the worktree's commit {short} is not in this PC's copy"))?;
    let links = store.symlinks(head).map_err(|e| e.to_string())?;
    let paths: Vec<&str> = links.iter().map(|(path, _)| path.as_str()).collect();
    Ok(links
        .iter()
        .filter(|(path, target)| link_escapes(path, target, &paths))
        .map(|(path, target)| format!("{path} -> {target}"))
        .collect())
}

/// Whether the link at `path` naming `target` can lead outside the
/// checkout: an absolute or home-relative target, one that climbs above
/// the root, or one that passes through another link (`others`), whose own
/// target the text does not show.
///
/// File systems read names differently (a backslash, trailing dots and
/// spaces, `~` short names, case and Unicode folding), so rather than model
/// each one, a link whose path or target is not [`plain`] counts as
/// leading outside, and the rest compare ASCII-case-insensitively.
fn link_escapes(path: &str, target: &str, others: &[&str]) -> bool {
    if !plain(path) || !plain(target) || target.starts_with('/') {
        return true;
    }
    let path = path.to_ascii_lowercase();
    let others: Vec<String> = others.iter().map(|o| o.to_ascii_lowercase()).collect();
    let mut parts: Vec<String> = path.split('/').map(str::to_owned).collect();
    parts.pop();
    for part in target.to_ascii_lowercase().split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return true;
                }
            }
            part => {
                parts.push(part.to_owned());
                let through = parts.join("/");
                if through != path && others.contains(&through) {
                    return true;
                }
            }
        }
    }
    false
}

/// Whether every component of a link path or target is one every file
/// system reads the same way: ASCII letters, digits and `._-+@`, not
/// ending in a dot unless it is `.` or `..`.
fn plain(text: &str) -> bool {
    text.split('/').all(|part| {
        let chars = part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+' | '@'));
        chars && (matches!(part, "." | "..") || !part.ends_with('.'))
    })
}

/// The same check over a file list the app already has (a fixtures run).
#[must_use]
pub fn check_files(files: &[ChangedFile]) -> ConfigCheck {
    ConfigCheck::of(
        files
            .iter()
            .filter(|file| is_agent_config(&file.path))
            .map(|file| ConfigChange {
                path: file.path.to_string(),
                old_path: None,
                kind: match file.status {
                    FileStatus::Added => ChangeKind::Added,
                    FileStatus::Deleted => ChangeKind::Deleted,
                    FileStatus::Modified => ChangeKind::Modified,
                    FileStatus::Renamed => ChangeKind::Renamed,
                },
            })
            .collect(),
    )
}

/// How the agent starts in a pull request's worktree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    /// Without the checkout's agent config: the default. Only Claude has
    /// such a launch; for any other program a shell opens instead.
    Safe,
    /// As the command says, loading the checkout's config. Only after the
    /// user confirmed the files, or the program and worktree, by name.
    TrustPr,
}

/// Whether `command` is one whose checkout config [`LaunchMode::Safe`] can
/// keep out.
#[must_use]
pub fn has_safe_mode(command: &str) -> bool {
    CommandLine::parse(command).is_ok_and(|line| line.stem() == CLAUDE)
}

/// Whether the user chooses how `command` starts before anything runs: when
/// the pull request changes agent config, or the comparison failed, and
/// always for a program with no safe launch.
#[must_use]
pub fn needs_choice(command: &str, check: &ConfigCheck) -> bool {
    check.needs_choice() || !has_safe_mode(command)
}

/// What the terminal runs for `command` under `mode`: Claude with
/// [`SAFE_CLAUDE_FLAGS`] after its program, the command as it is when
/// trusted, and nothing (a shell) for any other program in safe mode.
///
/// # Errors
/// When the command names no program.
pub fn agent_command(command: &str, mode: LaunchMode) -> Result<Option<String>, String> {
    let line = CommandLine::parse(command)?;
    Ok(match mode {
        LaunchMode::TrustPr => Some(command.trim().to_owned()),
        LaunchMode::Safe if line.stem() == CLAUDE => {
            let program = if line.program.contains(char::is_whitespace) {
                format!("\"{}\"", line.program)
            } else {
                line.program.to_owned()
            };
            let flags = SAFE_CLAUDE_FLAGS.join(" ");
            Some(
                format!("{program} {flags} {}", line.rest)
                    .trim_end()
                    .to_owned(),
            )
        }
        LaunchMode::Safe => None,
    })
}

/// Blocking work that makes a pull request's worktree and checks what agent
/// config the pull request changes, reporting its steps.
pub type PrepareJob = Box<dyn FnOnce(worktrees::Progress<'_>) -> Result<Outcome, String> + Send>;

/// The work that makes `key`'s worktree on `machine` in a repository the app
/// owns ([`Placement::AppOwned`]) and compares its agent config with the
/// merge base. A fixtures run makes an empty folder under `fixture_root`
/// instead, and checks the fixture's file list. Reads the session; the job
/// it returns blocks on git and the network.
///
/// # Errors
/// For an invented machine, a pull request that is not open, or a live run
/// not connected to the forges yet.
pub fn prepare_job(
    session: &Session,
    key: &PrKey,
    machine: &Machine,
    fixture_root: &Path,
) -> Result<PrepareJob, String> {
    if machine.fixture {
        return Err(format!(
            "{} is an invented machine; nothing connects to it",
            machine.name
        ));
    }
    let open = session
        .pr(key)
        .ok_or_else(|| "the pull request is not open".to_owned())?;
    if session.is_fixtures() {
        let dir = fixture_root
            .join(format!(
                "{}-{}",
                key.repo.owner.replace('/', "_"),
                key.repo.name
            ))
            .join(worktrees::leaf(key.number.0));
        let head = open.pr.sha.to_string();
        let check = check_files(&open.files);
        return Ok(Box::new(move |progress: worktrees::Progress<'_>| {
            progress("adding the worktree");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let info = WorktreeInfo {
                machine: machines::THIS_PC.to_owned(),
                local: true,
                path: dir.to_string_lossy().into_owned(),
                head,
                note: None,
            };
            Ok(Prepared {
                info,
                check,
                links: Ok(Vec::new()),
                mcp_file: None,
            }
            .into())
        }));
    }
    let services = session
        .services()
        .cloned()
        .ok_or_else(|| "not connected to the forges yet".to_owned())?;
    let request = services.fetch_request(&key.repo, key.number, open.pr.base.as_ref());
    let source = PrSource::new(key.repo.clone(), request);
    let machine = machine.clone();
    Ok(Box::new(move |progress: worktrees::Progress<'_>| {
        if machine.is_local() {
            let objects = services.objects.app_owned(&source.repo)?;
            let info = worktrees::ensure_local(&objects, &source, progress)?;
            progress("comparing agent config");
            let check = check_store(&objects, source.number, &info.head);
            let links = escaping_links(&objects, &info.head);
            return Ok(Prepared {
                info,
                check,
                links,
                mcp_file: None,
            }
            .into());
        }
        let info = worktrees::ensure_remote(&machine, &source, Placement::AppOwned, progress)?;
        // The remote checkout is compared here, in this PC's copy, which has
        // the base branch too.
        progress("comparing agent config");
        let (check, links) =
            match services.objects.resolve(&source.repo).and_then(|objects| {
                worktrees::fetch_local(&objects, &source.fetch).map(|_| objects)
            }) {
                Ok(objects) => (
                    check_store(&objects, source.number, &info.head),
                    escaping_links(&objects, &info.head),
                ),
                Err(error) => (ConfigCheck::Unchecked(error.clone()), Err(error)),
            };
        Ok(Prepared {
            info,
            check,
            links,
            mcp_file: None,
        }
        .into())
    }))
}

/// The work behind "Review with agent": makes `key`'s worktree off the
/// workspace's repository directory on `machine` (or, when that has no
/// clone yet, ends with [`Outcome::Clone`]: a terminal the user sees runs
/// `git clone` there), compares its agent config with the merge base,
/// lists links that leave it, writes `context` to its `.review/context.md`
/// (kept out of git) and `mcp`'s config, which holds the launch's token, to
/// a file only the user can read outside the worktree. A fixtures run makes
/// an empty folder under `fixture_root` instead. Reads the session; the job
/// blocks on git, the disk and the network.
///
/// # Errors
/// For an invented machine, a pull request that is not open, or a live run
/// not connected to the forges yet.
pub fn prepare_review_job(
    session: &Session,
    key: &PrKey,
    machine: &Machine,
    workspace: &WorkspaceEntry,
    context: String,
    mcp: Option<McpLaunch>,
    fixture_root: &Path,
) -> Result<PrepareJob, String> {
    if machine.fixture {
        return Err(format!(
            "{} is an invented machine; nothing connects to it",
            machine.name
        ));
    }
    let open = session
        .pr(key)
        .ok_or_else(|| "the pull request is not open".to_owned())?;
    let workdir = workspace.workdir().map(str::to_owned);
    if session.is_fixtures() {
        let dir = fixture_root
            .join(format!(
                "{}-{}",
                key.repo.owner.replace('/', "_"),
                key.repo.name
            ))
            .join(worktrees::leaf(key.number.0));
        let head = open.pr.sha.to_string();
        let check = check_files(&open.files);
        let machine = machine.clone();
        return Ok(Box::new(move |progress: worktrees::Progress<'_>| {
            progress("adding the worktree");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = std::path::absolute(&dir).unwrap_or(dir);
            refuse_nested(&path.to_string_lossy(), workdir.as_deref(), true)?;
            worktrees::write_review_files_local(&path, &context, false)?;
            let mcp_file = mcp
                .as_ref()
                .map(|mcp| crate::claude_review::write_config(&machine, mcp))
                .transpose()?;
            let info = WorktreeInfo {
                machine: machines::THIS_PC.to_owned(),
                local: true,
                path: path.to_string_lossy().into_owned(),
                head,
                note: None,
            };
            Ok(Prepared {
                info,
                check,
                links: Ok(Vec::new()),
                mcp_file,
            }
            .into())
        }));
    }
    let services = session
        .services()
        .cloned()
        .ok_or_else(|| "not connected to the forges yet".to_owned())?;
    let request = services.fetch_request(&key.repo, key.number, open.pr.base.as_ref());
    let source = PrSource::new(key.repo.clone(), request);
    let machine = machine.clone();
    let dir = workspace.path.trim().to_owned();
    let local = machine.is_local();
    // Before anything is placed: a worktree in the working directory would
    // put the pull request's files in the user's own project.
    for place in [dir.clone(), format!("{dir}.worktrees")] {
        refuse_nested(&place, workdir.as_deref(), local)?;
    }
    Ok(Box::new(move |progress: worktrees::Progress<'_>| {
        let needs_clone = if local {
            worktrees::needs_clone_local(&worktrees::expand_home(&dir))?
        } else {
            progress("connecting");
            worktrees::needs_clone_remote(&machine, &dir)?
        };
        if needs_clone {
            let spec = Box::new(TerminalSpec::clone_into(
                &machine,
                &source.fetch.remote_url,
                &dir,
            )?);
            return Ok(Outcome::Clone { spec, dir });
        }
        let (info, check, links) = if local {
            let dir = worktrees::expand_home(&dir);
            let (clone, info) = worktrees::ensure_in_dir_local(&dir, &source, progress)?;
            refuse_nested(&info.path, workdir.as_deref(), true)?;
            progress("comparing agent config");
            let check = check_store(&clone, source.number, &info.head);
            let links = escaping_links(&clone, &info.head);
            (info, check, links)
        } else {
            let info = worktrees::ensure_in_dir_remote(&machine, &source, &dir, progress)?;
            refuse_nested(&info.path, workdir.as_deref(), false)?;
            progress("comparing agent config");
            // The remote checkout is compared here, in this PC's copy.
            let (check, links) = match services.objects.resolve(&source.repo).and_then(|objects| {
                worktrees::fetch_local(&objects, &source.fetch).map(|_| objects)
            }) {
                Ok(objects) => (
                    check_store(&objects, source.number, &info.head),
                    escaping_links(&objects, &info.head),
                ),
                Err(error) => (ConfigCheck::Unchecked(error.clone()), Err(error)),
            };
            (info, check, links)
        };
        // A checkout with links out of it is refused before anything is
        // written for it.
        if !links.as_ref().is_ok_and(Vec::is_empty) {
            return Ok(Prepared {
                info,
                check,
                links,
                mcp_file: None,
            }
            .into());
        }
        progress("writing the context file");
        if local {
            worktrees::write_review_files_local(Path::new(&info.path), &context, true)?;
        } else {
            worktrees::write_review_files_remote(&machine, &info.path, &context)?;
        }
        let mcp_file = mcp
            .as_ref()
            .map(|mcp| crate::claude_review::write_config(&machine, mcp))
            .transpose()?;
        Ok(Prepared {
            info,
            check,
            links,
            mcp_file,
        }
        .into())
    }))
}

/// Refuses `place` (a worktree, or where one goes) inside the agent's
/// working directory, where the pull request's nested `CLAUDE.md` and
/// `.claude/` would be under its cwd. On another machine a path under `~`
/// is compared only with another under `~`, as text.
fn refuse_nested(place: &str, workdir: Option<&str>, local: bool) -> Result<(), String> {
    let Some(workdir) = workdir else {
        return Ok(());
    };
    let (place, workdir) = if local {
        let expand = |path: &str| worktrees::expand_home(path).to_string_lossy().into_owned();
        (expand(place), expand(workdir))
    } else if place.starts_with('~') != workdir.starts_with('~') {
        return Ok(());
    } else {
        (place.to_owned(), workdir.to_owned())
    };
    let fold_case = local && cfg!(windows);
    if crate::claude_review::command::within(&place, &workdir, fold_case) {
        return Err(format!(
            "{place} is inside the working directory {workdir}, where the pull request's own              agent config would load; move the repository's directory out of it"
        ));
    }
    Ok(())
}

/// The terminal for `machine`'s command in a pull request's `worktree`,
/// started as `mode` says.
///
/// # Errors
/// When the machine's command names no program.
pub fn launch_spec(
    machine: &Machine,
    worktree: &WorktreeInfo,
    mode: LaunchMode,
) -> Result<TerminalSpec, String> {
    command_spec(machine, machine.command(), worktree, mode)
}

/// The terminal for `command` in a pull request's `worktree` on `machine`,
/// started as `mode` says.
///
/// # Errors
/// When the command names no program.
pub fn command_spec(
    machine: &Machine,
    command: &str,
    worktree: &WorktreeInfo,
    mode: LaunchMode,
) -> Result<TerminalSpec, String> {
    Ok(TerminalSpec {
        cwd: Some(worktree.path.clone()),
        command: agent_command(command, mode)?,
        ..TerminalSpec::shell_on(machine)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_that_leave_the_checkout_are_found() {
        let none: &[&str] = &[];
        for (path, target) in [
            ("docs/key", "/home/u/.ssh/id_ed25519"),
            ("key", "~/.ssh/id_ed25519"),
            ("key", "C:/Users/u/.ssh/id"),
            ("key", "C:\\Users\\u\\.ssh\\id"),
            ("docs/key", "../../.ssh/id_ed25519"),
            ("a", "b/../../x"),
        ] {
            assert!(link_escapes(path, target, none), "{path} -> {target}");
        }
        for (path, target) in [
            ("latest", "docs/README.md"),
            ("docs/up", "../README.md"),
            ("a/b/c", "../../d/./e"),
        ] {
            assert!(!link_escapes(path, target, none), "{path} -> {target}");
        }
        // `out` may itself lead anywhere, so a link through it might too,
        // whatever the case: Windows and macOS find `OUT` as `out`.
        assert!(link_escapes("doc", "out/../x", &["doc", "out"]));
        assert!(link_escapes("doc", "OUT/x", &["doc", "out"]));
        assert!(!link_escapes("doc", "src/x", &["doc", "out"]));
        // A backslash is a separator on Windows and a name on Linux.
        assert!(link_escapes("a", r"x\..\..\y", none));
        assert!(link_escapes(r"a\b", "c", none));
        assert!(link_escapes("a", "c:x", none));
        // Names file systems fold or trim differently.
        for target in [
            "..../x",
            "x./y",
            "x /y",
            "PROGRA~1/x",
            "caf\u{e9}/x",
            "\u{3c2}/x",
        ] {
            assert!(link_escapes("a", target, none), "{target}");
        }
        assert!(!link_escapes("a", "v1.2_x-y+z@w/./f.txt", none));
    }

    #[test]
    fn agent_config_paths_match_at_any_depth() {
        for path in [
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".claude/hooks/x.sh",
            "sub/.claude/skills/a/SKILL.md",
            ".Claude/Settings.json",
            ".mcp.json",
            "pkg/.mcp.json",
            "CLAUDE.md",
            "docs/CLAUDE.md",
            "CLAUDE.local.md",
            "AGENTS.md",
            "a/AGENTS.md",
            ".cursor/rules/x.mdc",
            ".github/copilot-instructions.md",
            ".vscode/settings.json",
            ".claude",
            r"sub\.claude\settings.json",
            ".gemini/settings.json",
            "GEMINI.md",
            "x/.codex/config.toml",
            ".aider.conf.yml",
            ".aiderignore",
            ".continue/config.json",
            ".windsurfrules",
            ".windsurf/rules/a.md",
            ".env",
            ".envrc",
            "app/.env.local",
        ] {
            assert!(is_agent_config(path), "{path}");
        }
        for path in [
            "src/claude.rs",
            "claude/settings.json",
            "docs/CLAUDE.md.txt",
            ".github/workflows/ci.yml",
            ".vscode/launch.json",
            "mcp.json",
            "src/env.rs",
            "docs/gemini.txt",
            "",
        ] {
            assert!(!is_agent_config(path), "{path}");
        }
    }

    #[test]
    fn safe_mode_adds_the_flags_after_the_program() {
        let safe = |command| agent_command(command, LaunchMode::Safe).unwrap();
        assert_eq!(
            safe("claude").as_deref(),
            Some("claude --setting-sources user --strict-mcp-config")
        );
        assert_eq!(
            safe("claude --resume").as_deref(),
            Some("claude --setting-sources user --strict-mcp-config --resume")
        );
        assert_eq!(
            safe("\"C:/Program Files/c/claude.exe\" -c").as_deref(),
            Some("\"C:/Program Files/c/claude.exe\" --setting-sources user --strict-mcp-config -c")
        );
        assert_eq!(safe("codex"), None, "no safe launch: a shell only");
        assert_eq!(
            agent_command("claude --resume", LaunchMode::TrustPr)
                .unwrap()
                .as_deref(),
            Some("claude --resume")
        );
        assert!(agent_command("  ", LaunchMode::Safe).is_err());
        assert!(has_safe_mode("/usr/local/bin/claude"));
        assert!(!has_safe_mode("aider"));
    }

    #[test]
    fn a_worktree_inside_the_working_directory_is_refused() {
        assert!(refuse_nested("/w/me/r/.claude/worktrees/pr-4", Some("/w/me"), false).is_err());
        assert!(refuse_nested("/w/r/.claude/worktrees/pr-4", Some("/w/me"), false).is_ok());
        assert!(refuse_nested("/w/r", None, false).is_ok());
        assert!(refuse_nested("~/me/r", Some("~/me"), false).is_err());
        // Across `~` and an absolute path the remote home is unknown.
        assert!(refuse_nested("/home/u/me/r", Some("~/me"), false).is_ok());
        assert!(refuse_nested("C:/src/me/r", Some("C:/src/me"), true).is_err());
    }

    #[test]
    fn checks_need_a_choice_unless_clean() {
        let files = [ChangedFile {
            path: ".claude/settings.json".into(),
            blob: "b".into(),
            status: FileStatus::Modified,
            added: 1,
            removed: 0,
        }];
        let check = check_files(&files);
        assert!(check.needs_choice());
        assert_eq!(check.changes()[0].kind_label(), "changed");
        assert_eq!(check_files(&[]), ConfigCheck::Clean);
        assert!(!ConfigCheck::Clean.needs_choice());
        assert!(ConfigCheck::Unchecked("x".into()).needs_choice());
        assert!(!needs_choice("claude --resume", &ConfigCheck::Clean));
        assert!(needs_choice("claude", &check));
        for other in ["codex", "gemini", "aider", "/home/u/bin/agent.sh"] {
            assert!(needs_choice(other, &ConfigCheck::Clean), "{other}");
        }
    }
}
