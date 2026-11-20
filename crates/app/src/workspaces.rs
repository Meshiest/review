//! The workspaces "Review with agent" runs in: the machine, the repository's
//! directory there, the agent's working directory, command and first
//! prompt. They come from `[[workspaces]]` in `config.toml` and are edited
//! on the settings page. A repository may have several; the one marked
//! preferred (chosen in the button's menu) is the one a launch uses.
//!
//! Every change is written to the config file, off the main thread, and
//! only what the file holds is used: a change that cannot be written is
//! undone and says why. A save rewrites only those tables (the rest of the
//! config still loads at startup only).

use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Context, Entity, Global, Task};

use crate::claude_review::command::{self, check_template};
use crate::config::{self, WorkspaceEntry};
use crate::machines::{self, LOCAL_HOST};
use crate::model::RepoRef;
use crate::terminals::CommandLine;

/// A workspace's identity for this run, so a settings row follows its
/// workspace across saves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WorkspaceId(u64);

/// A workspace and its identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    pub id: WorkspaceId,
    pub entry: WorkspaceEntry,
}

/// Where the last write stands.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SaveState {
    #[default]
    Idle,
    Saving,
    Saved,
    /// Not written, and why; the change was undone.
    Failed(String),
}

/// Which workspace a launch on a repository uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pick {
    /// None matches the repository.
    Missing,
    /// The preferred one, or the only one: its index.
    One(usize),
    /// Several match and none is preferred: their indexes, for the user to
    /// choose.
    Choose(Vec<usize>),
}

pub struct Workspaces {
    /// What the config file holds: what launches use.
    saved: Vec<Stored>,
    /// `saved` with the changes on their way to the file.
    wanted: Vec<Stored>,
    /// The config file, or why there is none to write.
    file: Result<PathBuf, String>,
    state: SaveState,
    /// A write runs.
    writing: bool,
    /// `wanted` changed while it ran: write again when it ends.
    again: bool,
    next_id: u64,
    /// A row the settings page adds, for a repository that has none.
    requested: Option<WorkspaceEntry>,
    /// Counts loads from the config, which rebuild the settings rows.
    epoch: u64,
    _save: Option<Task<()>>,
}

struct WorkspacesHandle(Entity<Workspaces>);

impl Global for WorkspacesHandle {}

/// The config file a run writes its workspaces to: the user's, or for a
/// fixtures run one of its own under `.scratch`, never the user's.
///
/// # Errors
/// When the platform has no config directory.
pub fn config_file(fixtures: bool) -> Result<PathBuf, String> {
    if fixtures {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        return Ok(std::env::current_dir()
            .unwrap_or_default()
            .join(".scratch")
            .join("fixture-config")
            .join(format!("{}-{stamp}", std::process::id()))
            .join("config.toml"));
    }
    config::Config::default_path().ok_or_else(|| "this platform has no config directory".to_owned())
}

impl Workspaces {
    /// Installs the global, empty until [`Self::loaded`], writing to `file`.
    pub fn install(file: Result<PathBuf, String>, cx: &mut App) -> Entity<Self> {
        let workspaces = cx.new(|_| Self {
            saved: Vec::new(),
            wanted: Vec::new(),
            file,
            state: SaveState::Idle,
            writing: false,
            again: false,
            next_id: 0,
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

    /// The config's workspaces, as the file holds them.
    pub fn loaded(&mut self, entries: Vec<WorkspaceEntry>, cx: &mut Context<Self>) {
        self.saved = entries
            .into_iter()
            .map(|entry| Stored {
                id: self.new_id(),
                entry,
            })
            .collect();
        self.wanted.clone_from(&self.saved);
        self.epoch += 1;
        cx.notify();
    }

    /// Writes to `file` from now on.
    pub fn set_file(&mut self, file: Result<PathBuf, String>, cx: &mut Context<Self>) {
        self.file = file;
        cx.notify();
    }

    fn new_id(&mut self) -> WorkspaceId {
        self.next_id += 1;
        WorkspaceId(self.next_id)
    }

    /// The workspaces the config file holds.
    #[must_use]
    pub fn saved(&self) -> &[Stored] {
        &self.saved
    }

    /// The workspaces with the changes still being written.
    #[must_use]
    pub fn wanted(&self) -> &[Stored] {
        &self.wanted
    }

    /// The saved workspaces' entries, in order.
    pub fn entries(&self) -> impl Iterator<Item = &WorkspaceEntry> {
        self.saved.iter().map(|stored| &stored.entry)
    }

    #[must_use]
    pub fn saved_entry(&self, id: WorkspaceId) -> Option<&WorkspaceEntry> {
        find_id(&self.saved, id)
    }

    #[must_use]
    pub fn wanted_entry(&self, id: WorkspaceId) -> Option<&WorkspaceEntry> {
        find_id(&self.wanted, id)
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub fn state(&self) -> &SaveState {
        &self.state
    }

    /// Whether a write runs now.
    #[must_use]
    pub fn is_writing(&self) -> bool {
        self.writing
    }

    /// The file changes go to, or why there is none.
    pub fn file(&self) -> Result<&Path, &str> {
        self.file.as_deref().map_err(String::as_str)
    }

    /// The saved workspaces that match `repo`.
    #[must_use]
    pub fn matching(&self, repo: &RepoRef) -> Vec<&Stored> {
        self.saved
            .iter()
            .filter(|stored| stored.entry.is_for(repo))
            .collect()
    }

    /// The saved workspace a launch on `repo` uses (see [`pick`]): `None`
    /// when none or several without a preference match.
    #[must_use]
    pub fn for_repo(&self, repo: &RepoRef) -> Option<&Stored> {
        let entries: Vec<WorkspaceEntry> = self.entries().cloned().collect();
        match pick(&entries, repo) {
            Pick::One(ix) => self.saved.get(ix),
            Pick::Missing | Pick::Choose(_) => None,
        }
    }

    /// What a launch on `repo` does: see [`pick`].
    #[must_use]
    pub fn pick(&self, repo: &RepoRef) -> Pick {
        let entries: Vec<WorkspaceEntry> = self.entries().cloned().collect();
        pick(&entries, repo)
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

    /// Saves one row: `entry` replaces workspace `id`, or is added (as `id`
    /// when given, so a row whose first save failed keeps its identity).
    /// Whether it is preferred stays as it was. Returns its identity.
    pub fn save_entry(
        &mut self,
        id: Option<WorkspaceId>,
        mut entry: WorkspaceEntry,
        cx: &mut Context<Self>,
    ) -> WorkspaceId {
        let id = id.unwrap_or_else(|| self.new_id());
        match self.wanted.iter_mut().find(|stored| stored.id == id) {
            Some(stored) => {
                entry.preferred = stored.entry.preferred;
                stored.entry = entry;
            }
            None => self.wanted.push(Stored { id, entry }),
        }
        self.write(cx);
        id
    }

    /// Removes workspace `id` from the config.
    pub fn remove(&mut self, id: WorkspaceId, cx: &mut Context<Self>) {
        self.wanted.retain(|stored| stored.id != id);
        self.write(cx);
    }

    /// Makes workspace `id` the one `repo` uses, and no other.
    pub fn prefer(&mut self, repo: &RepoRef, id: WorkspaceId, cx: &mut Context<Self>) {
        for stored in &mut self.wanted {
            if stored.entry.is_for(repo) {
                stored.entry.preferred = stored.id == id;
            }
        }
        self.write(cx);
    }

    /// Writes `wanted` to the file in the background; a change made while
    /// one write runs is written after it.
    fn write(&mut self, cx: &mut Context<Self>) {
        let path = match &self.file {
            Ok(path) => path.clone(),
            Err(why) => {
                self.state = SaveState::Failed(format!("Not saved: {why}."));
                self.wanted.clone_from(&self.saved);
                cx.notify();
                return;
            }
        };
        if self.writing {
            self.again = true;
            cx.notify();
            return;
        }
        self.writing = true;
        self.again = false;
        self.state = SaveState::Saving;
        let snapshot = self.wanted.clone();
        let entries: Vec<WorkspaceEntry> = snapshot.iter().map(|s| s.entry.clone()).collect();
        let write = cx
            .background_executor()
            .spawn(async move { config::save_workspaces(&path, &entries) });
        self._save = Some(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| this.written(snapshot, result, cx))
                .ok();
        }));
        cx.notify();
    }

    /// A write of `snapshot` ended: the file holds it, or every change not
    /// in the file is undone.
    fn written(
        &mut self,
        snapshot: Vec<Stored>,
        result: Result<(), config::ConfigError>,
        cx: &mut Context<Self>,
    ) {
        self.writing = false;
        match result {
            Ok(()) => {
                self.saved = snapshot;
                if self.again {
                    self.write(cx);
                    return;
                }
                self.state = SaveState::Saved;
            }
            Err(error) => {
                self.again = false;
                self.wanted.clone_from(&self.saved);
                self.state = SaveState::Failed(format!("Not saved: could not write {error}"));
            }
        }
        cx.notify();
    }
}

fn find_id(stored: &[Stored], id: WorkspaceId) -> Option<&WorkspaceEntry> {
    stored
        .iter()
        .find(|stored| stored.id == id)
        .map(|stored| &stored.entry)
}

/// Which of `entries` a launch on `repo` uses: the preferred one among
/// those that match, else the only one that matches; with several and
/// none preferred, the user chooses.
#[must_use]
pub fn pick(entries: &[WorkspaceEntry], repo: &RepoRef) -> Pick {
    let matching: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.is_for(repo))
        .map(|(ix, _)| ix)
        .collect();
    if let Some(&ix) = matching.iter().find(|&&ix| entries[ix].preferred) {
        return Pick::One(ix);
    }
    match matching.as_slice() {
        [] => Pick::Missing,
        [one] => Pick::One(*one),
        _ => Pick::Choose(matching),
    }
}

/// How the button's menu names a workspace: its machine and directory, and
/// the forge when it names one.
#[must_use]
pub fn describe(entry: &WorkspaceEntry) -> String {
    let machine = if entry.machine == LOCAL_HOST {
        machines::THIS_PC
    } else {
        entry.machine.as_str()
    };
    match &entry.host {
        Some(host) => format!("{machine}: {} ({host})", entry.path),
        None => format!("{machine}: {}", entry.path),
    }
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
    fn a_launch_uses_the_preferred_workspace_or_the_only_one() {
        let repo = RepoRef::new("github.com", "o", "r");
        let other = entry("o/other", "local", "/x");
        let any_forge = entry("o/r", "local", "/any");
        let on_github = WorkspaceEntry {
            host: Some("github.com".into()),
            ..entry("o/r", "devbox", "/github")
        };
        let on_gitlab = WorkspaceEntry {
            host: Some("gitlab.com".into()),
            ..entry("o/r", "local", "/gitlab")
        };
        assert_eq!(pick(std::slice::from_ref(&other), &repo), Pick::Missing);
        assert_eq!(
            pick(
                &[other.clone(), any_forge.clone(), on_gitlab.clone()],
                &repo
            ),
            Pick::One(1),
            "the only match; another forge's does not count"
        );
        let several = [other.clone(), any_forge.clone(), on_github.clone()];
        assert_eq!(pick(&several, &repo), Pick::Choose(vec![1, 2]));
        let preferred = WorkspaceEntry {
            preferred: true,
            ..on_github.clone()
        };
        assert_eq!(
            pick(&[other, any_forge, preferred], &repo),
            Pick::One(2),
            "the preference wins"
        );
    }

    #[test]
    fn workspaces_describe_their_machine_and_directory() {
        assert_eq!(
            describe(&entry("o/r", "local", "C:/src/r")),
            format!("{}: C:/src/r", machines::THIS_PC)
        );
        let remote = WorkspaceEntry {
            host: Some("gitlab.com".into()),
            ..entry("o/r", "devbox", "~/r")
        };
        assert_eq!(describe(&remote), "devbox: ~/r (gitlab.com)");
    }
}
