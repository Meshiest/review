//! "Review with agent": the agent (Claude Code by default) runs in a
//! terminal tab, in the pull request's worktree, where the user can talk to
//! it. Where is the repository's workspace ([`crate::workspaces`]); a
//! repository without one opens the settings on a new row for it.
//!
//! The launch follows the policy in [`crate::agent`]. The pull request's
//! title, description, diff and threads go in a context file beside the
//! checkout ([`prompt`]), never on the command line, which holds only fixed
//! text, the user's config and the forge's names for the pull request
//! ([`command`]). Claude's findings come back through a local MCP server
//! ([`mcp`]) as draft comments marked "from Claude", and its suggested
//! verdict and summary fill in the Submit review form: nothing is posted, the
//! user reads, edits, adds to the review or discards each.

pub mod command;
pub mod findings;
pub mod mcp;
pub mod prompt;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt as _;
use futures::channel::mpsc;
use gpui::{App, AppContext as _, Context, Entity, Global, Subscription, Task, WeakEntity, Window};
use store::DraftAnchor;

use crate::agent::{self, ConfigCheck, LaunchMode};
use crate::checklist::{Checklist, Purpose as StepsFor};
use crate::config::WorkspaceEntry;
use crate::machines::{Machine, Place};
use crate::model::PrKey;
use crate::panels::PanelKind;
use crate::pr_header::{Facts, verdict_blocked};
use crate::session::{OpenMode, Session};
use crate::sink::Verdict;
use crate::terminal_panel::{Retry, RunState, TerminalPanel};
use crate::terminals::{
    AgentCommand, Forward, LaunchTab, Prepared, TerminalSpec, Terminals, Where,
};
use crate::untrusted::strip_controls;
use crate::workspaces::{self, Pick, WorkspaceId, Workspaces};
use crate::writing::{ReviewSuggestion, Suggested, Writing};
use command::{ADDITIONAL_CLAUDE_MD, Platform, Start, Values};
use findings::{Dropped, Finding};
use mcp::{CommentRequest, Handler, Limits, McpServer, SuggestRequest, Token, ToolRequest};
use repo::SshHost;

/// How long a tool call waits for the app to make its draft.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
/// The first character of a pending comment's line shown in the menu.
const MENU_TEXT: usize = 80;

/// What a "Review with agent" launch needs besides the worktree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewLaunch {
    pub workspace: WorkspaceEntry,
    /// The pull request as its terminal's tab names it: `widgets#41`, or
    /// `widgets!41` for a merge request.
    pub label: String,
    /// The pull request's web page, for the prompt.
    pub url: String,
    /// Claude's MCP server for this launch; `None` for another agent.
    pub mcp: Option<McpLaunch>,
    /// The app's own folders on this PC, as deny rule paths (`~/.../**`).
    pub app_dirs: Vec<String>,
}

/// Where a launch's MCP config file goes: never the worktree, which may sit
/// on any drive with any permissions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpFile {
    /// In the app's per-user data folder on this PC.
    Local(PathBuf),
    /// Under the remote user's `~/.cache/review/mcp`, by this name.
    Remote { host: SshHost, name: String },
}

/// A launch's token and the MCP config that names it. The config goes in a
/// file only the user can read, never on a command line, where any user of
/// the machine could read it.
#[derive(Clone, PartialEq, Eq)]
pub struct McpLaunch {
    pub token: Token,
    /// Over SSH, the port forwarded to the server.
    pub forward: Option<Forward>,
    /// The config file's text.
    pub config: String,
    pub file: McpFile,
}

impl std::fmt::Debug for McpLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpLaunch")
            .field("forward", &self.forward)
            .field("file", &self.file)
            .finish_non_exhaustive()
    }
}

/// Writes `mcp`'s config where its file goes and returns the path the
/// agent opens. Blocks on the disk or `ssh`.
///
/// # Errors
/// When the file cannot be made there: the launch does not start then.
pub fn write_config(machine: &Machine, mcp: &McpLaunch) -> Result<String, String> {
    let written = match &mcp.file {
        McpFile::Local(path) => crate::worktrees::write_private_local(path, &mcp.config),
        McpFile::Remote { name, .. } => {
            crate::worktrees::write_private_remote(machine, name, &mcp.config)
        }
    };
    written.map_err(|error| format!("could not write the review's MCP config: {error}"))
}

/// The config file's name: a hash of the token, which does not give it away.
fn file_name(token: &Token) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(token.as_str().as_bytes());
    let hex: String = digest[..16].iter().map(|b| format!("{b:02x}")).collect();
    format!("{hex}.json")
}

/// A tool call waiting for the app.
struct Call {
    key: PrKey,
    request: ToolRequest,
    reply: std::sync::mpsc::Sender<Result<String, String>>,
}

/// A launch's token and its config file, while they live.
#[derive(Clone)]
struct Issued {
    token: Token,
    file: McpFile,
}

/// A pull request's agent terminal.
struct AgentTerminal {
    panel: WeakEntity<TerminalPanel>,
    issued: Issued,
    _watch: [Subscription; 2],
}

/// The MCP server, started on the first launch, and each pull request's
/// agent terminal with its token.
pub struct AgentReview {
    server: Option<McpServer>,
    calls: mpsc::UnboundedSender<Call>,
    terminals: HashMap<PrKey, AgentTerminal>,
    /// Tokens issued for launches whose terminal has not opened yet.
    issued: HashMap<PrKey, Issued>,
    /// Where this PC's MCP config files go: the app's data folder (a
    /// fixtures run's `.scratch`); `None` until a live run has loaded its
    /// config.
    config_dir: Option<PathBuf>,
    _calls: Task<()>,
}

struct AgentReviewHandle(Entity<AgentReview>);

impl Global for AgentReviewHandle {}

impl AgentReview {
    /// Installs the global. Tool calls are turned into drafts on the main
    /// thread, one at a time. A fixtures run keeps its MCP config files
    /// under `.scratch`, never in the user's data folder.
    pub fn install(fixtures: bool, cx: &mut App) -> Entity<Self> {
        let (calls, mut rx) = mpsc::unbounded::<Call>();
        let review = cx.new(|cx| Self {
            server: None,
            calls,
            terminals: HashMap::new(),
            issued: HashMap::new(),
            config_dir: None,
            _calls: cx.spawn(async move |_, cx| {
                while let Some(call) = rx.next().await {
                    let reply = cx.update(|cx| match call.request {
                        ToolRequest::Comment(request) => add_comment(&call.key, request, cx),
                        ToolRequest::Suggest(request) => suggest_review(&call.key, request, cx),
                    });
                    let _ = call.reply.send(reply);
                }
            }),
        });
        if fixtures {
            // A folder of its own, never swept: fixture runs (and tests)
            // may run side by side.
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let dir = std::env::current_dir()
                .unwrap_or_default()
                .join(".scratch")
                .join("fixture-mcp")
                .join(format!("{}-{stamp}", std::process::id()));
            review.update(cx, |review, _| review.config_dir = Some(dir));
        }
        cx.set_global(AgentReviewHandle(review.clone()));
        review
    }

    #[must_use]
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<AgentReviewHandle>()
            .map(|handle| handle.0.clone())
    }

    /// Where MCP config files go on this PC from now on. Files a run that
    /// ended without removing them left there are removed, off the main
    /// thread.
    pub fn set_config_dir(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let sweep = dir.clone();
        let now = std::time::SystemTime::now();
        cx.background_executor()
            .spawn(async move { crate::worktrees::sweep_private_local(&sweep, now) })
            .detach();
        self.config_dir = Some(dir);
    }

    /// The server's port on this PC, once it runs.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        self.server.as_ref().map(McpServer::port)
    }

    /// The token of `key`'s agent terminal, while it lives.
    #[must_use]
    pub fn token(&self, key: &PrKey) -> Option<&Token> {
        self.terminals
            .get(key)
            .map(|terminal| &terminal.issued.token)
    }

    /// `key`'s agent terminal while its program starts or runs.
    #[must_use]
    pub fn live_panel(&self, key: &PrKey, cx: &App) -> Option<Entity<TerminalPanel>> {
        let panel = self.terminals.get(key)?.panel.upgrade()?;
        matches!(
            panel.read(cx).state(),
            RunState::Starting | RunState::Running
        )
        .then_some(panel)
    }

    fn server(&mut self) -> Result<&McpServer, String> {
        if self.server.is_none() {
            let calls = self.calls.clone();
            let handler: Handler = Arc::new(move |key: &PrKey, request: ToolRequest| {
                let (reply, answer) = std::sync::mpsc::channel();
                calls
                    .unbounded_send(Call {
                        key: key.clone(),
                        request,
                        reply,
                    })
                    .map_err(|_| "The app is closing.".to_owned())?;
                answer
                    .recv_timeout(CALL_TIMEOUT)
                    .map_err(|_| "The app did not answer in time; try again.".to_owned())?
            });
            let server = McpServer::start(handler, Limits::default())
                .map_err(|error| format!("could not start the review's MCP server: {error}"))?;
            self.server = Some(server);
        }
        Ok(self.server.as_ref().expect("started above"))
    }

    /// A new token for a launch on `key` on `machine`, the MCP config naming
    /// it (on a forwarded port over SSH) and where that config's file goes.
    /// A token issued earlier for `key` and never used ends.
    ///
    /// # Errors
    /// When the server cannot start, there is no randomness, or this PC's
    /// config folder is not known yet.
    pub fn issue(
        &mut self,
        key: &PrKey,
        machine: &Machine,
        cx: &mut Context<Self>,
    ) -> Result<McpLaunch, String> {
        let local_dir = self.config_dir.clone();
        let server = self.server()?;
        let token = server
            .issue(key.clone())
            .map_err(|error| format!("no token for the review: {error}"))?;
        let name = file_name(&token);
        let placed = match &machine.place {
            Place::Local => local_dir
                .map(|dir| (McpFile::Local(dir.join(&name)), None))
                .ok_or_else(|| "the app's data folder is not known yet".to_owned()),
            Place::Ssh(host) => random_port().map(|remote| {
                let file = McpFile::Remote {
                    host: host.clone(),
                    name,
                };
                let forward = Forward {
                    remote,
                    local: server.port(),
                };
                (file, Some(forward))
            }),
        };
        let (file, forward) = match placed {
            Ok(placed) => placed,
            Err(error) => {
                server.revoke(&token);
                return Err(error);
            }
        };
        let port = forward.map_or(server.port(), |forward| forward.remote);
        let config = McpServer::config_json(port, &token);
        let issued = Issued {
            token: token.clone(),
            file: file.clone(),
        };
        if let Some(old) = self.issued.insert(key.clone(), issued) {
            self.retire(&old, cx);
        }
        Ok(McpLaunch {
            token,
            forward,
            config,
            file,
        })
    }

    /// Ends `token` and removes its file, unless a terminal runs with it.
    pub fn forget(&mut self, token: &Token, cx: &mut Context<Self>) {
        let running = self.terminals.values().any(|t| t.issued.token == *token);
        let key = self
            .issued
            .iter()
            .find(|(_, issued)| issued.token == *token)
            .map(|(key, _)| key.clone());
        if let Some(issued) = key.and_then(|key| self.issued.remove(&key))
            && !running
        {
            self.retire(&issued, cx);
        }
    }

    /// Revokes a token and removes its config file, off the main thread.
    fn retire(&self, issued: &Issued, cx: &mut Context<Self>) {
        if let Some(server) = &self.server {
            server.revoke(&issued.token);
        }
        match issued.file.clone() {
            McpFile::Local(path) => {
                cx.background_executor()
                    .spawn(async move {
                        let _ = std::fs::remove_file(path);
                    })
                    .detach();
            }
            McpFile::Remote { host, name } => {
                let _ = std::thread::Builder::new()
                    .name("review-mcp-remove".to_owned())
                    .spawn(move || crate::worktrees::remove_private_remote(&host, &name));
            }
        }
    }

    /// Follows `panel`, which runs `agent`: its token ends when its program
    /// does or the panel closes, and a click on "Review with agent" while it
    /// runs brings it forward.
    pub fn attach(
        &mut self,
        agent: &AgentCommand,
        panel: &Entity<TerminalPanel>,
        cx: &mut Context<Self>,
    ) {
        let key = agent.pr.clone();
        let issued = match self.issued.get(&key) {
            Some(issued) if issued.token == agent.token => self.issued.remove(&key),
            _ => None,
        };
        let Some(issued) = issued else {
            return;
        };
        let ended = key.clone();
        let exited = cx.observe(panel, move |this, panel, cx| {
            if matches!(
                panel.read(cx).state(),
                RunState::Exited(_) | RunState::Failed(_)
            ) {
                this.end(&ended, cx);
            }
        });
        let released = key.clone();
        let closed = cx.observe_release(panel, move |this, _, cx| this.end(&released, cx));
        let terminal = AgentTerminal {
            panel: panel.downgrade(),
            issued,
            _watch: [exited, closed],
        };
        if let Some(old) = self.terminals.insert(key, terminal) {
            self.retire(&old.issued, cx);
        }
        cx.notify();
    }

    /// Ends `key`'s terminal's token and config file.
    fn end(&mut self, key: &PrKey, cx: &mut Context<Self>) {
        if let Some(terminal) = self.terminals.get(key) {
            let issued = terminal.issued.clone();
            self.retire(&issued, cx);
            cx.notify();
        }
    }
}

/// Ends the token of a review launch that will not start.
pub fn forget(review: Option<&ReviewLaunch>, cx: &mut App) {
    let (Some(mcp), Some(state)) = (review.and_then(|r| r.mcp.as_ref()), AgentReview::global(cx))
    else {
        return;
    };
    state.update(cx, |state, cx| state.forget(&mcp.token, cx));
}

/// The terminal spec for Claude, with its MCP config file written.
fn claude_terminal(
    key: &PrKey,
    machine: &Machine,
    prepared: &Prepared,
    review: &ReviewLaunch,
    mode: LaunchMode,
) -> Result<TerminalSpec, String> {
    let workspace = &review.workspace;
    let mcp = review
        .mcp
        .as_ref()
        .ok_or_else(|| "the review has no MCP server".to_owned())?;
    let mcp_file = prepared
        .mcp_file
        .as_deref()
        .ok_or_else(|| "the review's MCP config was not written".to_owned())?;
    let local = machine.is_local();
    let worktree = prepared.info.path.clone();
    let start = command::start(
        workspace.workdir(),
        mode,
        prepared.check == ConfigCheck::Clean,
    );
    let values = Values {
        number: key.number.0,
        owner: key.repo.owner.clone(),
        name: key.repo.name.clone(),
        host: key.repo.host.clone(),
        url: review.url.clone(),
        context: in_worktree(&worktree, command::CONTEXT_FILE, local),
        worktree: worktree.clone(),
    };
    let prompt = command::expand(command::template(workspace.prompt.as_deref()), &values)?;
    let denied = denied_for(machine, &worktree, &start, review)?;
    let argv = command::claude_argv(&command::ClaudeLaunch {
        command: workspace.command(),
        start: &start,
        mode,
        worktree: &worktree,
        mcp_config: mcp_file,
        denied: &denied,
        prompt: &prompt,
    })?;
    let cwd = match start {
        Start::Workdir(dir) if local => crate::worktrees::expand_home(&dir)
            .to_string_lossy()
            .into_owned(),
        // `ssh` starts in the home directory, and a quoted `~` stays a `~`.
        Start::Workdir(dir) => repo::shell::home_relative(&dir).to_owned(),
        Start::Worktree => worktree,
    };
    Ok(TerminalSpec {
        cwd: Some(cwd),
        agent: Some(AgentCommand {
            argv,
            cleared: vec![ADDITIONAL_CLAUDE_MD.to_owned()],
            forward: mcp.forward,
            pr: key.clone(),
            label: review.label.clone(),
            token: mcp.token.clone(),
        }),
        ..TerminalSpec::shell_on(machine)
    })
}

/// `key` as a terminal's tab names it: the repository's name, then the
/// forge's sigil and the number, from the open pull request or its inbox row.
pub(crate) fn pr_label(key: &PrKey, cx: &App) -> String {
    let session = Session::global(cx).read(cx);
    let sigil = session
        .pr(key)
        .map(|open| open.pr.forge)
        .or_else(|| session.inbox.get(key).map(|row| row.forge))
        .map_or('#', |forge| forge.sigil());
    format!("{}{sigil}{}", key.repo.name, key.number)
}

/// Where the remote machine keeps the app's scratch clones and the review's
/// MCP config files, denied like this PC's own folders.
const REMOTE_APP_DIR: &str = "~/.cache/review/**";

/// The tools a launch denies on `machine`: its platform's credential paths
/// and the app's own folders there.
///
/// # Errors
/// When the worktree or the working directory is inside one of them.
fn denied_for(
    machine: &Machine,
    worktree: &str,
    start: &Start,
    review: &ReviewLaunch,
) -> Result<String, String> {
    let local = machine.is_local();
    let platform = if local {
        Platform::this_pc()
    } else {
        Platform::Remote
    };
    let home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    let fold = local && cfg!(windows);
    let form = |path: &str| {
        if local {
            let path = crate::worktrees::expand_home(path);
            command::rule_form(&path.to_string_lossy(), home.as_deref(), fold)
        } else {
            path.replace('\\', "/")
        }
    };
    let mut inside = vec![("worktree", form(worktree))];
    if let Start::Workdir(dir) = start {
        inside.push(("working directory", form(dir)));
    }
    let remote = [REMOTE_APP_DIR.to_owned()];
    let extra: &[String] = if local { &review.app_dirs } else { &remote };
    command::denied(review.workspace.allow_commands, platform, extra, &inside)
}

/// The app's folders on this PC (its config and data, where tokens and
/// caches live) as deny rule paths; one outside the home directory on
/// Windows has no rule form and is left out.
#[must_use]
pub fn app_dirs(data_dir: Option<&Path>) -> Vec<String> {
    let home = dirs::home_dir().map(|home| home.to_string_lossy().into_owned());
    let config =
        crate::config::Config::default_path().and_then(|path| path.parent().map(Path::to_path_buf));
    let data = data_dir
        .map(Path::to_path_buf)
        .or_else(|| dirs::data_local_dir().map(|dir| dir.join("review")));
    [config, data]
        .into_iter()
        .flatten()
        .filter_map(|dir| {
            let form = command::rule_form(&dir.to_string_lossy(), home.as_deref(), cfg!(windows));
            if form.starts_with("~/") {
                Some(format!("{form}/**"))
            } else if form.starts_with('/') {
                Some(format!("/{form}/**"))
            } else {
                None
            }
        })
        .collect()
}

/// `file` (relative) in `worktree`, absolute, as the agent's machine writes
/// paths.
fn in_worktree(worktree: &str, file: &str, local: bool) -> String {
    if local {
        std::path::Path::new(worktree)
            .join(file)
            .to_string_lossy()
            .into_owned()
    } else {
        format!("{}/{file}", worktree.trim_end_matches('/'))
    }
}

/// A port for the SSH forward on the remote machine, from the dynamic range.
fn random_port() -> Result<u16, String> {
    let mut bytes = [0u8; 2];
    getrandom::fill(&mut bytes).map_err(|error| error.to_string())?;
    Ok(20_000 + u16::from_le_bytes(bytes) % 40_000)
}

/// The terminal a review starts in the prepared worktree: Claude with the
/// review's MCP server, or for any other agent the confirm-first launch of
/// "Open on machine" with the workspace's command.
///
/// # Errors
/// When the MCP config was not written, the prompt has an unknown
/// placeholder, or the command names no program.
pub fn launch_spec(
    key: &PrKey,
    machine: &Machine,
    prepared: &Prepared,
    review: &ReviewLaunch,
    mode: LaunchMode,
) -> Result<TerminalSpec, String> {
    let command = review.workspace.command();
    if !agent::has_safe_mode(command) {
        return agent::command_spec(machine, command, &prepared.info, mode);
    }
    claude_terminal(key, machine, prepared, review, mode)
}

/// Refuses a checkout with symbolic links that lead out of it: Claude reads
/// the worktree without asking, and its permission rules see a link's path,
/// not where it leads.
///
/// # Errors
/// The links, or why they could not be read.
pub fn refuse_links(links: &Result<Vec<String>, String>) -> Result<(), String> {
    match links {
        Ok(links) if links.is_empty() => Ok(()),
        Ok(links) => Err(format!(
            "the pull request has symbolic links that lead outside its checkout, so the agent \
             does not start there: {}",
            links.join(", ")
        )),
        Err(error) => Err(format!(
            "could not check the checkout's symbolic links: {error}"
        )),
    }
}

/// Makes the draft one tool call asks for, or says why not.
fn add_comment(key: &PrKey, request: CommentRequest, cx: &mut App) -> Result<String, String> {
    let session = Session::global(cx);
    let (diff, writing) = {
        let session = session.read(cx);
        let diff = session
            .pr(key)
            .and_then(|open| open.diff.as_ref())
            .map(|diff| Arc::clone(&diff.diff));
        (diff, session.writing_for(key))
    };
    let (Some(diff), Some(writing)) = (diff, writing) else {
        return Err(
            "Not added: the pull request is not open in the app with its diff loaded; ask the \
             user to open it."
                .to_owned(),
        );
    };
    let finding = Finding {
        path: request.path,
        line: request.line,
        start: request.start_line,
        side: request.side,
        body: request.body,
    };
    let anchor = findings::anchor(&finding, &diff).map_err(|why| why.explain(&finding))?;
    let kept = writing.update(cx, |writing, cx| {
        writing.add_from_claude(vec![(anchor, finding.body.clone())], cx)
    });
    if !kept.is_empty() {
        return Err(Dropped::DraftThere.explain(&finding));
    }
    let side = match finding.side {
        forge::model::Side::New => "",
        forge::model::Side::Old => " (old side)",
    };
    Ok(format!(
        "Added a draft on {}:{}{side}. The user reviews it before anything is posted.",
        finding.path, finding.line
    ))
}

/// Puts the suggested verdict and summary in `key`'s Submit review form, or
/// beside the user's own, and says what it did.
fn suggest_review(key: &PrKey, request: SuggestRequest, cx: &mut App) -> Result<String, String> {
    let session = Session::global(cx);
    let Some(writing) = session.read(cx).writing_for(key) else {
        return Err(
            "Not saved: the pull request is not open in the app; ask the user to open it."
                .to_owned(),
        );
    };
    let verdict = request.verdict;
    let blocked = Facts::read(session.read(cx), key, Some(writing.read(cx)))
        .and_then(|facts| verdict_blocked(verdict, &facts.caps, facts.own, facts.forge));
    let suggestion = ReviewSuggestion {
        verdict,
        body: request.body,
    };
    let done = writing.update(cx, |writing, cx| writing.suggest_review(suggestion, cx));
    let mut text = match done {
        Suggested::Filled => format!(
            "Filled in the Submit review form with {} and your summary.",
            verdict.label()
        ),
        Suggested::Offered => "The user already wrote a summary or picked a verdict, so your \
            suggestion is offered beside theirs in the Submit review form."
            .to_owned(),
    };
    if let Some(why) = blocked {
        text.push_str(&format!(
            " {} cannot be used on this pull request ({why}); the user picks another verdict.",
            verdict.label()
        ));
    }
    text.push_str(
        " Nothing is submitted until the user does. Calling suggest_review again replaces \
         this suggestion.",
    );
    Ok(text)
}

/// What a click on "Review with agent" did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Started {
    /// Brought the running or starting agent's tab forward.
    Focused,
    /// Opened the launch tab, which shows the steps.
    Launched,
    /// No workspace matches: the settings opened on a new row for one.
    Settings,
    /// Several match and none is preferred: nothing started. The header's
    /// button opens its menu to ask instead, and the palette lists each.
    Choose,
}

/// "Review with agent" on `key`: brings its agent's tab forward while it
/// starts or runs; otherwise starts the agent in the repository's preferred
/// workspace (or its only one), or opens the settings on a new workspace
/// for the repository.
pub fn start(key: &PrKey, window: &mut Window, cx: &mut App) -> Started {
    if focus_running(key, window, cx) {
        return Started::Focused;
    }
    let Some(workspaces) = Workspaces::global(cx) else {
        return Started::Settings;
    };
    let pick = workspaces.read(cx).pick(&key.repo);
    match pick {
        Pick::Missing => {
            open_workspace_settings(Some(key), window, cx);
            Started::Settings
        }
        Pick::Choose(_) => Started::Choose,
        Pick::One(ix) => {
            let workspace = workspaces.read(cx).saved()[ix].entry.clone();
            launch(key, workspace, cx);
            Started::Launched
        }
    }
}

/// Brings `key`'s agent tab forward while its agent runs or its launch is
/// under way, and says whether there was one.
fn focus_running(key: &PrKey, window: &mut Window, cx: &mut App) -> bool {
    let running = AgentReview::global(cx).and_then(|review| review.read(cx).live_panel(key, cx));
    let panel = running.or_else(|| {
        let panel = Terminals::global(cx)?.read(cx).launch_tab(key, cx)?;
        let stopped = panel.read(cx).checklist().is_some_and(Checklist::stopped);
        (!stopped).then_some(panel)
    });
    let Some(panel) = panel else {
        return false;
    };
    if let Some(shell) = crate::shell::Shell::global(cx) {
        shell.update(cx, |shell, cx| shell.focus_terminal(&panel, window, cx));
    }
    true
}

/// Starts the agent on `key` in `workspace`: its tab opens at once with the
/// steps, which the worktree job then goes through.
pub fn launch(key: &PrKey, workspace: WorkspaceEntry, cx: &mut App) {
    let Some(terminals) = Terminals::global(cx) else {
        return;
    };
    Session::global(cx).update(cx, |session, cx| session.keep(key, cx));
    let machine = terminals
        .read(cx)
        .machines(cx)
        .into_iter()
        .find(|machine| workspaces::machine_key(machine) == workspace.machine);
    let machine_name = machine
        .as_ref()
        .map_or_else(|| workspace.machine.clone(), |machine| machine.name.clone());
    let title = format!("Review {} \u{b7} {machine_name}", pr_label(key, cx));
    // Retry and Continue start the same workspace again.
    let retry: Retry = {
        let (key, workspace) = (key.clone(), workspace.clone());
        Rc::new(move |_, cx| launch(&key, workspace.clone(), cx))
    };
    let remote = workspace.machine != crate::machines::LOCAL_HOST;
    terminals.update(cx, |terminals, cx| {
        let tab = LaunchTab {
            title,
            machine: machine_name.clone(),
            checklist: Checklist::plan(StepsFor::Review, remote),
            retry,
        };
        terminals.open_launch_tab(key, tab, Where::Split, cx);
    });
    let Some(machine) = machine else {
        let error = format!(
            "the workspace's machine {} is neither this PC nor a host of the SSH config",
            workspace.machine
        );
        terminals.update(cx, |terminals, cx| {
            terminals.fail(key.clone(), &workspace.machine, error, cx);
        });
        return;
    };
    let launch = review_inputs(key, cx).and_then(|(context, url)| {
        let mcp = if agent::has_safe_mode(workspace.command()) {
            let state = AgentReview::global(cx)
                .ok_or_else(|| "Review with agent is not available".to_owned())?;
            Some(state.update(cx, |state, cx| state.issue(key, &machine, cx))?)
        } else {
            None
        };
        let data_dir = Session::global(cx)
            .read(cx)
            .services()
            .map(|services| services.config.data_dir());
        Ok((
            context,
            ReviewLaunch {
                workspace,
                label: pr_label(key, cx),
                url,
                mcp,
                app_dirs: app_dirs(data_dir.as_deref()),
            },
        ))
    });
    match launch {
        Ok((context, review)) => terminals.update(cx, |terminals, cx| {
            terminals.review_on(key.clone(), machine, review, context, cx);
        }),
        Err(error) => terminals.update(cx, |terminals, cx| {
            terminals.fail(key.clone(), &machine.name, error, cx);
        }),
    }
}

/// Makes workspace `id` the one `key`'s repository uses, saved in the
/// config, and starts the agent there.
pub fn choose_and_launch(key: &PrKey, id: WorkspaceId, cx: &mut App) {
    let Some(workspaces) = Workspaces::global(cx) else {
        return;
    };
    let Some(workspace) = workspaces.read(cx).saved_entry(id).cloned() else {
        return;
    };
    workspaces.update(cx, |workspaces, cx| workspaces.prefer(&key.repo, id, cx));
    launch(key, workspace, cx);
}

/// The settings page on its Workspaces section; with `key`, a new row for
/// its repository.
pub fn open_workspace_settings(key: Option<&PrKey>, window: &mut Window, cx: &mut App) {
    if let (Some(key), Some(workspaces)) = (key, Workspaces::global(cx)) {
        workspaces.update(cx, |workspaces, cx| workspaces.request(&key.repo, cx));
    }
    if let Some(shell) = crate::shell::Shell::global(cx) {
        shell.update(cx, |shell, cx| shell.show_settings(window, cx));
    }
    if let Some(panel) = crate::settings_panel::SettingsPanel::existing(cx) {
        panel.update(cx, |panel, cx| panel.reveal_workspaces(window, cx));
    }
}

/// The context file's text and the pull request's web page, read now.
fn review_inputs(key: &PrKey, cx: &App) -> Result<(String, String), String> {
    let session = Session::global(cx).read(cx);
    let open = session
        .pr(key)
        .ok_or_else(|| "the pull request is not open".to_owned())?;
    let diff = open
        .diff
        .clone()
        .ok_or_else(|| "the diff has not loaded yet; try again once it shows".to_owned())?;
    let input = prompt::PromptInput {
        title: open.pr.title.to_string(),
        description: open.pr.description.to_string(),
        reference: format!(
            "{}/{}{}{}",
            key.repo.owner,
            key.repo.name,
            open.pr.forge.sigil(),
            key.number
        ),
        head: diff.head_sha.clone(),
        diff: diff.diff,
        threads: open.threads.clone(),
    };
    let context = prompt::build(&input, &prompt::new_token());
    let url = crate::references::pr_url(open.pr.forge, &key.repo, key.number);
    Ok((context, url))
}

/// One of Claude's drafts not yet added to the review, as the menu lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingComment {
    pub anchor: DraftAnchor,
    /// `path:line`.
    pub place: String,
    /// The body's first line, shortened, without control characters.
    pub first_line: String,
}

/// Claude's suggested review, as the menu lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingSuggestion {
    pub verdict: Verdict,
    /// The summary's first line, shortened, without control characters;
    /// empty for no summary.
    pub first_line: String,
}

/// Claude's suggested review in `writing`, until it is submitted or
/// discarded.
#[must_use]
pub fn pending_suggestion(writing: &Writing) -> Option<PendingSuggestion> {
    let suggestion = writing.suggestion()?;
    Some(PendingSuggestion {
        verdict: suggestion.verdict,
        first_line: first_line(&suggestion.body),
    })
}

/// How many things from Claude wait for the user: drafts and a suggestion.
#[must_use]
pub fn pending_count(writing: &Writing) -> usize {
    pending(writing).len() + usize::from(writing.suggestion().is_some())
}

/// `body`'s first line with text, shortened for the menu.
pub(crate) fn first_line(body: &str) -> String {
    let first = body.lines().find(|line| !line.trim().is_empty());
    let first = strip_controls(first.unwrap_or_default().trim()).into_owned();
    match first.char_indices().nth(MENU_TEXT) {
        Some((at, _)) => format!("{}\u{2026}", &first[..at]),
        None => first,
    }
}

/// Claude's drafts in `writing`, in the order they came.
#[must_use]
pub fn pending(writing: &Writing) -> Vec<PendingComment> {
    writing
        .slots()
        .iter()
        .filter(|slot| writing.is_from_claude(&slot.anchor))
        .map(|slot| {
            let place = match &slot.anchor {
                DraftAnchor::Line { path, line, .. } => format!("{path}:{}", line.line),
                DraftAnchor::File { path } => path.clone(),
                _ => "the pull request".to_owned(),
            };
            PendingComment {
                anchor: slot.anchor.clone(),
                place,
                first_line: first_line(&slot.body),
            }
        })
        .collect()
}

/// Shows `anchor`'s draft in `key`'s diff, scrolled to and open.
pub fn jump_to(key: &PrKey, anchor: &DraftAnchor, cx: &mut App) {
    let session = Session::global(cx);
    session.update(cx, |session, cx| {
        session.show(key, PanelKind::Diff, OpenMode::Permanent, cx);
    });
    if let Some(writing) = session.read(cx).writing_for(key) {
        writing.update(cx, |writing, cx| writing.open(anchor.clone(), None, cx));
    }
}

/// Adds each of Claude's drafts on `key` to the pending review.
pub fn add_all(key: &PrKey, cx: &mut App) {
    let Some(writing) = Session::global(cx).read(cx).writing_for(key) else {
        return;
    };
    writing.update(cx, |writing, cx| {
        for comment in pending(writing) {
            writing.queue_draft(&comment.anchor, cx);
        }
    });
}

/// Discards each of Claude's drafts on `key`, and its suggested review
/// with what of it the form still holds unchanged.
pub fn discard_all(key: &PrKey, cx: &mut App) {
    let Some(writing) = Session::global(cx).read(cx).writing_for(key) else {
        return;
    };
    writing.update(cx, |writing, cx| {
        for comment in pending(writing) {
            writing.discard(&comment.anchor, cx);
        }
        writing.discard_suggestion(cx);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ConfigCheck;
    use crate::machines::{self, MachineEntry};
    use crate::model::{PrNumber, RepoRef};
    use crate::worktrees::WorktreeInfo;

    fn key() -> PrKey {
        PrKey::new(RepoRef::new("github.com", "o", "r"), PrNumber(41))
    }

    fn prepared(path: &str, check: ConfigCheck) -> Prepared {
        Prepared {
            info: WorktreeInfo {
                machine: "devbox".into(),
                local: false,
                path: path.into(),
                head: "abc".into(),
                note: None,
            },
            check,
            links: Ok(Vec::new()),
            mcp_file: Some("/home/u/.cache/review/mcp/ab.json".into()),
        }
    }

    fn review(workdir: Option<&str>, token: &Token) -> ReviewLaunch {
        ReviewLaunch {
            label: "r#41".into(),
            workspace: WorkspaceEntry {
                repo: "o/r".into(),
                machine: "devbox".into(),
                path: "~/src/r".into(),
                workdir: workdir.map(str::to_owned),
                prompt: Some("/code-review {url} see {context}".into()),
                ..WorkspaceEntry::default()
            },
            url: "https://github.com/o/r/pull/41".into(),
            mcp: Some(McpLaunch {
                token: token.clone(),
                forward: Some(Forward {
                    remote: 40_001,
                    local: 5555,
                }),
                config: McpServer::config_json(40_001, token),
                file: McpFile::Remote {
                    host: SshHost::new("devbox").unwrap(),
                    name: file_name(token),
                },
            }),
            app_dirs: vec!["~/.local/share/review/**".into()],
        }
    }

    fn devbox() -> Machine {
        machines::list(
            &[MachineEntry {
                host: "devbox".into(),
                ..MachineEntry::default()
            }],
            &[],
        )
        .remove(1)
    }

    #[test]
    fn a_remote_review_forwards_the_server_and_names_the_config_file() {
        let token = Token::new().unwrap();
        let spec = claude_terminal(
            &key(),
            &devbox(),
            &prepared("/home/u/src/r/.claude/worktrees/pr-41", ConfigCheck::Clean),
            &review(Some("~/me"), &token),
            LaunchMode::Safe,
        )
        .unwrap();
        let agent = spec.agent.clone().unwrap();
        assert_eq!(
            agent.forward,
            Some(Forward {
                remote: 40_001,
                local: 5555
            })
        );
        assert_eq!(spec.cwd.as_deref(), Some("me"), "home-relative over SSH");
        assert!(
            agent
                .argv
                .windows(2)
                .any(|w| w == ["--mcp-config", "/home/u/.cache/review/mcp/ab.json"])
        );
        let denied = agent
            .argv
            .windows(2)
            .find(|w| w[0] == "--disallowedTools")
            .map(|w| w[1].clone())
            .unwrap();
        assert!(
            denied.contains("Read(~/Library/Keychains/**)"),
            "a remote machine"
        );
        assert!(
            !denied.contains(".local/share/review/**"),
            "this PC's folders are not the machine's"
        );
        assert!(
            denied.contains("Read(~/.cache/review/**)"),
            "the machine's MCP configs and scratch clones"
        );
        assert!(
            agent.argv.iter().all(|arg| !arg.contains(token.as_str())),
            "the token is never on the command line"
        );
        assert_eq!(
            agent.argv.last().unwrap(),
            "/code-review https://github.com/o/r/pull/41 see \
             /home/u/src/r/.claude/worktrees/pr-41/.review/context.md"
        );
        assert!(
            agent
                .argv
                .windows(2)
                .any(|w| w == ["--add-dir", "/home/u/src/r/.claude/worktrees/pr-41"])
        );
        assert_eq!(agent.cleared, [ADDITIONAL_CLAUDE_MD]);
        assert!(!format!("{spec:?}").contains(token.as_str()));
    }

    #[test]
    fn a_changed_config_starts_in_the_worktree_with_the_safe_flags() {
        let token = Token::new().unwrap();
        let changed = ConfigCheck::Unchecked("x".into());
        let spec = claude_terminal(
            &key(),
            &devbox(),
            &prepared("/w/pr-41", changed),
            &review(Some("/home/u/me"), &token),
            LaunchMode::Safe,
        )
        .unwrap();
        assert_eq!(spec.cwd.as_deref(), Some("/w/pr-41"));
        let argv = spec.agent.unwrap().argv;
        for flag in agent::SAFE_CLAUDE_FLAGS {
            assert!(argv.iter().any(|arg| arg == flag), "{flag}");
        }
        assert!(!argv.iter().any(|arg| arg == "--add-dir"));
        assert!(argv.iter().any(|arg| arg == "--disallowedTools"));
    }

    #[test]
    fn config_files_are_named_by_a_hash_of_the_token() {
        let token = Token::new().unwrap();
        let name = file_name(&token);
        assert_eq!(name.len(), 32 + 5);
        assert!(!name.contains(token.as_str()));
        assert_eq!(name, file_name(&token));
        let dirs = app_dirs(Some(Path::new("/opt/review-data")));
        if !cfg!(windows) {
            assert!(
                dirs.contains(&"//opt/review-data/**".to_owned()),
                "{dirs:?}"
            );
        }
        if let Some(home) = dirs::home_dir() {
            let under = app_dirs(Some(&home.join("rd")));
            assert!(under.contains(&"~/rd/**".to_owned()), "{under:?}");
        }
    }

    #[test]
    fn a_working_directory_in_a_credential_tree_refuses_the_launch() {
        let token = Token::new().unwrap();
        let error = claude_terminal(
            &key(),
            &devbox(),
            &prepared("/home/u/src/r/.claude/worktrees/pr-41", ConfigCheck::Clean),
            &review(Some("~/.config/me"), &token),
            LaunchMode::Safe,
        )
        .unwrap_err();
        assert!(error.contains("~/.config"), "{error}");
        let error = claude_terminal(
            &key(),
            &devbox(),
            &prepared("~/.cache/review/o/r.worktrees/pr-41", ConfigCheck::Clean),
            &review(None, &token),
            LaunchMode::Safe,
        )
        .unwrap_err();
        assert!(error.contains("~/.cache/review"), "{error}");
    }

    #[test]
    fn links_out_of_the_checkout_are_refused() {
        assert!(refuse_links(&Ok(Vec::new())).is_ok());
        assert!(refuse_links(&Ok(vec!["a -> /etc".into()])).is_err());
        assert!(refuse_links(&Err("x".into())).is_err());
    }
}
