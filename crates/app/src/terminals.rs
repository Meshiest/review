//! Terminals and the machines they run on: the machine picker that "New
//! terminal" (`Ctrl+\``) and "Open on machine" share, and the pull request
//! worktrees "Open on machine" makes before it starts its command there.
//!
//! The shell places the terminal panels; this module says what to start and
//! where (a [`TerminalsEvent::Open`]). A launch that makes a worktree opens
//! its tab first ([`TerminalsEvent::Show`]), which shows the steps until the
//! program starts there.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::prelude::FluentBuilder as _;
use gpui::{
    AnyElement, App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, FontWeight,
    Global, InteractiveElement as _, IntoElement, MouseButton, ParentElement, Render,
    StatefulInteractiveElement as _, Styled, Task, WeakEntity, Window, actions, div, px,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, h_flex, v_flex};
use serde::{Deserialize, Serialize};
use term::SessionConfig;

use crate::agent::{self, ConfigCheck, LaunchMode};
use crate::checklist::{Checklist, Purpose as StepsFor};
use crate::claude_review::{self, ReviewLaunch};
use crate::machines::{self, Machine, Place};
use crate::model::{ForgeKind, PrKey};
use crate::panels::PanelKind;
use crate::session::{OpenMode, Session};
use crate::terminal_panel::{Retry, TerminalPanel};
use crate::ui::{self, Button, ButtonKind, TextExt as _};
use crate::worktrees::{self, Step, WorktreeInfo};

actions!(
    terminals,
    [
        /// `Ctrl+\``: pick a machine and open a terminal there.
        NewTerminal,
        PickerUp,
        PickerDown,
        PickerConfirm,
        PickerCancel
    ]
);

/// Blocking work that makes a worktree and checks its agent config,
/// reporting its steps.
pub type WorktreeJob = Box<dyn FnOnce(worktrees::Progress<'_>) -> Result<Outcome, String> + Send>;

/// How a worktree job ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The worktree is ready for its command.
    Prepared(Box<Prepared>),
    /// The workspace's directory has no clone yet: a terminal runs the clone
    /// where the user sees it and can answer git, and "Review with agent"
    /// goes on from there when asked again.
    Clone {
        spec: Box<TerminalSpec>,
        dir: String,
    },
}

impl From<Prepared> for Outcome {
    fn from(prepared: Prepared) -> Self {
        Self::Prepared(Box::new(prepared))
    }
}

/// A pull request's worktree, ready for its command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub info: WorktreeInfo,
    /// The agent config the pull request changes.
    pub check: ConfigCheck,
    /// The checkout's symbolic links that point outside it, or why they
    /// could not be read.
    pub links: Result<Vec<String>, String>,
    /// "Review with agent": where Claude's MCP config was written, on the
    /// agent's machine.
    pub mcp_file: Option<String>,
}

/// Where a launch goes and what for.
#[derive(Clone, Copy)]
struct Launch<'a> {
    key: &'a PrKey,
    machine: &'a Machine,
    review: Option<&'a ReviewLaunch>,
}

/// What a launch's tab shows before its program starts.
pub struct LaunchTab {
    pub title: String,
    /// The machine's name, as the picker shows it.
    pub machine: String,
    pub checklist: Checklist,
    /// Runs the launch again after a failed step.
    pub retry: Retry,
    /// Stops the launch while a step runs; `None` offers no Cancel.
    pub cancel: Option<Retry>,
}

/// The step that starts the launch's program.
fn start_step(review: Option<&ReviewLaunch>) -> Step {
    match review {
        Some(_) => Step::StartAgent,
        None => Step::StartCommand,
    }
}

/// A worktree whose pull request changes agent config, waiting for the user
/// to say how its command starts.
#[derive(Clone, Debug)]
pub struct PendingLaunch {
    pub key: PrKey,
    pub machine: Machine,
    pub at: Where,
    pub prepared: Prepared,
    /// The second step: the user asked to trust the files and now confirms.
    pub confirming: bool,
    /// "Review with agent": the workspace's agent, prompt and MCP server
    /// instead of the machine's command.
    pub review: Option<ReviewLaunch>,
}

impl PendingLaunch {
    /// The command that starts.
    #[must_use]
    pub fn command(&self) -> &str {
        match &self.review {
            Some(review) => review.workspace.command(),
            None => self.machine.command(),
        }
    }
}

/// The machine picker's key context, which holds focus while it is open.
pub const PICKER_CONTEXT: &str = "MachinePicker";

/// What a terminal runs, where. Saved with the layout, so a restart can
/// offer to open it again.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalSpec {
    /// The machine's name as the picker shows it.
    pub machine: String,
    /// The SSH host; `None` on this PC.
    #[serde(default)]
    pub host: Option<String>,
    /// The directory it starts in, on that machine.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Run in the shell first; the shell stays when it ends.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
    /// An agent started directly instead of a shell. Not saved with the
    /// layout: its review token dies with the terminal.
    #[serde(skip)]
    pub agent: Option<AgentCommand>,
}

/// An agent a terminal starts as itself, its words never parsed by a shell
/// on this PC; over SSH they are quoted for the remote POSIX shell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentCommand {
    /// The program, then its arguments.
    pub argv: Vec<String>,
    /// Variables cleared in its environment.
    pub cleared: Vec<String>,
    /// Over SSH, `-R 127.0.0.1:<remote>:127.0.0.1:<local>`: the review's MCP
    /// server reached from the machine.
    pub forward: Option<Forward>,
    /// The pull request it reviews.
    pub pr: PrKey,
    /// That pull request as the tab names it (see [`TerminalSpec::title`]).
    pub label: String,
    /// Its review token, revoked when the terminal ends.
    pub token: crate::claude_review::mcp::Token,
    /// The worktree it reviews in, which "Clean up" removes.
    pub worktree: worktrees::ReviewWorktree,
}

/// A remote port forwarded to a port of this PC's loopback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Forward {
    pub remote: u16,
    pub local: u16,
}

/// The forge CLI's login for `host`; the app reads its token from that CLI.
#[must_use]
pub fn sign_in_command(kind: ForgeKind, host: &str) -> String {
    match kind {
        ForgeKind::GitHub => format!("gh auth login --hostname {host}"),
        ForgeKind::GitLab => format!("glab auth login --hostname {host}"),
        ForgeKind::Forgejo => format!("fj -H {host} auth login"),
    }
}

/// The command a terminal on `machine` runs to clone `url` into `dir`:
/// `git clone <url> <dir>`, each word quoted for the shell that reads the
/// line (the machine's shell here, a POSIX shell over SSH). A local `dir`
/// is made absolute with `~` expanded; a remote one is relative to the
/// home directory `ssh` starts in.
///
/// # Errors
/// For an unusable URL, or a word the local shell cannot quote.
pub fn clone_command(machine: &Machine, url: &str, dir: &str) -> Result<String, String> {
    let url = url.trim();
    if !url.starts_with(|ch: char| ch.is_ascii_alphanumeric()) {
        return Err(format!("unusable remote URL `{url}`"));
    }
    let dir = dir.trim();
    if dir.is_empty() {
        return Err("no directory to clone into".to_owned());
    }
    let (kind, dir) = match &machine.place {
        Place::Local => {
            let path = worktrees::expand_home(dir);
            let path = std::path::absolute(&path).unwrap_or(path);
            (local_shell_kind(machine), path_text(&path))
        }
        Place::Ssh(_) => {
            let dir = repo::shell::home_relative(dir);
            // Never read as an option.
            let dir = if dir.starts_with('-') {
                format!("./{dir}")
            } else {
                dir.to_owned()
            };
            (ShellKind::Posix, dir)
        }
    };
    Ok(format!(
        "git clone {} {}",
        quote_for(kind, url)?,
        quote_for(kind, &dir)?
    ))
}

/// The kind of shell a terminal on this PC starts: the machine's own, else
/// the terminal's default (`pwsh` on Windows, the login shell elsewhere).
fn local_shell_kind(machine: &Machine) -> ShellKind {
    match machine.shell() {
        Some(shell) => ShellKind::of(shell),
        None if cfg!(windows) => ShellKind::PowerShell,
        None => ShellKind::Posix,
    }
}

/// `word` as one literal argument for `kind`.
///
/// # Errors
/// For control characters, and for `"` or `%` under `cmd`, which no
/// quoting there keeps literal.
fn quote_for(kind: ShellKind, word: &str) -> Result<String, String> {
    if word.contains(char::is_control) {
        return Err(format!("`{word}` has control characters"));
    }
    Ok(match kind {
        ShellKind::Posix => repo::shell::quote(word).into_owned(),
        // PowerShell reads the typographic single quotes as `'` too; each
        // is doubled inside single quotes, where nothing else expands.
        ShellKind::PowerShell => {
            let mut quoted = String::with_capacity(word.len() + 2);
            quoted.push('\'');
            for ch in word.chars() {
                if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
                    quoted.push(ch);
                }
                quoted.push(ch);
            }
            quoted.push('\'');
            quoted
        }
        ShellKind::Cmd => {
            if word.contains(['"', '%']) {
                return Err(format!("`{word}` cannot be quoted for cmd"));
            }
            format!("\"{word}\"")
        }
    })
}

/// How a local shell is told to run a command and stay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShellKind {
    PowerShell,
    Cmd,
    Posix,
}

impl ShellKind {
    fn of(program: &str) -> Self {
        let stem = std::path::Path::new(program)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        match stem.as_str() {
            "pwsh" | "powershell" => Self::PowerShell,
            "cmd" => Self::Cmd,
            _ => Self::Posix,
        }
    }
}

impl TerminalSpec {
    /// A shell on `machine`, in its home directory.
    #[must_use]
    pub fn shell_on(machine: &Machine) -> Self {
        Self {
            machine: machine.name.clone(),
            host: match &machine.place {
                Place::Local => None,
                Place::Ssh(host) => Some(host.as_str().to_owned()),
            },
            cwd: None,
            command: None,
            shell: machine.shell().map(str::to_owned),
            agent: None,
        }
    }

    /// A shell on `machine` that runs the forge CLI's login for `host`, the
    /// way back from an auth error. `None` when `host` is not a plain host
    /// name (with an optional port), since it goes into a command line.
    #[must_use]
    pub fn sign_in(kind: ForgeKind, host: &str, machine: &Machine) -> Option<Self> {
        let plain = host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':'))
            && host.starts_with(|ch: char| ch.is_ascii_alphanumeric());
        if !plain {
            return None;
        }
        Some(Self {
            command: Some(sign_in_command(kind, host)),
            ..Self::shell_on(machine)
        })
    }

    /// A shell on `machine` that runs [`clone_command`] first and stays
    /// open, so git can ask for credentials and the user can go on typing.
    ///
    /// # Errors
    /// As [`clone_command`].
    pub fn clone_into(machine: &Machine, url: &str, dir: &str) -> Result<Self, String> {
        Ok(Self {
            command: Some(clone_command(machine, url, dir)?),
            ..Self::shell_on(machine)
        })
    }

    /// What the tab says: the machine and the end of the directory, or for
    /// an agent the pull request it reviews and the machine.
    #[must_use]
    pub fn title(&self) -> String {
        if let Some(agent) = &self.agent {
            return format!("Review {} \u{b7} {}", agent.label, self.machine);
        }
        match self.cwd.as_deref().map(short_dir) {
            Some(dir) => format!("{} \u{b7} {dir}", self.machine),
            None => self.machine.clone(),
        }
    }

    /// The program, arguments and directory to start. On this PC the shell
    /// itself; over SSH the system `ssh` with a terminal, which runs the
    /// login shell there (in `cwd`, running `command` first).
    ///
    /// Every program started is an absolute path from the PATH, never one
    /// in `cwd` (see [`term::program`]); over SSH the remote shell finds the
    /// command the same way. Blocks on the disk: call it off the UI thread.
    ///
    /// # Errors
    /// When the host name is not one `ssh` should be given, or a program is
    /// not on the PATH.
    pub fn session_config(&self) -> Result<SessionConfig, String> {
        self.session_config_with(&|program: &str| {
            term::program::resolve(program).map_err(|e| e.to_string())
        })
    }

    fn session_config_with(&self, resolve: &Resolver<'_>) -> Result<SessionConfig, String> {
        if let Some(agent) = &self.agent {
            return self.agent_config(agent, resolve);
        }
        let mut config = SessionConfig::default();
        let command = self
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
            .map(CommandLine::parse)
            .transpose()?;
        match &self.host {
            None => {
                if let Some(shell) = &self.shell {
                    config.program.clone_from(shell);
                }
                config.program = path_text(&resolve(&config.program)?);
                config.cwd = self.cwd.as_ref().map(PathBuf::from).or_else(dirs::home_dir);
                let command = command
                    .map(|line| Ok::<_, String>((path_text(&resolve(line.program)?), line.rest)))
                    .transpose()?;
                let kind = ShellKind::of(&config.program);
                config.args = local_args(kind, &config.program, command);
                config.raw_args = kind == ShellKind::Cmd;
            }
            Some(host) => {
                let host = repo::SshHost::new(host).map_err(|e| e.to_string())?;
                config.program = path_text(&resolve("ssh")?);
                config.args = vec!["-t".to_owned(), host.as_str().to_owned()];
                if let Some(line) = self.remote_line(command) {
                    config.args.extend(["--".to_owned(), line]);
                }
                config.cwd = dirs::home_dir();
            }
        }
        Ok(config)
    }

    /// An agent's program and arguments: on this PC started as they are,
    /// with the cleared variables set empty (a terminal only adds to the
    /// environment, and Claude Code reads an empty value as off); over SSH
    /// the same words quoted for the remote login shell, which clears the
    /// variables itself, with the review's port forwarded.
    fn agent_config(
        &self,
        agent: &AgentCommand,
        resolve: &Resolver<'_>,
    ) -> Result<SessionConfig, String> {
        let (program, args) = agent
            .argv
            .split_first()
            .ok_or_else(|| "the agent has no program".to_owned())?;
        let mut config = SessionConfig::default();
        match &self.host {
            None => {
                config.program = path_text(&resolve(program)?);
                config.args = args.to_vec();
                config.cwd = self.cwd.as_ref().map(PathBuf::from).or_else(dirs::home_dir);
                config.env = agent
                    .cleared
                    .iter()
                    .map(|name| (name.clone(), String::new()))
                    .collect();
            }
            Some(host) => {
                let host = repo::SshHost::new(host).map_err(|e| e.to_string())?;
                config.program = path_text(&resolve("ssh")?);
                config.args = vec!["-t".to_owned()];
                if let Some(forward) = agent.forward {
                    config.args.extend([
                        "-o".to_owned(),
                        "ExitOnForwardFailure=yes".to_owned(),
                        "-R".to_owned(),
                        format!("127.0.0.1:{}:127.0.0.1:{}", forward.remote, forward.local),
                    ]);
                }
                let rest = repo::shell::join(args.iter().map(String::as_str));
                let line = CommandLine {
                    program,
                    rest: &rest,
                };
                let unset = agent
                    .cleared
                    .iter()
                    .map(|name| format!("unset {}; ", repo::shell::quote(name)))
                    .collect::<String>();
                let remote = self
                    .remote_line_with(Some(line), &unset)
                    .ok_or_else(|| "nothing to run".to_owned())?;
                config
                    .args
                    .extend([host.as_str().to_owned(), "--".to_owned(), remote]);
                config.cwd = dirs::home_dir();
            }
        }
        Ok(config)
    }

    /// The command line the remote shell runs, quoted for a POSIX shell;
    /// `None` for a plain login shell.
    fn remote_line(&self, command: Option<CommandLine<'_>>) -> Option<String> {
        self.remote_line_with(command, "")
    }

    /// [`Self::remote_line`], with `before` (fixed text) run in the login
    /// shell just before the command.
    fn remote_line_with(&self, command: Option<CommandLine<'_>>, before: &str) -> Option<String> {
        if self.cwd.is_none() && command.is_none() && self.shell.is_none() {
            return None;
        }
        // Unquoted, so the remote shell expands it.
        let shell = self.shell.as_deref().map_or_else(
            || "\"${SHELL:-/bin/sh}\"".to_owned(),
            |shell| repo::shell::quote(shell).into_owned(),
        );
        // The shell starts through the guard too: after the `cd` below, a bare
        // name looked up on a PATH with a relative entry could run a program
        // the pull request's checkout planted.
        let guarded = |program: &str, rest: &str| {
            format!(
                "/bin/sh -c {} review {program} {rest}",
                repo::shell::quote(REMOTE_GUARD)
            )
            .trim_end()
            .to_owned()
        };
        let login = guarded(&shell, "-l");
        let start = match command {
            // An interactive login shell sets the PATH the command needs,
            // `sh` finds it on that PATH alone, and a login shell takes over
            // when it ends.
            Some(line) => {
                let run = guarded(&repo::shell::quote(line.program), line.rest);
                let inner =
                    repo::shell::quote(&format!("{before}{run}; exec {login}")).into_owned();
                format!("exec {}", guarded(&shell, &format!("-lic {inner}")))
            }
            None => format!("exec {login}"),
        };
        Some(match &self.cwd {
            Some(dir) => format!("cd {} && {start}", repo::shell::quote(dir)),
            None => start,
        })
    }
}

/// Finds a program's absolute path.
type Resolver<'a> = dyn Fn(&str) -> Result<PathBuf, String> + 'a;

/// What runs `"$1"` on a remote machine, with the rest of its arguments:
/// `sh` refuses a PATH with an empty or relative entry (one that names the
/// current directory, a pull request's checkout) and starts only an absolute
/// path `command -v` found on it, never an alias or function a shell
/// startup file defined.
pub const REMOTE_GUARD: &str = "case \":$PATH:\" in *:[!/]*) \
     echo \"review: the PATH has an empty or relative entry, so $1 was not started\" >&2; \
     exit 126;; esac; \
     p=$(command -v \"$1\") || p=; \
     case $p in /*) shift; exec \"$p\" \"$@\";; esac; \
     echo \"review: $1 is not a program on the PATH\" >&2; exit 127";

/// A command as the config writes it: its program, then the rest, which the
/// shell reads as it is (the user's own text).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandLine<'a> {
    pub program: &'a str,
    pub rest: &'a str,
}

impl<'a> CommandLine<'a> {
    /// The first word, or a double-quoted path, is the program.
    ///
    /// # Errors
    /// For an empty program, an unclosed quote, or one that starts with `-`.
    pub fn parse(command: &'a str) -> Result<Self, String> {
        let command = command.trim();
        let (program, rest) = match command.strip_prefix('"') {
            Some(quoted) => quoted
                .split_once('"')
                .ok_or_else(|| format!("unclosed quote in the command `{command}`"))?,
            None => command
                .split_once(char::is_whitespace)
                .unwrap_or((command, "")),
        };
        if program.is_empty() || program.starts_with('-') {
            return Err(format!("no program to run in the command `{command}`"));
        }
        Ok(Self {
            program,
            rest: rest.trim(),
        })
    }

    /// The program's file name without an extension, lowercase: what kind
    /// of program it is.
    #[must_use]
    pub fn stem(&self) -> String {
        std::path::Path::new(self.program)
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    }
}

fn path_text(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// The shell's arguments that run `command` (an absolute program and the
/// rest of its line) and then stay.
fn local_args(kind: ShellKind, shell: &str, command: Option<(String, &str)>) -> Vec<String> {
    let mut args: Vec<String> = match kind {
        ShellKind::PowerShell => vec!["-NoLogo".to_owned()],
        ShellKind::Cmd | ShellKind::Posix => Vec::new(),
    };
    let Some((program, rest)) = command else {
        return args;
    };
    match kind {
        ShellKind::PowerShell => {
            let line = format!("& '{}' {rest}", program.replace('\'', "''"));
            args.extend(["-NoExit".to_owned(), "-Command".to_owned()]);
            args.push(line.trim_end().to_owned());
        }
        // `/S` strips exactly the outer quotes, leaving the quoted program.
        ShellKind::Cmd => {
            let line = format!("\"{program}\" {rest}");
            args.extend(["/S".to_owned(), "/K".to_owned()]);
            args.push(format!("\"{}\"", line.trim_end()));
        }
        ShellKind::Posix => {
            let line = format!("{} {rest}", repo::shell::quote(&program));
            args.extend(["-l", "-i", "-c"].map(str::to_owned));
            args.push(format!(
                "{}; exec {} -l",
                line.trim_end(),
                repo::shell::quote(shell)
            ));
        }
    }
    args
}

/// The last two parts of a path, `~` kept.
fn short_dir(path: &str) -> String {
    let parts: Vec<&str> = path.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => path.to_owned(),
        [one] => (*one).to_owned(),
        [.., a, b] => format!("{a}/{b}"),
    }
}

/// Where a new terminal goes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Where {
    /// A tab in the main area's current group.
    Tab,
    /// A new group to the right of it.
    #[default]
    Split,
    /// Beside the CI job log, in the right-hand pane.
    RightPane,
}

impl Where {
    pub const ALL: [Self; 3] = [Self::Tab, Self::Split, Self::RightPane];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Tab => "Tab",
            Self::Split => "Split",
            Self::RightPane => "Right pane",
        }
    }

    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Tab => "picker-at-tab",
            Self::Split => "picker-at-split",
            Self::RightPane => "picker-at-right",
        }
    }
}

/// What a pick does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Purpose {
    NewTerminal,
    /// Make sure the pull request's worktree is there, then run the
    /// machine's command in it.
    OpenOn(PrKey),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalsEvent {
    /// Open a terminal panel running `spec`, placed `at`.
    Open { spec: Box<TerminalSpec>, at: Where },
    /// Show a launch's tab: placed `at` when it is not in the dock yet,
    /// brought forward when it is.
    Show {
        panel: Entity<TerminalPanel>,
        at: Where,
    },
}

/// A pull request's worktree on the machine it was last opened on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeState {
    Working {
        machine: String,
        step: Step,
    },
    Ready(WorktreeInfo),
    /// Ready, but its command waits for the user: the pull request changes
    /// agent config, or the command has no safe launch.
    Choosing(WorktreeInfo),
    /// A terminal clones into the workspace's directory; the review starts
    /// when asked again after it is done.
    Cloning {
        machine: String,
        dir: String,
    },
    Failed {
        machine: String,
        error: String,
    },
}

#[derive(Clone)]
struct Picker {
    purpose: Purpose,
    at: Where,
    selected: usize,
    /// Where focus goes back when it closes.
    previous: Option<FocusHandle>,
}

/// The machines, the picker and the pull requests' worktrees: one per app,
/// drawn by the shell as an overlay layer.
pub struct Terminals {
    fixtures: bool,
    /// Invented hosts a test lists after the fixtures' own.
    fixture_hosts: Vec<String>,
    /// `~/.ssh/config`'s hosts, once read off the main thread.
    ssh_hosts: Vec<String>,
    machines: Vec<Machine>,
    picker: Option<Picker>,
    focus: FocusHandle,
    worktrees: HashMap<PrKey, WorktreeState>,
    jobs: HashMap<PrKey, Task<()>>,
    /// The cancel flag of each pull request's last job: the git and `ssh`
    /// it runs are killed once it is set.
    cancels: HashMap<PrKey, Arc<AtomicBool>>,
    /// Each pull request's launch tab, which shows the steps until its
    /// program starts.
    tabs: HashMap<PrKey, WeakEntity<TerminalPanel>>,
    pending: Option<PendingLaunch>,
    /// Where a fixtures run makes its worktree folders.
    fixture_root: PathBuf,
    /// Jobs a test hands in, run in place of the next launches' own.
    queued_jobs: VecDeque<WorktreeJob>,
    _read_hosts: Option<Task<()>>,
}

struct TerminalsHandle(Entity<Terminals>);

impl Global for TerminalsHandle {}

impl EventEmitter<TerminalsEvent> for Terminals {}

impl Terminals {
    /// Installs the global. A live run reads the SSH config's host names in
    /// the background; a fixtures run lists invented machines instead.
    pub fn install(fixtures: bool, cx: &mut App) -> Entity<Self> {
        let terminals = cx.new(|cx: &mut Context<Self>| {
            let read_hosts = (!fixtures).then(|| {
                cx.spawn(async move |this, cx| {
                    let hosts = cx
                        .background_executor()
                        .spawn(async {
                            machines::user_ssh_config()
                                .map(|path| machines::ssh_config_hosts(&path))
                                .unwrap_or_default()
                        })
                        .await;
                    let _ = this.update(cx, |this, cx| {
                        this.ssh_hosts = hosts;
                        cx.notify();
                    });
                })
            });
            Self {
                fixtures,
                fixture_hosts: Vec::new(),
                ssh_hosts: Vec::new(),
                machines: Vec::new(),
                picker: None,
                focus: cx.focus_handle(),
                worktrees: HashMap::new(),
                jobs: HashMap::new(),
                cancels: HashMap::new(),
                tabs: HashMap::new(),
                pending: None,
                fixture_root: std::env::current_dir()
                    .unwrap_or_default()
                    .join(".scratch")
                    .join("fixture-worktrees"),
                queued_jobs: VecDeque::new(),
                _read_hosts: read_hosts,
            }
        });
        cx.set_global(TerminalsHandle(terminals.clone()));
        terminals
    }

    #[must_use]
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<TerminalsHandle>()
            .map(|handle| handle.0.clone())
    }

    /// The machines a picker offers now.
    #[must_use]
    pub fn machines(&self, cx: &App) -> Vec<Machine> {
        if self.fixtures {
            let mut list = machines::fixtures();
            list.extend(
                self.fixture_hosts
                    .iter()
                    .map(|host| machines::fixture(host, None)),
            );
            return list;
        }
        let session = Session::global(cx);
        let entries = session
            .read(cx)
            .services()
            .map(|services| services.config.machines.clone())
            .unwrap_or_default();
        machines::list(&entries, &self.ssh_hosts)
    }

    /// Lists `hosts` too in a fixtures run, as invented SSH machines, for
    /// tests that need a long list.
    pub fn add_fixture_hosts(&mut self, hosts: impl IntoIterator<Item = String>) {
        self.fixture_hosts.extend(hosts);
    }

    /// Where a fixtures run makes worktree folders: `.scratch/fixture-worktrees`
    /// in the working directory unless set.
    pub fn set_fixture_root(&mut self, root: PathBuf) {
        self.fixture_root = root;
    }

    /// Runs `job` in place of the next launch's own worktree job, for tests
    /// that step through a launch or make it fail.
    pub fn queue_job(&mut self, job: WorktreeJob) {
        self.queued_jobs.push_back(job);
    }

    #[must_use]
    pub fn is_picking(&self) -> bool {
        self.picker.is_some()
    }

    /// The machines the open picker lists, for tests.
    #[must_use]
    pub fn listed(&self) -> &[Machine] {
        &self.machines
    }

    #[must_use]
    pub fn worktree(&self, key: &PrKey) -> Option<&WorktreeState> {
        self.worktrees.get(key)
    }

    /// The launch waiting for the user's choice, if any.
    #[must_use]
    pub fn pending(&self) -> Option<&PendingLaunch> {
        self.pending.as_ref()
    }

    /// `key`'s launch tab while it shows the steps, before its program
    /// starts.
    #[must_use]
    pub fn launch_tab(&self, key: &PrKey, cx: &App) -> Option<Entity<TerminalPanel>> {
        let panel = self.tabs.get(key)?.upgrade()?;
        panel.read(cx).checklist().is_some().then_some(panel)
    }

    /// Opens `key`'s launch tab at once, before any work: the tab already
    /// showing its steps starts over, else a new tab opens `at`.
    pub fn open_launch_tab(
        &mut self,
        key: &PrKey,
        tab: LaunchTab,
        at: Where,
        cx: &mut Context<Self>,
    ) -> Entity<TerminalPanel> {
        let LaunchTab {
            title,
            machine,
            checklist,
            retry,
            cancel,
        } = tab;
        let panel = match self.launch_tab(key, cx) {
            Some(panel) => {
                panel.update(cx, |panel, cx| panel.relaunch(checklist, cx));
                panel
            }
            None => {
                let stub = TerminalSpec {
                    machine,
                    ..TerminalSpec::default()
                };
                let panel =
                    TerminalPanel::launching(title, stub, checklist, Some(retry), cancel, cx);
                self.tabs.insert(key.clone(), panel.downgrade());
                panel
            }
        };
        cx.emit(TerminalsEvent::Show {
            panel: panel.clone(),
            at,
        });
        panel
    }

    /// Changes the steps `key`'s launch tab shows, while it shows them.
    fn steps(&self, key: &PrKey, cx: &mut App, f: impl FnOnce(&mut Checklist)) {
        if let Some(panel) = self.tabs.get(key).and_then(WeakEntity::upgrade) {
            panel.update(cx, |panel, cx| panel.update_checklist(f, cx));
        }
    }

    /// Opens the picker for `purpose`, with focus on it.
    pub fn open_picker(&mut self, purpose: Purpose, window: &mut Window, cx: &mut Context<Self>) {
        self.machines = self.machines(cx);
        let previous = window.focused(cx);
        self.picker = Some(Picker {
            purpose,
            at: Where::default(),
            selected: 0,
            previous,
        });
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub fn set_where(&mut self, at: Where, cx: &mut Context<Self>) {
        if let Some(picker) = &mut self.picker {
            picker.at = at;
            cx.notify();
        }
    }

    fn close_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<Picker> {
        let picker = self.picker.take()?;
        if let Some(previous) = &picker.previous {
            window.focus(previous, cx);
        }
        cx.notify();
        Some(picker)
    }

    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let count = self.machines.len();
        if let Some(picker) = &mut self.picker
            && count > 0
        {
            picker.selected = (picker.selected as isize + by).rem_euclid(count as isize) as usize;
            cx.notify();
        }
    }

    /// Picks machine `ix` of the open picker.
    pub fn pick(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(machine) = self.machines.get(ix).cloned() else {
            return;
        };
        let Some(picker) = self.close_picker(window, cx) else {
            return;
        };
        match picker.purpose {
            Purpose::NewTerminal => {
                if machine.fixture {
                    return;
                }
                cx.emit(TerminalsEvent::Open {
                    spec: Box::new(TerminalSpec::shell_on(&machine)),
                    at: picker.at,
                });
            }
            Purpose::OpenOn(key) => self.open_on(key, machine, picker.at, cx),
        }
    }

    /// Opens a tab with the steps, makes the pull request's worktree on
    /// `machine` (fetching its head), then runs the machine's command in
    /// that tab.
    pub fn open_on(&mut self, key: PrKey, machine: Machine, at: Where, cx: &mut Context<Self>) {
        let title = format!(
            "{} \u{b7} {}",
            claude_review::pr_label(&key, cx),
            machine.name
        );
        let retry: Retry = {
            let (key, machine) = (key.clone(), machine.clone());
            Rc::new(move |_, cx| {
                if let Some(terminals) = Terminals::global(cx) {
                    terminals.update(cx, |terminals, cx| {
                        terminals.open_on(key.clone(), machine.clone(), at, cx);
                    });
                }
            })
        };
        let tab = LaunchTab {
            title,
            machine: machine.name.clone(),
            checklist: Checklist::plan(StepsFor::Command, !machine.is_local()),
            retry,
            cancel: None,
        };
        self.open_launch_tab(&key, tab, at, cx);
        let job = self.worktree_job(&key, &machine, cx);
        self.run_job(key, machine, at, job, None, cx);
    }

    /// "Review with agent": makes the pull request's worktree where the
    /// workspace says, writes `context` in it, then starts the agent in the
    /// launch tab (asking first when the pull request changes agent config,
    /// and always for an agent other than Claude). The tab is open already
    /// ([`Self::open_launch_tab`]).
    pub fn review_on(
        &mut self,
        key: PrKey,
        machine: Machine,
        review: ReviewLaunch,
        context: String,
        cx: &mut Context<Self>,
    ) {
        let job = agent::prepare_review_job(
            Session::global(cx).read(cx),
            &key,
            &machine,
            &review.workspace,
            context,
            review.mcp.clone(),
            &self.fixture_root,
        );
        self.run_job(key, machine, Where::Split, job, Some(review), cx);
    }

    /// `key`'s launch failed before its job ran: its tab shows `error`.
    pub fn fail(&mut self, key: PrKey, machine: &str, error: String, cx: &mut Context<Self>) {
        self.steps(&key, cx, |steps| steps.fail(error.clone()));
        self.worktrees.insert(
            key,
            WorktreeState::Failed {
                machine: machine.to_owned(),
                error,
            },
        );
        cx.notify();
    }

    fn run_job(
        &mut self,
        key: PrKey,
        machine: Machine,
        at: Where,
        job: Result<WorktreeJob, String>,
        review: Option<ReviewLaunch>,
        cx: &mut Context<Self>,
    ) {
        let purpose = if review.is_some() {
            StepsFor::Review
        } else {
            StepsFor::Command
        };
        let plan = Checklist::plan(purpose, !machine.is_local());
        self.steps(&key, cx, |steps| *steps = plan);
        self.worktrees.insert(
            key.clone(),
            WorktreeState::Working {
                machine: machine.name.clone(),
                step: Step::Fetch,
            },
        );
        cx.notify();
        let job = match self.queued_jobs.pop_front() {
            Some(queued) => Ok(queued),
            None => job,
        };
        let job = match job {
            Ok(job) => job,
            Err(error) => {
                claude_review::forget(review.as_ref(), cx);
                self.fail(key, &machine.name, error, cx);
                return;
            }
        };
        let flag = Arc::new(AtomicBool::new(false));
        self.cancels.insert(key.clone(), Arc::clone(&flag));
        let (tx, mut rx) = mpsc::unbounded::<Result<Step, Result<Outcome, String>>>();
        let job_flag = Arc::clone(&flag);
        thread::Builder::new()
            .name(format!("worktree-{}", key.number))
            .spawn(move || {
                let progress_tx = tx.clone();
                let mut progress = move |step: Step| {
                    let _ = progress_tx.unbounded_send(Ok(step));
                };
                let result = repo::cancel::scope(job_flag, || job(&mut progress));
                let _ = tx.unbounded_send(Err(result));
            })
            .ok();
        let task_key = key.clone();
        let task = cx.spawn(async move |this, cx| {
            while let Some(message) = rx.next().await {
                if flag.load(Ordering::Relaxed) {
                    // Canceled: what the job made is removed once it ends.
                    if message.is_err() {
                        break;
                    }
                    continue;
                }
                let done = this.update(cx, |this, cx| {
                    let next = match message {
                        Ok(step) => {
                            this.steps(&task_key, cx, |steps| steps.reach(step));
                            WorktreeState::Working {
                                machine: machine.name.clone(),
                                step,
                            }
                        }
                        Err(Ok(Outcome::Prepared(prepared))) => this.prepared(
                            task_key.clone(),
                            machine.clone(),
                            at,
                            *prepared,
                            review.clone(),
                            cx,
                        ),
                        Err(Ok(Outcome::Clone { spec, dir })) => {
                            let what = format!(
                                "{dir} on {} has no clone yet, so a terminal beside this one \
                                 clones the repository there. Choose Continue once git is done.",
                                machine.name
                            );
                            this.steps(&task_key, cx, |steps| {
                                steps.block(Step::CloneInTerminal, what);
                            });
                            cx.emit(TerminalsEvent::Open { spec, at });
                            WorktreeState::Cloning {
                                machine: machine.name.clone(),
                                dir,
                            }
                        }
                        Err(Err(error)) => {
                            this.steps(&task_key, cx, |steps| steps.fail(error.clone()));
                            WorktreeState::Failed {
                                machine: machine.name.clone(),
                                error,
                            }
                        }
                    };
                    let done = !matches!(next, WorktreeState::Working { .. });
                    if matches!(
                        next,
                        WorktreeState::Failed { .. } | WorktreeState::Cloning { .. }
                    ) {
                        claude_review::forget(review.as_ref(), cx);
                    }
                    this.worktrees.insert(task_key.clone(), next);
                    cx.notify();
                    done
                });
                if !matches!(done, Ok(false)) {
                    return;
                }
            }
            if !flag.load(Ordering::Relaxed) {
                return;
            }
            let removal = this.update(cx, |this, cx| {
                claude_review::forget(review.as_ref(), cx);
                this.remove_canceled(&task_key, &machine, review.as_ref())
            });
            let removed = match removal {
                Ok(Some(removal)) => removal
                    .await
                    .unwrap_or_else(|_| Err("the cleanup did not start".to_owned())),
                Ok(None) => Ok(None),
                Err(_) => return,
            };
            let _ = this.update(cx, |this, cx| {
                this.canceled(task_key, &machine.name, removed, cx);
            });
        });
        self.jobs.insert(key, task);
    }

    /// "Cancel" in `key`'s launch tab while its job runs: the step under
    /// way stops (its git or `ssh` is killed), nothing starts, and once the
    /// job has ended the worktree it made or half made is removed; the tab
    /// then says what happened, and "Review with agent" starts over.
    pub fn cancel_job(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if !matches!(self.worktrees.get(key), Some(WorktreeState::Working { .. })) {
            return;
        }
        let Some(flag) = self.cancels.get(key) else {
            return;
        };
        flag.store(true, Ordering::Relaxed);
        self.steps(key, cx, Checklist::cancel);
        cx.notify();
    }

    /// Removes a canceled review's worktree off the main thread, as "Clean
    /// up" does; `None` when there is nothing to remove (another launch,
    /// or a fixtures run's plain folder).
    fn remove_canceled(
        &self,
        key: &PrKey,
        machine: &Machine,
        review: Option<&ReviewLaunch>,
    ) -> Option<futures::channel::oneshot::Receiver<Result<Option<String>, String>>> {
        let review = review.filter(|_| !self.fixtures)?;
        let (machine, dir, number) = (
            machine.clone(),
            review.workspace.path.trim().to_owned(),
            key.number.0,
        );
        let (tx, rx) = futures::channel::oneshot::channel();
        // A thread that does not start drops the sender, which the receiver
        // reports.
        let _ = thread::Builder::new()
            .name(format!("worktree-cancel-{number}"))
            .spawn(move || {
                let _ = tx.send(worktrees::remove_canceled(&machine, &dir, number));
            });
        Some(rx)
    }

    /// A canceled launch has stopped: its tab says so, with what became of
    /// its worktree.
    fn canceled(
        &mut self,
        key: PrKey,
        machine: &str,
        removed: Result<Option<String>, String>,
        cx: &mut Context<Self>,
    ) {
        let error = match removed {
            Ok(None) => "You canceled the launch; it left no worktree.".to_owned(),
            Ok(Some(path)) => {
                format!("You canceled the launch; its worktree at {path} was removed.")
            }
            Err(why) => format!("You canceled the launch. Its worktree stays registered: {why}"),
        };
        self.steps(&key, cx, |steps| steps.fail(error.clone()));
        self.worktrees.insert(
            key,
            WorktreeState::Failed {
                machine: machine.to_owned(),
                error,
            },
        );
        cx.notify();
    }

    /// The worktree is up: start its command without the checkout's agent
    /// config, or, when the pull request changes some, ask first.
    fn prepared(
        &mut self,
        key: PrKey,
        machine: Machine,
        at: Where,
        prepared: Prepared,
        review: Option<ReviewLaunch>,
        cx: &mut Context<Self>,
    ) -> WorktreeState {
        if review.is_some()
            && let Err(error) = claude_review::refuse_links(&prepared.links)
        {
            self.steps(&key, cx, |steps| steps.fail(error.clone()));
            return WorktreeState::Failed {
                machine: machine.name,
                error,
            };
        }
        let command = review
            .as_ref()
            .map_or(machine.command(), |review| review.workspace.command());
        if agent::needs_choice(command, &prepared.check) {
            if let Some(old) = self.pending.take() {
                claude_review::forget(old.review.as_ref(), cx);
                self.steps(&old.key, cx, |steps| {
                    steps.fail("Another launch asked first, so this one stopped.".to_owned());
                });
                self.worktrees
                    .insert(old.key, WorktreeState::Ready(old.prepared.info));
            }
            let start = start_step(review.as_ref());
            self.steps(&key, cx, |steps| {
                steps.block(start, "Choose how it starts in the dialog.".to_owned());
            });
            let state = WorktreeState::Choosing(prepared.info.clone());
            self.pending = Some(PendingLaunch {
                key,
                machine,
                at,
                prepared,
                confirming: false,
                review,
            });
            return state;
        }
        let launch = Launch {
            key: &key,
            machine: &machine,
            review: review.as_ref(),
        };
        self.launch(launch, prepared, LaunchMode::Safe, cx)
    }

    /// Starts the program in the launch's tab. A tab the user closed
    /// meanwhile ends the launch.
    fn launch(
        &mut self,
        launch: Launch<'_>,
        prepared: Prepared,
        mode: LaunchMode,
        cx: &mut Context<Self>,
    ) -> WorktreeState {
        let Launch {
            key,
            machine,
            review,
        } = launch;
        self.steps(key, cx, |steps| steps.reach(start_step(review)));
        let spec = match review {
            Some(review) => claude_review::launch_spec(key, machine, &prepared, review, mode),
            None => agent::launch_spec(machine, &prepared.info, mode),
        };
        let spec = match spec {
            Ok(spec) => spec,
            Err(error) => {
                self.steps(key, cx, |steps| steps.fail(error.clone()));
                return WorktreeState::Failed {
                    machine: machine.name.clone(),
                    error,
                };
            }
        };
        let Some(panel) = self.launch_tab(key, cx) else {
            claude_review::forget(review, cx);
            return WorktreeState::Ready(prepared.info);
        };
        if let (Some(agent), Some(state)) = (&spec.agent, claude_review::AgentReview::global(cx)) {
            state.update(cx, |state, cx| state.attach(agent, &panel, cx));
        }
        panel.update(cx, |panel, cx| panel.run(spec, cx));
        WorktreeState::Ready(prepared.info)
    }

    /// Answers the waiting launch: start it as `mode` says. Trusting the
    /// pull request's config takes two steps, [`Self::ask_trust`] first.
    pub fn choose(&mut self, mode: LaunchMode, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        if mode == LaunchMode::TrustPr && !pending.confirming {
            self.pending = Some(pending);
            return;
        }
        let launch = Launch {
            key: &pending.key,
            machine: &pending.machine,
            review: pending.review.as_ref(),
        };
        let state = self.launch(launch, pending.prepared, mode, cx);
        if matches!(state, WorktreeState::Failed { .. }) {
            claude_review::forget(pending.review.as_ref(), cx);
        }
        self.worktrees.insert(pending.key, state);
        cx.notify();
    }

    /// "Trust this PR's config": shows the confirmation naming the files.
    pub fn ask_trust(&mut self, confirming: bool, cx: &mut Context<Self>) {
        if let Some(pending) = &mut self.pending {
            pending.confirming = confirming;
            cx.notify();
        }
    }

    /// Drops the waiting launch; the worktree stays.
    pub fn cancel_launch(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        claude_review::forget(pending.review.as_ref(), cx);
        self.steps(&pending.key, cx, |steps| {
            steps.fail("You canceled the launch; the worktree stays.".to_owned());
        });
        self.worktrees
            .insert(pending.key, WorktreeState::Ready(pending.prepared.info));
        cx.notify();
    }

    /// Shows a changed config file in the pull request's diff; the dialog
    /// stays beside it.
    fn view_diff(&self, path: &str, cx: &mut Context<Self>) {
        let Some(pending) = &self.pending else {
            return;
        };
        let key = pending.key.clone();
        let path = gpui::SharedString::from(path.to_owned());
        Session::global(cx).update(cx, |session, cx| {
            session.show(&key, PanelKind::Diff, OpenMode::Permanent, cx);
            session.select_file(path, cx);
        });
    }

    /// The blocking work that makes `key`'s worktree on `machine` and checks
    /// what agent config its pull request changes.
    fn worktree_job(
        &self,
        key: &PrKey,
        machine: &Machine,
        cx: &App,
    ) -> Result<WorktreeJob, String> {
        agent::prepare_job(
            Session::global(cx).read(cx),
            key,
            machine,
            &self.fixture_root,
        )
    }

    fn picker_view(&self, picker: &Picker, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let title = match &picker.purpose {
            Purpose::NewTerminal => "New terminal on".to_owned(),
            Purpose::OpenOn(key) => format!("Open {}#{} on", key.repo.name, key.number),
        };
        let hover = c.hover;
        let wheres = Where::ALL.map(|at| {
            let chosen = picker.at == at;
            div()
                .id(at.id())
                .debug_selector(move || at.id().into())
                .px(px(8.0))
                .h(px(22.0))
                .flex()
                .items_center()
                .rounded(t.radius.sm)
                .border_1()
                .border_color(if chosen { c.accent } else { c.border })
                .when(chosen, |el| el.bg(c.selected))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .text_color(if chosen { c.text } else { c.text_secondary })
                .on_click(cx.listener(move |this, _, _, cx| this.set_where(at, cx)))
                .child(at.label())
        });
        let rows = self.machines.iter().enumerate().map(|(ix, machine)| {
            let selected = ix == picker.selected;
            let icon = if machine.is_local() {
                IconName::Monitor
            } else {
                IconName::Server
            };
            let detail = if machine.fixture {
                format!("{}; invented, never connected", machine.detail())
            } else {
                machine.detail()
            };
            h_flex()
                .id(("machine-row", ix))
                .debug_selector(move || format!("machine-row-{ix}"))
                .h(px(40.0))
                .px(px(10.0))
                .gap(px(10.0))
                .items_center()
                .rounded(t.radius.md)
                .cursor_pointer()
                .when(selected, |el| el.bg(c.selected))
                .when(!selected, |el| el.hover(move |s| s.bg(hover)))
                .on_click(cx.listener(move |this, _, window, cx| this.pick(ix, window, cx)))
                .child(Icon::new(icon).size(px(16.0)).text_color(c.text_muted))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(
                            div()
                                .truncate()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(c.text)
                                .child(machine.name.clone()),
                        )
                        .child(
                            div()
                                .truncate()
                                .tiny(&t)
                                .text_color(c.text_muted)
                                .child(detail),
                        ),
                )
                .into_any_element()
        });
        div()
            .id("machine-picker-layer")
            .absolute()
            .inset_0()
            .occlude()
            .flex()
            .items_start()
            .justify_center()
            .pt(px(96.0))
            .bg(c.background.opacity(0.6))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.close_picker(window, cx);
                }),
            )
            .child(
                v_flex()
                    .id("machine-picker")
                    .debug_selector(|| "machine-picker".into())
                    .key_context(PICKER_CONTEXT)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(|this, _: &PickerUp, _, cx| this.step(-1, cx)))
                    .on_action(cx.listener(|this, _: &PickerDown, _, cx| this.step(1, cx)))
                    .on_action(cx.listener(|this, _: &PickerConfirm, window, cx| {
                        let selected = this.picker.as_ref().map_or(0, |p| p.selected);
                        this.pick(selected, window, cx);
                    }))
                    .on_action(cx.listener(|this, _: &PickerCancel, window, cx| {
                        this.close_picker(window, cx);
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .w(px(480.0))
                    .max_h(px(560.0))
                    .gap(px(10.0))
                    .p(px(14.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.border_strong)
                    .bg(c.surface_menu)
                    .small(&t)
                    .child(
                        h_flex()
                            .gap(px(8.0))
                            .items_center()
                            .child(
                                Icon::new(IconName::SquareTerminal)
                                    .size(px(16.0))
                                    .text_color(c.text_muted),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text)
                                    .child(title),
                            )
                            .child(div().flex_1())
                            .child(
                                div()
                                    .tiny(&t)
                                    .text_color(c.text_muted)
                                    .child("Up/Down, Enter, Esc"),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap(px(6.0))
                            .items_center()
                            .child(div().text_color(c.text_muted).child("Open as"))
                            .children(wheres),
                    )
                    .when(matches!(picker.purpose, Purpose::OpenOn(_)), |el| {
                        el.child(
                            div()
                                .debug_selector(|| "picker-worktree-note".into())
                                .tiny(&t)
                                .text_color(c.text_muted)
                                .child(
                                    "The worktree goes in a repository the app owns (its data \
                                     folder here, ~/.cache/review over SSH), outside your clones. \
                                     Claude starts without the pull request's agent config; \
                                     any other command asks first.",
                                ),
                        )
                    })
                    .child(ui::divider(cx))
                    .child(
                        v_flex()
                            .id("machine-list")
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scroll()
                            .gap(px(2.0))
                            .children(rows),
                    ),
            )
            .into_any_element()
    }
}

impl Terminals {
    /// The card that asks how to start the command of a pull request that
    /// changes agent config. It does not cover the window, so "View diff"
    /// shows the file beside it.
    fn launch_dialog(&self, pending: &PendingLaunch, cx: &mut Context<Self>) -> AnyElement {
        let t = ui::theme(cx);
        let c = &t.colors;
        let check = &pending.prepared.check;
        let info = &pending.prepared.info;
        let command = pending.command();
        let program = CommandLine::parse(command)
            .map_or_else(|_| command.to_owned(), |line| line.program.to_owned());
        let safe = agent::has_safe_mode(command);
        let (repo, number) = (&pending.key.repo.name, pending.key.number);
        let title = match check {
            ConfigCheck::Unchecked(_) => format!("Could not check {repo}#{number}'s agent config"),
            ConfigCheck::Changed(_) => format!("{repo}#{number} changes agent config"),
            ConfigCheck::Clean => format!("Run {program} in {repo}#{number}'s checkout?"),
        };
        let unsafe_note = if safe {
            String::new()
        } else {
            format!(" {program} has no launch that keeps a checkout's agent config out.")
        };
        let lead = match check {
            ConfigCheck::Unchecked(error) => format!(
                "Comparing it with its merge base failed ({error}), so any agent config in \
                 the checkout may be the pull request's.{unsafe_note}"
            ),
            ConfigCheck::Changed(_) => format!(
                "{program} would load these files from the pull request. Hooks and MCP servers \
                 in them run commands on {}.{unsafe_note}",
                info.machine
            ),
            ConfigCheck::Clean => format!(
                "The checkout is someone else's code, and {program} has no launch that keeps \
                 its agent config out. A shell opens there unless you choose to run {program}."
            ),
        };
        let workdir = pending
            .review
            .as_ref()
            .and_then(|review| review.workspace.workdir())
            .filter(|_| safe && check.needs_choice());
        let lead = match workdir {
            Some(dir) => format!(
                "{lead} Starting without them starts {program} in the worktree with your user \
                 settings only, not in {dir}."
            ),
            None => lead,
        };
        let files = check.changes().iter().enumerate().map(|(ix, change)| {
            let path = change.path.clone();
            let shown = match &change.old_path {
                Some(old) => format!("{old} \u{2192} {}", change.path),
                None => change.path.clone(),
            };
            h_flex()
                .gap(px(8.0))
                .items_center()
                .child(
                    div()
                        .w(px(56.0))
                        .flex_none()
                        .tiny(&t)
                        .text_color(c.text_muted)
                        .child(change.kind_label()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .mono(&t)
                        .text_color(c.text)
                        .child(shown),
                )
                .child(
                    Button::new(("agent-config-diff", ix), "View diff", ButtonKind::Ghost)
                        .on_click(cx.listener(move |this, _, _, cx| this.view_diff(&path, cx))),
                )
                .into_any_element()
        });
        let place = if pending.review.is_some() {
            "off the directory the repository's workspace names"
        } else if info.local {
            "in the app's data folder, outside your clones"
        } else {
            "in the app's scratch clone under ~/.cache/review, outside your clones"
        };
        let (safe_label, trust_label) = if safe {
            (
                "Start without the PR's agent config".to_owned(),
                "Trust this PR's config".to_owned(),
            )
        } else {
            (
                format!("Open a shell without {program}"),
                format!("Run {program} anyway"),
            )
        };
        let buttons = if pending.confirming {
            let names = if check.changes().is_empty() {
                "whatever agent config the checkout holds".to_owned()
            } else {
                check
                    .changes()
                    .iter()
                    .map(|change| change.path.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let question = format!(
                "Run {program} on {} in {}, with {names} from this pull request? \
                 Hooks and servers in it run as you.",
                info.machine, info.path
            );
            v_flex()
                .gap(px(8.0))
                .child(
                    div()
                        .debug_selector(|| "agent-trust-question".into())
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(c.danger)
                        .child(question),
                )
                .child(
                    h_flex()
                        .gap(px(8.0))
                        .child(
                            Button::new(
                                "agent-trust-confirm",
                                "Trust and start",
                                ButtonKind::Danger,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.choose(LaunchMode::TrustPr, cx);
                            })),
                        )
                        .child(
                            Button::new("agent-trust-back", "Back", ButtonKind::Ghost)
                                .on_click(cx.listener(|this, _, _, cx| this.ask_trust(false, cx))),
                        ),
                )
        } else {
            v_flex().child(
                h_flex()
                    .gap(px(8.0))
                    .flex_wrap()
                    .child(
                        Button::new("agent-start-safe", safe_label, ButtonKind::Primary).on_click(
                            cx.listener(|this, _, _, cx| this.choose(LaunchMode::Safe, cx)),
                        ),
                    )
                    .child(
                        Button::new("agent-trust", trust_label, ButtonKind::Outline)
                            .on_click(cx.listener(|this, _, _, cx| this.ask_trust(true, cx))),
                    )
                    .child(
                        Button::new("agent-cancel", "Cancel", ButtonKind::Ghost)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel_launch(cx))),
                    ),
            )
        };
        div()
            .id("agent-config-layer")
            .absolute()
            .top(px(96.0))
            .right(px(24.0))
            .w(px(520.0))
            .child(
                v_flex()
                    .id("agent-config-dialog")
                    .debug_selector(|| "agent-config-dialog".into())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .gap(px(10.0))
                    .p(px(14.0))
                    .rounded(t.radius.lg)
                    .border_1()
                    .border_color(c.warn)
                    .bg(c.surface_menu)
                    .small(&t)
                    .child(
                        h_flex()
                            .gap(px(8.0))
                            .items_center()
                            .child(
                                Icon::new(IconName::TriangleAlert)
                                    .size(px(16.0))
                                    .text_color(c.warn),
                            )
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(c.text)
                                    .child(title),
                            ),
                    )
                    .child(div().text_color(c.text_secondary).child(lead))
                    .child(v_flex().gap(px(4.0)).children(files))
                    .child(
                        div()
                            .debug_selector(|| "agent-worktree-place".into())
                            .tiny(&t)
                            .text_color(c.text_muted)
                            .child(format!("Worktree on {}, {place}:", info.machine))
                            .child(div().mono(&t).truncate().child(info.path.clone())),
                    )
                    .child(ui::divider(cx))
                    .child(buttons),
            )
            .into_any_element()
    }
}

impl Render for Terminals {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(picker) = self.picker.clone() {
            return self.picker_view(&picker, cx);
        }
        match self.pending.clone() {
            Some(pending) => self.launch_dialog(&pending, cx),
            None => div().into_any_element(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn devbox() -> Machine {
        machines::list(&[], &["devbox".to_owned()])
            .into_iter()
            .nth(1)
            .expect("devbox")
    }

    fn worktree(path: &str) -> WorktreeInfo {
        WorktreeInfo {
            machine: "devbox".into(),
            local: false,
            path: path.into(),
            head: "b4c0f065a76a".into(),
            note: None,
        }
    }

    /// Every program to absolute `/abs/<name>`, as if found on the PATH.
    fn on_path(program: &str) -> Result<PathBuf, String> {
        Ok(PathBuf::from(format!("/abs/{program}")))
    }

    fn safe_spec(machine: &Machine, path: &str) -> TerminalSpec {
        agent::launch_spec(machine, &worktree(path), LaunchMode::Safe).unwrap()
    }

    #[test]
    fn signing_in_runs_the_forges_login_on_a_plain_host_only() {
        let pc = Machine::this_pc(None);
        let spec = TerminalSpec::sign_in(ForgeKind::GitLab, "gitlab.com", &pc).unwrap();
        assert_eq!(
            spec.command.as_deref(),
            Some("glab auth login --hostname gitlab.com")
        );
        assert_eq!(spec.host, None, "on this PC");
        let fj = TerminalSpec::sign_in(ForgeKind::Forgejo, "git.example.org:2222", &pc).unwrap();
        assert_eq!(
            fj.command.as_deref(),
            Some("fj -H git.example.org:2222 auth login")
        );
        for host in [
            "",
            "-x.org",
            "a.org; rm -rf ~",
            "a.org && b",
            "a b.org",
            "$(id).org",
        ] {
            assert!(
                TerminalSpec::sign_in(ForgeKind::GitHub, host, &pc).is_none(),
                "{host:?}"
            );
        }
    }

    fn local_with_shell(shell: &str) -> Machine {
        Machine::this_pc(Some(&machines::MachineEntry {
            host: machines::LOCAL_HOST.into(),
            shell: Some(shell.into()),
            ..machines::MachineEntry::default()
        }))
    }

    #[test]
    fn a_clone_is_one_quoted_line_for_the_shell_that_reads_it() {
        let url = "git@github.com:o/r.git";
        let remote = clone_command(&devbox(), url, "~/src/it's here").unwrap();
        assert_eq!(
            remote,
            r"git clone git@github.com:o/r.git 'src/it'\''s here'"
        );
        let dashed = clone_command(&devbox(), url, "-oops").unwrap();
        assert_eq!(dashed, "git clone git@github.com:o/r.git ./-oops");

        let dir = if cfg!(windows) {
            "C:/src/a b"
        } else {
            "/src/a b"
        };
        let absolute = path_text(&std::path::absolute(dir).unwrap());
        let pwsh = clone_command(&local_with_shell("pwsh"), url, dir).unwrap();
        assert_eq!(pwsh, format!("git clone '{url}' '{absolute}'"));
        let cmd = clone_command(&local_with_shell("cmd.exe"), url, dir).unwrap();
        assert_eq!(cmd, format!("git clone \"{url}\" \"{absolute}\""));
        let posix = clone_command(&local_with_shell("/bin/bash"), url, dir).unwrap();
        assert_eq!(posix, format!("git clone {url} '{absolute}'"));

        for bad_url in ["", "-u", "--upload-pack=x", " ;rm"] {
            assert!(
                clone_command(&devbox(), bad_url, "x").is_err(),
                "{bad_url:?}"
            );
        }
        assert!(clone_command(&devbox(), url, "  ").is_err());
        assert!(clone_command(&devbox(), url, "a\nb").is_err());
    }

    #[test]
    fn words_are_quoted_so_no_shell_expands_them() {
        let pwsh = |word| quote_for(ShellKind::PowerShell, word).unwrap();
        assert_eq!(pwsh("it's"), "'it''s'");
        assert_eq!(pwsh("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(pwsh("$(rm -rf ~); `x` @a"), "'$(rm -rf ~); `x` @a'");
        let cmd = |word| quote_for(ShellKind::Cmd, word);
        assert_eq!(cmd("a & b | c ^ d").unwrap(), "\"a & b | c ^ d\"");
        assert!(cmd("%PATH%").is_err());
        assert!(cmd("a\"b").is_err());
        let posix = |word| quote_for(ShellKind::Posix, word).unwrap();
        assert_eq!(posix("$(id) `id`"), "'$(id) `id`'");
        for kind in [ShellKind::PowerShell, ShellKind::Cmd, ShellKind::Posix] {
            assert!(quote_for(kind, "a\rb").is_err());
        }
    }

    #[test]
    fn the_clone_terminal_runs_git_then_stays() {
        let spec = TerminalSpec::clone_into(&devbox(), "https://h/o/r.git", "src/r").unwrap();
        assert_eq!(
            spec.command.as_deref(),
            Some("git clone https://h/o/r.git src/r")
        );
        assert_eq!(spec.cwd, None, "in the home directory");
        let config = spec.session_config_with(&on_path).unwrap();
        assert_eq!(config.args[..3], ["-t", "devbox", "--"]);
        let guard = repo::shell::quote(REMOTE_GUARD);
        let run = format!("/bin/sh -c {guard} review git clone https://h/o/r.git src/r");
        let login = format!("/bin/sh -c {guard} review \"${{SHELL:-/bin/sh}}\" -l");
        assert_eq!(
            config.args[3],
            format!(
                "exec /bin/sh -c {guard} review \"${{SHELL:-/bin/sh}}\" -lic {}",
                repo::shell::quote(&format!("{run}; exec {login}"))
            )
        );

        let local =
            TerminalSpec::clone_into(&local_with_shell("pwsh"), "https://h/o/r.git", "/r").unwrap();
        let config = local.session_config_with(&on_path).unwrap();
        assert_eq!(config.args[..3], ["-NoLogo", "-NoExit", "-Command"]);
        assert!(
            config.args[3].starts_with("& '/abs/git' clone 'https://h/o/r.git' '"),
            "{}",
            config.args[3]
        );
    }

    #[test]
    fn a_remote_command_runs_in_a_login_shell_in_the_worktree() {
        let spec = safe_spec(&devbox(), "/home/u/x y/pr-4");
        let config = spec.session_config_with(&on_path).unwrap();
        assert_eq!(config.program, "/abs/ssh", "ssh from the PATH");
        assert_eq!(config.args[..3], ["-t", "devbox", "--"]);
        let guard = repo::shell::quote(REMOTE_GUARD);
        let run =
            format!("/bin/sh -c {guard} review claude --setting-sources user --strict-mcp-config");
        let login = format!("/bin/sh -c {guard} review \"${{SHELL:-/bin/sh}}\" -l");
        assert_eq!(
            config.args[3],
            format!(
                "cd '/home/u/x y/pr-4' && exec /bin/sh -c {guard} review \"${{SHELL:-/bin/sh}}\" -lic {}",
                repo::shell::quote(&format!("{run}; exec {login}"))
            )
        );
        assert_eq!(spec.title(), "devbox \u{b7} x y/pr-4");
    }

    /// The guard, run by a real POSIX shell where there is one: a PATH with
    /// `.` or an empty or relative entry starts nothing, and only a program
    /// found on the PATH runs.
    #[test]
    fn the_remote_guard_only_runs_programs_on_an_absolute_path() {
        let run = |path: Option<&str>, args: &[&str]| {
            let mut command = std::process::Command::new("sh");
            command.arg("-c").arg(REMOTE_GUARD).arg("review").args(args);
            if let Some(path) = path {
                command.env("PATH", path);
            }
            command.output().ok()
        };
        let Some(ran) = run(None, &["sh", "-c", "echo ran"]) else {
            return;
        };
        if !ran.status.success() {
            return;
        }
        assert_eq!(String::from_utf8_lossy(&ran.stdout).trim(), "ran");
        let Some(found) = run(None, &["sh", "-c", "command -v sh"]) else {
            return;
        };
        let found = String::from_utf8_lossy(&found.stdout).trim().to_owned();
        let Some((dir, _)) = found
            .rsplit_once('/')
            .filter(|(dir, _)| dir.starts_with('/'))
        else {
            return;
        };
        for path in [
            format!(".:{dir}"),
            format!("{dir}:"),
            format!("{dir}::/x"),
            format!("bin:{dir}"),
        ] {
            let out = run(Some(&path), &["sh", "-c", "echo ran"]).expect("sh runs");
            assert_eq!(out.status.code(), Some(126), "PATH={path}");
            assert!(out.stdout.is_empty(), "PATH={path}");
        }
        let missing = run(Some(dir), &["no-such-program-here"]).expect("sh runs");
        assert_eq!(missing.status.code(), Some(127));
        let relative = run(Some(dir), &["./sh", "-c", "echo ran"]).expect("sh runs");
        assert!(relative.stdout.is_empty(), "a relative program never runs");
    }

    fn agent(forward: Option<Forward>) -> AgentCommand {
        AgentCommand {
            argv: vec![
                "claude".into(),
                "--mcp-config".into(),
                "{\"mcpServers\":{}}".into(),
                "--".into(),
                "Review it; don't $(stop)".into(),
            ],
            cleared: vec!["CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD".into()],
            forward,
            pr: PrKey::new(
                crate::model::RepoRef::new("github.com", "o", "r"),
                crate::model::PrNumber(4),
            ),
            label: "r#4".into(),
            token: crate::claude_review::mcp::Token::new().unwrap(),
            worktree: worktrees::ReviewWorktree {
                machine: Machine::this_pc(None),
                repo: "~/src/r".into(),
                path: "/home/u/src/r/.claude/worktrees/pr-4".into(),
            },
        }
    }

    #[test]
    fn a_local_agent_starts_as_itself_with_its_words_and_a_cleared_variable() {
        let spec = TerminalSpec {
            machine: machines::THIS_PC.into(),
            cwd: Some("C:/w/pr-4".into()),
            shell: Some("cmd".into()),
            agent: Some(agent(None)),
            ..TerminalSpec::default()
        };
        let config = spec.session_config_with(&on_path).unwrap();
        assert_eq!(config.program, "/abs/claude", "no shell in between");
        assert_eq!(
            config.args,
            [
                "--mcp-config",
                "{\"mcpServers\":{}}",
                "--",
                "Review it; don't $(stop)"
            ]
        );
        assert!(!config.raw_args, "quoted as one word each");
        assert_eq!(
            config
                .env
                .get("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD")
                .map(String::as_str),
            Some("")
        );
        assert_eq!(config.cwd, Some(PathBuf::from("C:/w/pr-4")));
        assert_eq!(
            spec.title(),
            format!("Review r#4 \u{b7} {}", machines::THIS_PC),
            "the tab names the pull request under review"
        );
    }

    #[test]
    fn a_remote_agent_forwards_the_review_port_and_quotes_every_word() {
        let spec = TerminalSpec {
            cwd: Some("/w/pr-4".into()),
            agent: Some(agent(Some(Forward {
                remote: 40_123,
                local: 5_555,
            }))),
            ..TerminalSpec::shell_on(&devbox())
        };
        let config = spec.session_config_with(&on_path).unwrap();
        assert_eq!(config.program, "/abs/ssh");
        assert_eq!(
            config.args[..7],
            [
                "-t",
                "-o",
                "ExitOnForwardFailure=yes",
                "-R",
                "127.0.0.1:40123:127.0.0.1:5555",
                "devbox",
                "--"
            ]
        );
        let line = &config.args[7];
        assert!(line.starts_with("cd /w/pr-4 && exec /bin/sh -c "), "{line}");
        let guard = repo::shell::quote(REMOTE_GUARD);
        let words = repo::shell::join([
            "--mcp-config",
            "{\"mcpServers\":{}}",
            "--",
            "Review it; don't $(stop)",
        ]);
        let run = format!(
            "unset CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD; /bin/sh -c {guard} review claude {words}"
        );
        assert!(
            line.contains(&*repo::shell::quote(&format!(
                "{run}; exec /bin/sh -c {guard} review \"${{SHELL:-/bin/sh}}\" -l"
            ))),
            "{line}"
        );
    }

    #[test]
    fn a_plain_remote_shell_is_just_ssh() {
        let config = TerminalSpec::shell_on(&devbox())
            .session_config_with(&on_path)
            .unwrap();
        assert_eq!(config.args, ["-t", "devbox"]);
        let mut spec = TerminalSpec::shell_on(&devbox());
        spec.host = Some("-oProxyCommand=evil".into());
        assert!(spec.session_config_with(&on_path).is_err());
    }

    #[test]
    fn local_shells_run_the_absolute_command_and_stay() {
        let claude = || Some(("C:/bin/claude.cmd".to_owned(), "--resume"));
        assert_eq!(
            local_args(ShellKind::PowerShell, "pwsh", claude()),
            [
                "-NoLogo",
                "-NoExit",
                "-Command",
                "& 'C:/bin/claude.cmd' --resume"
            ]
        );
        assert_eq!(
            local_args(ShellKind::Cmd, "cmd", claude()),
            ["/S", "/K", "\"\"C:/bin/claude.cmd\" --resume\""]
        );
        assert_eq!(
            local_args(
                ShellKind::Posix,
                "/bin/zsh",
                Some(("/opt/my bin/claude".to_owned(), ""))
            ),
            ["-l", "-i", "-c", "'/opt/my bin/claude'; exec /bin/zsh -l"]
        );
        assert_eq!(
            local_args(
                ShellKind::PowerShell,
                "pwsh",
                Some(("C:/it's/x.exe".to_owned(), ""))
            ),
            ["-NoLogo", "-NoExit", "-Command", "& 'C:/it''s/x.exe'"]
        );
        assert_eq!(local_args(ShellKind::PowerShell, "pwsh", None), ["-NoLogo"]);
    }

    #[test]
    fn the_spec_never_starts_a_relative_program() {
        let spec = TerminalSpec {
            machine: machines::THIS_PC.into(),
            cwd: Some("C:/data/repos/r.worktrees/pr-4".into()),
            command: Some("claude --strict-mcp-config".into()),
            shell: Some("cmd".into()),
            host: None,
            agent: None,
        };
        let config = spec.session_config_with(&on_path).unwrap();
        assert_eq!(config.program, "/abs/cmd");
        assert!(config.raw_args, "cmd reads its own quotes");
        assert_eq!(
            config.args,
            ["/S", "/K", "\"\"/abs/claude\" --strict-mcp-config\""]
        );
        assert_eq!(
            config.cwd,
            Some(PathBuf::from("C:/data/repos/r.worktrees/pr-4"))
        );
        let resolve = |program: &str| match program {
            "cmd" => on_path(program),
            other => term::program::resolve_in(other, None, None).map_err(|e| e.to_string()),
        };
        for command in ["./claude", ".\\claude.cmd", "bin/claude -c"] {
            let spec = TerminalSpec {
                command: Some(command.into()),
                ..spec.clone()
            };
            let Err(error) = spec.session_config_with(&resolve) else {
                panic!("{command} started");
            };
            assert!(error.contains("relative"), "{command}: {error}");
        }
        let flag = TerminalSpec {
            command: Some("-oops".into()),
            ..spec
        };
        assert!(flag.session_config_with(&on_path).is_err());
    }

    #[test]
    fn commands_split_into_program_and_rest() {
        assert_eq!(
            CommandLine::parse(" claude --resume  x ").unwrap(),
            CommandLine {
                program: "claude",
                rest: "--resume  x"
            }
        );
        let quoted = CommandLine::parse("\"C:/Program Files/c/claude.exe\" -c").unwrap();
        assert_eq!(quoted.program, "C:/Program Files/c/claude.exe");
        assert_eq!(quoted.rest, "-c");
        assert_eq!(quoted.stem(), "claude");
        assert!(CommandLine::parse("\"unclosed").is_err());
        assert!(CommandLine::parse("").is_err());
    }

    #[test]
    fn specs_round_trip_through_the_layout() {
        let spec = safe_spec(&devbox(), "/w/pr-4");
        let value = serde_json::to_value(&spec).unwrap();
        assert_eq!(serde_json::from_value::<TerminalSpec>(value).unwrap(), spec);
        let old: TerminalSpec = serde_json::from_str("{\"machine\":\"This PC\"}").unwrap();
        assert_eq!(old.host, None);
    }
}
