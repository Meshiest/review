//! `<config dir>/review/config.toml`: the forges, the inbox sections, the
//! repositories with sections of their own, and the workspaces "Review with
//! agent" runs in.
//!
//! Everything here blocks (disk, child processes); the app calls it from a
//! background task. On first run the file is written from the forge CLIs that
//! are logged in, with comments that explain each field. Each later start asks
//! the CLIs again and appends a forge for each host none of them had reported
//! before; `logins.txt` beside the file remembers the hosts seen, so a forge
//! taken out of the file stays out.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use forge::model::{ForgeKind, RepoRef};
use serde::Deserialize;

/// How long one CLI may take to say which hosts it is logged in to.
const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The scp-style address git uses when a forge gives no other.
const DEFAULT_CLONE_URL: &str = "git@{host}:{owner}/{name}.git";

/// A built-in inbox section a forge contributes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SectionKind {
    ReviewRequested,
    AuthoredByMe,
}

impl SectionKind {
    pub const ALL: [Self; 2] = [Self::ReviewRequested, Self::AuthoredByMe];

    pub fn title(self) -> &'static str {
        match self {
            Self::ReviewRequested => "Review requested",
            Self::AuthoredByMe => "Created by me",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::ReviewRequested => "review_requested",
            Self::AuthoredByMe => "authored_by_me",
        }
    }

    pub fn query(self) -> forge::model::InboxSection {
        match self {
            Self::ReviewRequested => forge::model::InboxSection::ReviewRequested,
            Self::AuthoredByMe => forge::model::InboxSection::AuthoredByMe,
        }
    }
}

fn default_sections() -> Vec<SectionKind> {
    SectionKind::ALL.to_vec()
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForgeEntry {
    pub kind: ForgeKind,
    pub host: String,
    #[serde(default = "default_sections")]
    pub sections: Vec<SectionKind>,
    /// A template with `{host}`, `{owner}` and `{name}`.
    pub clone_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoEntry {
    /// The forge's host, as in `[[forges]]`.
    pub host: String,
    /// `owner/name`; a GitLab owner may be a nested group.
    pub repo: String,
    /// A clone the user already has. Fetched into, never checked out.
    pub clone: Option<PathBuf>,
    /// The URL git fetches from, when the forge's `clone_url` does not fit.
    pub remote: Option<String>,
}

impl RepoEntry {
    /// The repository, or `None` when `repo` is not `owner/name`.
    pub fn repo_ref(&self) -> Option<RepoRef> {
        let (owner, name) = self.repo.trim_matches('/').rsplit_once('/')?;
        (!owner.is_empty() && !name.is_empty()).then(|| RepoRef::new(&self.host, owner, name))
    }
}

/// `[[workspaces]]`: where "Review with agent" runs for one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceEntry {
    /// The forge's host; without it, the repository on whichever forge has it.
    pub host: Option<String>,
    /// `owner/name`.
    pub repo: String,
    /// `local` for this PC, or a `Host` of the SSH config.
    #[serde(default = "local_machine")]
    pub machine: String,
    /// The repository's directory there: a clone the pull request's
    /// worktree is added off, or an empty or missing directory to clone into.
    pub path: String,
    /// The agent's working directory: the user's own project, whose agent
    /// config applies. Without it the agent starts in the worktree.
    pub workdir: Option<String>,
    /// The agent's command. Default `claude`.
    pub command: Option<String>,
    /// The first prompt, with placeholders; default: review the pull request.
    pub prompt: Option<String>,
    /// Lets the agent run commands and edit files, which gives it the
    /// user's full file and network access (a shell reads any file) while
    /// the pull request's text, which can ask it for anything, is in its
    /// context. Off by default.
    #[serde(default)]
    pub allow_commands: bool,
    /// The one "Review with agent" uses for its repository when several
    /// match; chosen in the button's menu.
    #[serde(default)]
    pub preferred: bool,
}

fn local_machine() -> String {
    crate::machines::LOCAL_HOST.to_owned()
}

impl WorkspaceEntry {
    /// Whether this workspace is for `repo`.
    #[must_use]
    pub fn is_for(&self, repo: &RepoRef) -> bool {
        let host = self.host.as_deref().is_none_or(|host| host == repo.host);
        host && self.repo.trim_matches('/') == repo.path()
    }

    /// The agent's command, `claude` when none is set.
    #[must_use]
    pub fn command(&self) -> &str {
        self.command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
            .unwrap_or(crate::machines::DEFAULT_COMMAND)
    }

    /// The working directory, when one is set.
    #[must_use]
    pub fn workdir(&self) -> Option<&str> {
        self.workdir.as_deref().filter(|dir| !dir.trim().is_empty())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Where the cache and app-owned bare repositories live.
    pub data_dir: Option<PathBuf>,
    #[serde(default)]
    pub forges: Vec<ForgeEntry>,
    #[serde(default)]
    pub repos: Vec<RepoEntry>,
    /// Extras for the machines of `~/.ssh/config` and this PC.
    #[serde(default)]
    pub machines: Vec<crate::machines::MachineEntry>,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceEntry>,
}

/// A config file that exists but cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no config directory on this platform")]
    NoConfigDir,
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("{}: {message}", path.display())]
    Edit { path: PathBuf, message: String },
}

impl Config {
    /// `<config dir>/review/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("review").join("config.toml"))
    }

    /// # Errors
    /// If the text is not valid config TOML.
    pub fn parse(text: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(text)
    }

    /// Reads the file, or on first run detects the forges, writes the file
    /// and reads it back. Returns the config and whether it was created.
    ///
    /// # Errors
    /// If the file cannot be read, parsed or created.
    pub fn load_or_create(path: &Path) -> Result<(Self, bool), ConfigError> {
        let io_error = |source| ConfigError::Io {
            path: path.to_owned(),
            source,
        };
        let (text, created) = match std::fs::read_to_string(path) {
            Ok(text) => (text, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let detected = detect_forges();
                let text = render(&detected);
                write_atomic(path, &text).map_err(io_error)?;
                let seen = detected.iter().map(|forge| forge.host.clone()).collect();
                // Without the record the next start compares with the file
                // alone, which is as good right after writing it.
                let _ = write_seen(&seen_path(path), &seen);
                (text, true)
            }
            Err(error) => return Err(io_error(error)),
        };
        let config = Self::parse(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })?;
        Ok((config, created))
    }

    /// The configured data dir, or the platform's local data dir.
    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone().unwrap_or_else(|| {
            dirs::data_local_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("review")
        })
    }

    pub fn forge(&self, host: &str) -> Option<&ForgeEntry> {
        self.forges.iter().find(|forge| forge.host == host)
    }

    pub fn repo(&self, repo: &RepoRef) -> Option<&RepoEntry> {
        self.repos
            .iter()
            .find(|entry| entry.repo_ref().as_ref() == Some(repo))
    }

    /// The URL git fetches `repo` from.
    pub fn remote_url(&self, repo: &RepoRef) -> String {
        if let Some(remote) = self.repo(repo).and_then(|entry| entry.remote.clone()) {
            return remote;
        }
        let template = self
            .forge(&repo.host)
            .and_then(|forge| forge.clone_url.as_deref())
            .unwrap_or(DEFAULT_CLONE_URL);
        template
            .replace("{host}", &repo.host)
            .replace("{owner}", &repo.owner)
            .replace("{name}", &repo.name)
    }
}

fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// A TOML basic string.
fn quote(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
}

/// The first-run file: the detected forges, then commented examples.
pub fn render(forges: &[ForgeEntry]) -> String {
    let mut out = String::from(
        "# review configuration, read at startup.\n\
         # Written on first run from the forge CLIs that were logged in (gh, glab, fj);\n\
         # a host one of them logs in to later is appended at startup.\n\
         # Tokens are never stored here; the app asks the CLIs for them.\n\
         \n\
         # Where the cache and app-owned bare repositories live. Default: the\n\
         # platform's local data dir, under `review`.\n\
         # data_dir = \"D:/review-data\"\n\
         \n\
         # One [[forges]] per forge account.\n\
         #   kind       github, gitlab or forgejo\n\
         #   host       the web host\n\
         #   sections   inbox sections it feeds: \"review_requested\", \"authored_by_me\"\n\
         #   clone_url  how git reaches its repositories; {host}, {owner}, {name} are filled in\n",
    );
    if forges.is_empty() {
        out.push_str(
            "# No logged-in CLI was found. Log in with `gh auth login`, `glab auth login`\n\
             # or `fj auth login` and start review again (new logins are added here at\n\
             # startup), or add one:\n\
             # [[forges]]\n\
             # kind = \"github\"\n\
             # host = \"github.com\"\n",
        );
    }
    for forge in forges {
        render_forge(&mut out, forge);
    }
    out.push_str(
        "\n# One [[repos]] per repository that gets its own inbox section, listing its\n\
         # open pull requests.\n\
         #   host    the forge's host, as above\n\
         #   repo    owner/name\n\
         #   clone   optional: a clone you already have; review fetches into it and\n\
         #           never checks out, resets or moves HEAD. Without it, review keeps a\n\
         #           bare repository under data_dir.\n\
         #   remote  optional: the URL git fetches from, when clone_url does not fit\n\
         # [[repos]]\n\
         # host = \"github.com\"\n\
         # repo = \"owner/name\"\n\
         # clone = \"C:/src/name\"\n\
         \n\
         # Terminals and worktrees run on this PC or on a Host of ~/.ssh/config\n\
         # (connected only when you pick it). One [[machines]] per host adds:\n\
         #   host        the Host name, or \"local\" for this PC\n\
         #   repos_root  where clones live there: owner/name is at <repos_root>/<name>;\n\
         #               without one, review keeps a clone under ~/.cache/review there\n\
         #   clones      a clone per repository (\"owner/name\" = path), before repos_root\n\
         #   command     what \"Open on machine\" runs in the worktree (default claude)\n\
         #   shell       the shell a terminal there starts\n\
         # [[machines]]\n\
         # host = \"devbox\"\n\
         # repos_root = \"~/dev\"\n\
         \n\
         # \"Review with agent\" runs the agent in a terminal, in the pull request's\n\
         # worktree. One [[workspaces]] per repository says where; the Workspaces\n\
         # section of the settings page edits them.\n\
         #   host      optional: the forge's host; default, any forge with that repo\n\
         #   repo      owner/name\n\
         #   machine   \"local\" for this PC, or a Host of ~/.ssh/config\n\
         #   path      the repository's directory there: a clone (the worktree goes in\n\
         #             its .claude/worktrees/pr-N), or an empty or missing directory\n\
         #             it is cloned into first\n\
         #   workdir   optional: your own project, where the agent starts so its\n\
         #             .mcp.json, .claude/ settings, skills and CLAUDE.md apply; the\n\
         #             worktree is added with --add-dir. Must not contain path.\n\
         #   command   the agent (default claude); another agent asks before it runs\n\
         #   prompt    the first prompt; {number}, {owner}, {name}, {repo}, {host},\n\
         #             {url}, {context} (the file with the pull request's text) and\n\
         #             {worktree} are filled in; {{ and }} are braces\n\
         #   allow_commands  true lets the agent run commands and edit files: that\n\
         #             gives it your full file and network access while the pull\n\
         #             request's text is in its context. Off by default.\n\
         #   preferred  true on the one Review with agent uses when several\n\
         #             workspaces match the repository; its menu sets it.\n\
         # [[workspaces]]\n\
         # repo = \"owner/name\"\n\
         # machine = \"local\"\n\
         # path = \"C:/src/name\"\n\
         # prompt = \"/code-review {url}\"\n",
    );
    out
}

/// Replaces the `[[workspaces]]` tables of the config at `path` with
/// `workspaces`, leaving the rest of the file (comments, order, the other
/// tables) as it was. A table that stays keeps its own comments; one added
/// goes at the end. A missing file is created with just these tables.
///
/// # Errors
/// If the file cannot be read, does not parse, or cannot be written.
pub fn save_workspaces(path: &Path, workspaces: &[WorkspaceEntry]) -> Result<(), ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_error(error)),
    };
    let text = with_workspaces(&text, workspaces).map_err(|error| ConfigError::Edit {
        path: path.to_owned(),
        message: error,
    })?;
    write_atomic(path, &text).map_err(io_error)
}

/// `text` with its `[[workspaces]]` tables replaced by `workspaces`.
fn with_workspaces(text: &str, workspaces: &[WorkspaceEntry]) -> Result<String, String> {
    use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, value};

    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    if !doc.contains_key("workspaces") {
        doc.insert("workspaces", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let tables = doc["workspaces"]
        .as_array_of_tables_mut()
        .ok_or_else(|| "`workspaces` is not a list of [[workspaces]] tables".to_owned())?;
    while tables.len() > workspaces.len() {
        tables.remove(tables.len() - 1);
    }
    for (ix, entry) in workspaces.iter().enumerate() {
        if ix == tables.len() {
            tables.push(Table::new());
        }
        let table = tables.get_mut(ix).expect("pushed above");
        let fields = [
            ("host", entry.host.as_deref()),
            ("repo", Some(entry.repo.as_str())),
            ("machine", Some(entry.machine.as_str())),
            ("path", Some(entry.path.as_str())),
            ("workdir", entry.workdir.as_deref()),
            ("command", entry.command.as_deref()),
            ("prompt", entry.prompt.as_deref()),
        ];
        for (name, field) in fields {
            match field.filter(|text| !text.trim().is_empty()) {
                // Unchanged values keep their formatting and comments.
                Some(text) if table.get(name).and_then(Item::as_str) == Some(text) => {}
                Some(text) => {
                    table.insert(name, value(text));
                }
                None => {
                    table.remove(name);
                }
            }
        }
        for (name, on) in [
            ("allow_commands", entry.allow_commands),
            ("preferred", entry.preferred),
        ] {
            match on {
                true if table.get(name).and_then(Item::as_bool) == Some(true) => {}
                true => {
                    table.insert(name, value(true));
                }
                false => {
                    table.remove(name);
                }
            }
        }
    }
    if tables.is_empty() {
        doc.remove("workspaces");
    }
    Ok(doc.to_string())
}

/// Removes the `[[repos]]` tables for `repo` from the config at `path`,
/// leaving the rest of the file (comments, order, the other tables) as it
/// was. A missing file or a repository that is not listed changes nothing.
///
/// # Errors
/// If the file cannot be read, does not parse, or cannot be written.
pub fn remove_repo(path: &Path, repo: &RepoRef) -> Result<(), ConfigError> {
    let io_error = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(io_error(error)),
    };
    let edited = without_repo(&text, repo).map_err(|message| ConfigError::Edit {
        path: path.to_owned(),
        message,
    })?;
    if edited == text {
        return Ok(());
    }
    write_atomic(path, &edited).map_err(io_error)
}

/// `text` without the `[[repos]]` tables for `repo`, matched by host and
/// `owner/name` as [`Config::repo`] does.
fn without_repo(text: &str, repo: &RepoRef) -> Result<String, String> {
    use toml_edit::{DocumentMut, Item, Table};

    let is_repo = |table: &Table| {
        let field = |name: &str| table.get(name).and_then(Item::as_str).map(str::to_owned);
        let (Some(host), Some(name)) = (field("host"), field("repo")) else {
            return false;
        };
        let entry = RepoEntry {
            host,
            repo: name,
            clone: None,
            remote: None,
        };
        entry.repo_ref().as_ref() == Some(repo)
    };

    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| e.to_string())?;
    let Some(item) = doc.get_mut("repos") else {
        return Ok(text.to_owned());
    };
    let tables = item
        .as_array_of_tables_mut()
        .ok_or_else(|| "`repos` is not a list of [[repos]] tables".to_owned())?;
    let matching: Vec<usize> = tables
        .iter()
        .enumerate()
        .filter(|(_, table)| is_repo(table))
        .map(|(ix, _)| ix)
        .collect();
    if matching.is_empty() {
        return Ok(text.to_owned());
    }
    for ix in matching.into_iter().rev() {
        tables.remove(ix);
    }
    if tables.is_empty() {
        doc.remove("repos");
    }
    Ok(doc.to_string())
}

/// One `[[forges]]` table, after a blank line.
fn render_forge(out: &mut String, forge: &ForgeEntry) {
    let sections: Vec<String> = forge.sections.iter().map(|s| quote(s.key())).collect();
    let _ = write!(
        out,
        "\n[[forges]]\nkind = {}\nhost = {}\nsections = [{}]\n",
        quote(forge.kind.as_str()),
        quote(&forge.host),
        sections.join(", ")
    );
    if let Some(url) = &forge.clone_url {
        let _ = writeln!(out, "clone_url = {}", quote(url));
    }
}

/// `logins.txt` beside the config file: one host per line that a CLI has
/// reported logged in.
#[must_use]
pub fn seen_path(config: &Path) -> PathBuf {
    config.with_file_name("logins.txt")
}

fn read_seen(path: &Path) -> io::Result<Option<BTreeSet<String>>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(
            text.lines()
                .map(str::trim)
                .filter(|line| is_host(line))
                .map(str::to_owned)
                .collect(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn write_seen(path: &Path, seen: &BTreeSet<String>) -> io::Result<()> {
    let text: String = seen.iter().map(|host| format!("{host}\n")).collect();
    write_atomic(path, &text)
}

/// The detected forges to add: hosts the config does not list and no CLI
/// reported before. With no record of what was seen (a config from before
/// the record), everything the config lacks is new.
#[must_use]
pub fn new_logins(
    config: &Config,
    detected: &[ForgeEntry],
    seen: Option<&BTreeSet<String>>,
) -> Vec<ForgeEntry> {
    detected
        .iter()
        .filter(|forge| config.forge(&forge.host).is_none())
        .filter(|forge| seen.is_none_or(|seen| !seen.contains(&forge.host)))
        .cloned()
        .collect()
}

/// Asks the CLIs which hosts they are logged in to, appends a `[[forges]]`
/// for each new one to the config at `path` (the rest of the file as it
/// was), and records every host seen. Returns the forges added.
///
/// # Errors
/// If the file or the record cannot be read or written, or the file does not
/// parse; nothing is added then.
pub fn add_new_logins(path: &Path) -> Result<Vec<ForgeEntry>, ConfigError> {
    add_logins(path, &detect_forges())
}

fn add_logins(path: &Path, detected: &[ForgeEntry]) -> Result<Vec<ForgeEntry>, ConfigError> {
    let io_error = |path: &Path| {
        let path = path.to_owned();
        move |source| ConfigError::Io { path, source }
    };
    let text = std::fs::read_to_string(path).map_err(io_error(path))?;
    let config = Config::parse(&text).map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    let seen_file = seen_path(path);
    let seen = read_seen(&seen_file).map_err(io_error(&seen_file))?;
    let added = new_logins(&config, detected, seen.as_ref());
    if !added.is_empty() {
        let mut text = text;
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("\n# Added at startup from a new CLI login.");
        for forge in &added {
            render_forge(&mut text, forge);
        }
        write_atomic(path, &text).map_err(io_error(path))?;
    }
    let mut all = seen.unwrap_or_default();
    all.extend(config.forges.iter().map(|forge| forge.host.clone()));
    all.extend(detected.iter().map(|forge| forge.host.clone()));
    write_seen(&seen_file, &all).map_err(io_error(&seen_file))?;
    Ok(added)
}

/// The forges whose CLIs are logged in, each with the default sections. The
/// three CLIs are asked at once; one that is missing or slow is skipped.
pub fn detect_forges() -> Vec<ForgeEntry> {
    let probes: [(ForgeKind, &str, &[&str]); 3] = [
        (ForgeKind::GitHub, "gh", &["auth", "status"]),
        (ForgeKind::GitLab, "glab", &["auth", "status"]),
        (ForgeKind::Forgejo, "fj", &["auth", "list"]),
    ];
    let handles: Vec<_> = probes
        .into_iter()
        .map(|(kind, program, args)| {
            thread::spawn(move || (kind, run_cli(program, args).unwrap_or_default()))
        })
        .collect();
    let ports = forgejo_ssh_ports();
    let mut forges = Vec::new();
    for handle in handles {
        let Ok((kind, output)) = handle.join() else {
            continue;
        };
        let hosts = match kind {
            ForgeKind::Forgejo => output
                .lines()
                .map(str::trim)
                .filter(|line| is_host(line))
                .map(str::to_owned)
                .collect(),
            ForgeKind::GitHub | ForgeKind::GitLab => logged_in_hosts(&output),
        };
        for host in hosts {
            let clone_url = ports
                .get(&host)
                .map(|port| format!("ssh://git@{{host}}:{port}/{{owner}}/{{name}}.git"));
            forges.push(ForgeEntry {
                kind,
                host,
                sections: default_sections(),
                clone_url,
            });
        }
    }
    forges
}

/// Hosts from `gh auth status` or `glab auth status`: "Logged in to <host> ...".
fn logged_in_hosts(output: &str) -> Vec<String> {
    let mut hosts: Vec<String> = output
        .lines()
        .filter_map(|line| line.split_once("Logged in to ").map(|(_, rest)| rest))
        .filter_map(|rest| rest.split_whitespace().next())
        .filter(|host| is_host(host))
        .map(str::to_owned)
        .collect();
    hosts.dedup();
    hosts
}

fn is_host(text: &str) -> bool {
    !text.is_empty()
        && text.contains('.')
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
}

/// SSH ports fj knows from its `host:port` aliases, by host. Reads only the
/// alias table of `keys.json`, never the logins.
fn forgejo_ssh_ports() -> BTreeMap<String, u16> {
    #[derive(Deserialize)]
    struct Keys {
        #[serde(default)]
        aliases: BTreeMap<String, String>,
    }
    let Some(data) = dirs::data_dir() else {
        return BTreeMap::new();
    };
    let paths = if cfg!(windows) {
        ["forgejo-cli", "Cyborus"].map(|org| {
            data.join(org)
                .join("forgejo-cli")
                .join("data")
                .join("keys.json")
        })
    } else {
        [
            data.join("forgejo-cli").join("keys.json"),
            data.join("Cyborus").join("keys.json"),
        ]
    };
    paths
        .iter()
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .filter_map(|text| serde_json::from_str::<Keys>(&text).ok())
        .flat_map(|keys| keys.aliases)
        .filter_map(|(alias, host)| {
            let (alias_host, port) = alias.rsplit_once(':')?;
            (alias_host == host).then_some((host, port.parse().ok()?))
        })
        .collect()
}

/// stdout and stderr of a CLI (gh prints its status on stderr), or `None`
/// when it is missing or takes longer than [`DETECT_TIMEOUT`].
fn run_cli(program: &str, args: &[&str]) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // Without it every CLI call flashes a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let child = command.spawn().ok()?;
    let id = child.id();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(DETECT_TIMEOUT) {
        Ok(Ok(output)) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Some(text)
        }
        Ok(Err(_)) => None,
        Err(_) => {
            kill(id);
            None
        }
    }
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
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github() -> ForgeEntry {
        ForgeEntry {
            kind: ForgeKind::GitHub,
            host: "github.com".into(),
            sections: default_sections(),
            clone_url: None,
        }
    }

    fn gitlab() -> ForgeEntry {
        ForgeEntry {
            kind: ForgeKind::GitLab,
            host: "gitlab.com".into(),
            sections: default_sections(),
            clone_url: None,
        }
    }

    /// A fresh directory under the workspace's git-ignored `.scratch`.
    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch")
            .join(format!("config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn new_logins_skip_listed_and_already_seen_hosts() {
        let config = Config::parse(&render(&[github()])).unwrap();
        let detected = [github(), gitlab()];
        let hosts = |forges: Vec<ForgeEntry>| -> Vec<String> {
            forges.into_iter().map(|forge| forge.host).collect()
        };
        assert_eq!(
            hosts(new_logins(&config, &detected, None)),
            ["gitlab.com"],
            "without a record, what the file lacks is new"
        );
        let seen = BTreeSet::from(["gitlab.com".to_owned()]);
        assert!(
            new_logins(&config, &detected, Some(&seen)).is_empty(),
            "a host seen before was taken out on purpose"
        );
    }

    #[test]
    fn a_new_login_is_appended_and_the_rest_of_the_file_kept() {
        let dir = scratch("append");
        let path = dir.join("config.toml");
        let mine = format!("{}\n# my own note\n", render(&[github()]));
        std::fs::write(&path, &mine).unwrap();

        let added = add_logins(&path, &[github(), gitlab()]).unwrap();
        assert_eq!(added, [gitlab()]);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(&mine), "the file as it was comes first");
        let config = Config::parse(&text).unwrap();
        assert!(config.forge("gitlab.com").is_some());

        // Taken out by hand, it stays out on the next start.
        std::fs::write(&path, &mine).unwrap();
        assert!(add_logins(&path, &[github(), gitlab()]).unwrap().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), mine);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_rendered_file_parses_back() {
        let forgejo = ForgeEntry {
            kind: ForgeKind::Forgejo,
            host: "git.example.org".into(),
            sections: vec![SectionKind::ReviewRequested],
            clone_url: Some("ssh://git@{host}:2222/{owner}/{name}.git".into()),
        };
        let text = render(&[github(), forgejo.clone()]);
        let config = Config::parse(&text).expect("rendered config parses");
        assert_eq!(config.forges, [github(), forgejo]);
        assert!(config.repos.is_empty());
        assert!(text.contains("# [[repos]]"), "examples stay commented");
        assert!(Config::parse(&render(&[])).unwrap().forges.is_empty());
    }

    #[test]
    fn repo_sections_and_remote_urls() {
        let text = r#"
            [[forges]]
            kind = "forgejo"
            host = "git.example.org"
            clone_url = "ssh://git@{host}:2222/{owner}/{name}.git"

            [[forges]]
            kind = "gitlab"
            host = "gitlab.com"
            sections = ["authored_by_me"]

            [[repos]]
            host = "gitlab.com"
            repo = "group/sub/project"
            clone = "C:/src/project"

            [[repos]]
            host = "git.example.org"
            repo = "cora/review"
            remote = "https://git.example.org/cora/review.git"
        "#;
        let config = Config::parse(text).unwrap();
        assert_eq!(config.forges[0].sections, SectionKind::ALL);
        assert_eq!(config.forges[1].sections, [SectionKind::AuthoredByMe]);
        let nested = config.repos[0].repo_ref().unwrap();
        assert_eq!(
            (nested.owner.as_str(), nested.name.as_str()),
            ("group/sub", "project")
        );
        assert_eq!(
            config.remote_url(&nested),
            "git@gitlab.com:group/sub/project.git"
        );
        let review = config.repos[1].repo_ref().unwrap();
        assert_eq!(
            config.remote_url(&review),
            "https://git.example.org/cora/review.git"
        );
        let other = RepoRef::new("git.example.org", "cora", "other");
        assert_eq!(
            config.remote_url(&other),
            "ssh://git@git.example.org:2222/cora/other.git"
        );
    }

    #[test]
    fn unknown_fields_are_errors() {
        assert!(
            Config::parse("[[forges]]\nkind = \"github\"\nhost = \"x.y\"\ntoken = \"t\"").is_err()
        );
        assert!(Config::parse("[[forges]]\nkind = \"bitbucket\"\nhost = \"x.y\"").is_err());
    }

    fn workspace(repo: &str, path: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            repo: repo.into(),
            machine: "local".into(),
            path: path.into(),
            ..WorkspaceEntry::default()
        }
    }

    #[test]
    fn workspaces_round_trip_and_keep_the_comments() {
        let dir = scratch("workspaces");
        let path = dir.join("config.toml");
        let mine = format!(
            "{}\n# my note about workspaces\n[[workspaces]]\n# the main one\nrepo = \"o/a\"\n\
             machine = \"local\"\npath = \"C:/src/a\"\n\n[[workspaces]]\nrepo = \"o/gone\"\n\
             path = \"/x\"\n",
            render(&[github()])
        );
        std::fs::write(&path, &mine).unwrap();
        let config = Config::parse(&mine).unwrap();
        assert_eq!(config.workspaces[1].machine, "local", "local by default");

        let mut kept = config.workspaces[0].clone();
        kept.allow_commands = true;
        kept.prompt = Some("/code-review {url}\nthen \"more\"".into());
        kept.workdir = Some("C:/src/me".into());
        let added = WorkspaceEntry {
            host: Some("github.com".into()),
            machine: "devbox".into(),
            command: Some("claude --model opus".into()),
            preferred: true,
            ..workspace("o/b", "~/src/b")
        };
        save_workspaces(&path, &[kept.clone(), added.clone()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my note about workspaces"), "{text}");
        assert!(text.contains("# the main one"), "{text}");
        assert!(text.contains("# One [[forges]] per forge account."));
        assert!(!text.contains("o/gone"));
        let back = Config::parse(&text).unwrap();
        assert_eq!(back.workspaces, [kept, added]);
        assert_eq!(back.forges, [github()], "the rest is as it was");

        save_workspaces(&path, &[]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(Config::parse(&text).unwrap().workspaces.is_empty());
        assert!(text.contains("# One [[forges]] per forge account."));

        let fresh = dir.join("new").join("config.toml");
        save_workspaces(&fresh, &[workspace("o/c", "/c")]).unwrap();
        let back = Config::parse(&std::fs::read_to_string(&fresh).unwrap()).unwrap();
        assert_eq!(back.workspaces, [workspace("o/c", "/c")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn workspaces_match_their_repository() {
        let repo = RepoRef::new("github.com", "o", "a");
        assert!(workspace("o/a", "/a").is_for(&repo));
        assert!(!workspace("o/b", "/a").is_for(&repo));
        let other_host = WorkspaceEntry {
            host: Some("gitlab.com".into()),
            ..workspace("o/a", "/a")
        };
        assert!(!other_host.is_for(&repo));
        assert_eq!(workspace("o/a", "/a").command(), "claude");
    }

    const WITH_REPOS: &str = "# top note\n\
        [[forges]]\nkind = \"github\"\nhost = \"github.com\"\n\n\
        # the first one\n[[repos]]\nhost = \"github.com\"\nrepo = \"o/a\"\nclone = \"C:/src/a\"\n\n\
        [[repos]] # the second\nhost = \"github.com\"\nrepo = \"o/b\"\n\n\
        [[repos]]\nhost = \"gitlab.com\"\nrepo = \"o/a\"\n\n\
        [[workspaces]]\nrepo = \"o/a\"\npath = \"/a\"\n";

    #[test]
    fn without_repo_removes_only_the_matching_table() {
        let repo = RepoRef::new("github.com", "o", "a");
        let text = without_repo(WITH_REPOS, &repo).unwrap();
        let config = Config::parse(&text).unwrap();
        let left: Vec<_> = config
            .repos
            .iter()
            .map(|r| (r.host.as_str(), r.repo.as_str()))
            .collect();
        assert_eq!(left, [("github.com", "o/b"), ("gitlab.com", "o/a")]);
        assert!(!text.contains("C:/src/a"), "{text}");
        assert!(text.contains("# top note"), "{text}");
        assert!(text.contains("# the second"), "{text}");
        assert_eq!(config.workspaces.len(), 1, "other tables stay");
        assert_eq!(config.forges.len(), 1);
    }

    #[test]
    fn without_repo_drops_the_key_with_the_last_table_and_ignores_absent_repos() {
        let one = "# note\n[[forges]]\nkind = \"github\"\nhost = \"github.com\"\n\n\
                   # about a\n[[repos]]\nhost = \"github.com\"\nrepo = \"o/a\"\n";
        let text = without_repo(one, &RepoRef::new("github.com", "o", "a")).unwrap();
        assert!(!text.contains("repos"), "{text}");
        assert!(
            text.contains("# note") && text.contains("[[forges]]"),
            "{text}"
        );
        for absent in [
            RepoRef::new("github.com", "o", "other"),
            RepoRef::new("gitlab.com", "o", "b"),
        ] {
            assert_eq!(without_repo(WITH_REPOS, &absent).unwrap(), WITH_REPOS);
        }
        assert_eq!(
            without_repo("# nothing\n", &RepoRef::new("github.com", "o", "a")).unwrap(),
            "# nothing\n"
        );
    }

    #[test]
    fn without_repo_rejects_a_malformed_file() {
        let repo = RepoRef::new("github.com", "o", "a");
        assert!(without_repo("[[repos]\nhost =", &repo).is_err());
        assert!(without_repo("repos = [1, 2]\n", &repo).is_err());
    }

    #[test]
    fn remove_repo_rewrites_the_file_and_leaves_a_missing_one() {
        let dir = scratch("remove-repo");
        let path = dir.join("config.toml");
        std::fs::write(&path, WITH_REPOS).unwrap();
        remove_repo(&path, &RepoRef::new("github.com", "o", "b")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("o/b"), "{text}");
        assert!(text.contains("# the first one"), "{text}");
        let missing = dir.join("none.toml");
        remove_repo(&missing, &RepoRef::new("github.com", "o", "b")).unwrap();
        assert!(!missing.exists());
        std::fs::write(&path, "[[repos").unwrap();
        assert!(remove_repo(&path, &RepoRef::new("github.com", "o", "b")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosts_from_cli_status() {
        let gh = "github.com\n  ✓ Logged in to github.com account Lovelace (keyring)\n  - Active account: true\n";
        assert_eq!(logged_in_hosts(gh), ["github.com"]);
        let glab = "gitlab.com\n  ✓ Logged in to gitlab.com as Lovelace (C:\\config.yml)\n";
        assert_eq!(logged_in_hosts(glab), ["gitlab.com"]);
        assert!(logged_in_hosts("You are not logged into any GitHub hosts.").is_empty());
    }
}
