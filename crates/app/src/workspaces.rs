//! The workspaces "Review with agent" runs in, one per repository: the
//! machine, the repository's directory there, the agent's working directory,
//! command and first prompt. They come from `[[workspaces]]` in
//! `config.toml` and are edited on the settings page; a save rewrites only
//! those tables, off the main thread, and applies at once (the rest of the
//! config still loads at startup only).

use std::path::PathBuf;

use gpui::{App, AppContext as _, Context, Entity, Global, Task};

use crate::claude_review::command::{self, check_template};
use crate::config::{self, WorkspaceEntry};
use crate::machines::{self, LOCAL_HOST};
use crate::model::RepoRef;
use crate::terminals::CommandLine;

/// Where the last save stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SaveState {
    #[default]
    Idle,
    Saving,
    Saved,
    Failed(String),
}

pub struct Workspaces {
    entries: Vec<WorkspaceEntry>,
    /// The config file a save rewrites; `None` keeps the workspaces for
    /// this run only (a fixtures run never writes the user's config).
    path: Option<PathBuf>,
    state: SaveState,
    /// A row the settings page adds, for a repository that has none.
    requested: Option<WorkspaceEntry>,
    /// Counts changes that did not come from the settings page's own rows.
    epoch: u64,
    _save: Option<Task<()>>,
}

struct WorkspacesHandle(Entity<Workspaces>);

impl Global for WorkspacesHandle {}

impl Workspaces {
    /// Installs the global, empty until [`Self::loaded`].
    pub fn install(cx: &mut App) -> Entity<Self> {
        let workspaces = cx.new(|_| Self {
            entries: Vec::new(),
            path: None,
            state: SaveState::Idle,
            requested: None,
            epoch: 0,
            _save: None,
        });
        cx.set_global(WorkspacesHandle(workspaces.clone()));
        workspaces
    }

    #[must_use]
    pub fn global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<WorkspacesHandle>()
            .map(|handle| handle.0.clone())
    }

    /// The config's workspaces, and the file saves go to.
    pub fn loaded(
        &mut self,
        path: Option<PathBuf>,
        entries: Vec<WorkspaceEntry>,
        cx: &mut Context<Self>,
    ) {
        self.path = path;
        self.entries = entries;
        self.epoch += 1;
        cx.notify();
    }

    #[must_use]
    pub fn entries(&self) -> &[WorkspaceEntry] {
        &self.entries
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub fn state(&self) -> &SaveState {
        &self.state
    }

    /// Whether saves reach a file.
    #[must_use]
    pub fn saves_to_file(&self) -> bool {
        self.path.is_some()
    }

    /// The workspace for `repo` (see [`find`]).
    #[must_use]
    pub fn for_repo(&self, repo: &RepoRef) -> Option<&WorkspaceEntry> {
        find(&self.entries, repo)
    }

    /// Asks the settings page for a new row for `repo`, on this PC.
    pub fn request(&mut self, repo: &RepoRef, cx: &mut Context<Self>) {
        self.requested = Some(WorkspaceEntry {
            host: Some(repo.host.clone()),
            repo: repo.path(),
            machine: LOCAL_HOST.to_owned(),
            ..WorkspaceEntry::default()
        });
        cx.notify();
    }

    /// The row asked for, once.
    pub fn take_request(&mut self) -> Option<WorkspaceEntry> {
        self.requested.take()
    }

    /// Applies `entries` now and writes them to the config file in the
    /// background.
    pub fn save(&mut self, entries: Vec<WorkspaceEntry>, cx: &mut Context<Self>) {
        self.entries.clone_from(&entries);
        let Some(path) = self.path.clone() else {
            self.state = SaveState::Saved;
            cx.notify();
            return;
        };
        self.state = SaveState::Saving;
        let write = cx
            .background_executor()
            .spawn(async move { config::save_workspaces(&path, &entries) });
        self._save = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.state = match result {
                    Ok(()) => SaveState::Saved,
                    Err(error) => SaveState::Failed(error.to_string()),
                };
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

/// The workspace for `repo`: one naming its host first, else one for the
/// repository on any forge.
#[must_use]
pub fn find<'a>(entries: &'a [WorkspaceEntry], repo: &RepoRef) -> Option<&'a WorkspaceEntry> {
    let mut matching = entries.iter().filter(|entry| entry.is_for(repo));
    let first = matching.next()?;
    if first.host.is_some() {
        return Some(first);
    }
    Some(matching.find(|entry| entry.host.is_some()).unwrap_or(first))
}

/// What is wrong with `entry`, if anything: `machines` are the machine
/// names the settings offer (`local` for this PC).
///
/// # Errors
/// The first problem, for the row.
pub fn validate(entry: &WorkspaceEntry, machines: &[String]) -> Result<(), String> {
    let repo = entry.repo.trim().trim_matches('/');
    let named = repo
        .rsplit_once('/')
        .is_some_and(|(owner, name)| !owner.is_empty() && !name.is_empty());
    if !named || repo.contains(char::is_whitespace) {
        return Err("The repository is owner/name.".to_owned());
    }
    if !machines.contains(&entry.machine) {
        return Err(format!("{} is not a machine listed here.", entry.machine));
    }
    if entry.path.trim().is_empty() {
        return Err("The directory is empty.".to_owned());
    }
    if entry.path.contains(char::is_control) {
        return Err("The directory has control characters.".to_owned());
    }
    CommandLine::parse(entry.command()).map_err(|error| format!("The command: {error}."))?;
    if let Some(prompt) = entry.prompt.as_deref() {
        check_template(prompt).map_err(|error| format!("The prompt: {error}."))?;
    }
    if let Some(workdir) = entry.workdir() {
        let fold = entry.machine == LOCAL_HOST && cfg!(windows);
        if command::within(&entry.path, workdir, fold) {
            return Err(
                "The repository's directory is inside the working directory, so the pull \
                 request's own agent config would load with yours. Pick directories apart."
                    .to_owned(),
            );
        }
    }
    Ok(())
}

/// The name a workspace stores for `machine`: `local` for this PC.
#[must_use]
pub fn machine_key(machine: &machines::Machine) -> String {
    if machine.is_local() {
        LOCAL_HOST.to_owned()
    } else {
        machine.name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(repo: &str, machine: &str, path: &str) -> WorkspaceEntry {
        WorkspaceEntry {
            repo: repo.into(),
            machine: machine.into(),
            path: path.into(),
            ..WorkspaceEntry::default()
        }
    }

    #[test]
    fn rows_are_checked_before_they_are_saved() {
        let machines = ["local".to_owned(), "devbox".to_owned()];
        assert_eq!(
            validate(&entry("o/r", "local", "C:/src/r"), &machines),
            Ok(())
        );
        let nested = WorkspaceEntry {
            host: Some("gitlab.com".into()),
            ..entry("group/sub/r", "devbox", "~/r")
        };
        assert_eq!(validate(&nested, &machines), Ok(()));
        for (bad, says) in [
            (entry("r", "local", "/r"), "owner/name"),
            (entry("o/", "local", "/r"), "owner/name"),
            (entry("o/r", "elsewhere", "/r"), "not a machine"),
            (entry("o/r", "local", "  "), "empty"),
            (
                WorkspaceEntry {
                    prompt: Some("review {title}".into()),
                    ..entry("o/r", "local", "/r")
                },
                "unknown placeholder",
            ),
            (
                WorkspaceEntry {
                    workdir: Some("/home/u/me".into()),
                    ..entry("o/r", "devbox", "/home/u/me/r")
                },
                "inside the working directory",
            ),
            (
                WorkspaceEntry {
                    command: Some("\"unclosed".into()),
                    ..entry("o/r", "local", "/r")
                },
                "command",
            ),
        ] {
            let error = validate(&bad, &machines).unwrap_err();
            assert!(error.contains(says), "{bad:?}: {error}");
        }
    }

    #[test]
    fn a_repository_finds_its_workspace() {
        let repo = RepoRef::new("github.com", "o", "r");
        let entries = vec![
            entry("o/other", "local", "/x"),
            entry("o/r", "local", "/any"),
            WorkspaceEntry {
                host: Some("github.com".into()),
                ..entry("o/r", "local", "/github")
            },
        ];
        let found = find(&entries, &repo).map(|e| e.path.as_str());
        assert_eq!(found, Some("/github"), "the host's own first");
        assert_eq!(
            find(&entries[..2], &repo).map(|e| e.path.as_str()),
            Some("/any")
        );
        assert!(find(&entries, &RepoRef::new("github.com", "o", "none")).is_none());
    }
}
