//! The machines a terminal or a worktree can be on: this PC, and every
//! `Host` of the user's `~/.ssh/config` (wildcard patterns skipped,
//! `Include`s followed), with what `[[machines]]` in `config.toml` adds.
//!
//! Only the config's text is read: never a key, never another file it names
//! (an `IdentityFile`), and never a VPN's device list. Listing a
//! machine does not connect to it; nothing runs there until the user picks it.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use forge::model::RepoRef;
use repo::SshHost;
use serde::Deserialize;

/// What a picker calls this computer.
pub const THIS_PC: &str = "This PC";
/// `host = "local"` in `[[machines]]` sets this PC's extras.
pub const LOCAL_HOST: &str = "local";
/// What "Open on machine" starts when the config names nothing else.
pub const DEFAULT_COMMAND: &str = "claude";
/// `Include` depth past which a loop is assumed.
const MAX_INCLUDE_DEPTH: usize = 16;

/// `[[machines]]`: what one machine adds to its SSH config entry.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineEntry {
    /// A `Host` of the SSH config, or `local` for this PC.
    pub host: String,
    /// Where clones live there: a repository `name` is at `<repos_root>/<name>`.
    pub repos_root: Option<String>,
    /// Clones by `owner/name`, before `repos_root`.
    #[serde(default)]
    pub clones: BTreeMap<String, String>,
    /// What "Open on machine" runs in the worktree. Default `claude`.
    pub command: Option<String>,
    /// The shell a terminal there starts (and runs `command` in). Default:
    /// `pwsh` here, the login shell over SSH.
    pub shell: Option<String>,
}

/// Where a machine is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    Local,
    Ssh(SshHost),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    pub place: Place,
    pub extras: MachineEntry,
    /// An invented machine of a fixtures run: listed, never connected to.
    pub fixture: bool,
}

impl Machine {
    #[must_use]
    pub fn this_pc(extras: Option<&MachineEntry>) -> Self {
        Self {
            name: THIS_PC.to_owned(),
            place: Place::Local,
            extras: extras.cloned().unwrap_or_default(),
            fixture: false,
        }
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.place == Place::Local
    }

    /// What "Open on machine" runs there.
    #[must_use]
    pub fn command(&self) -> &str {
        self.extras
            .command
            .as_deref()
            .filter(|command| !command.trim().is_empty())
            .unwrap_or(DEFAULT_COMMAND)
    }

    #[must_use]
    pub fn shell(&self) -> Option<&str> {
        self.extras
            .shell
            .as_deref()
            .filter(|shell| !shell.trim().is_empty())
    }

    /// The clone of `repo` the config names on this machine, if any: its
    /// `clones` entry, else `<repos_root>/<name>`.
    #[must_use]
    pub fn configured_clone(&self, repo: &RepoRef) -> Option<String> {
        if let Some(path) = self.extras.clones.get(&repo.path()) {
            return Some(path.clone());
        }
        let root = self.extras.repos_root.as_deref()?.trim_end_matches('/');
        Some(format!("{root}/{}", path_part(&repo.name)))
    }

    /// One line about it for the picker.
    #[must_use]
    pub fn detail(&self) -> String {
        let place = match &self.place {
            Place::Local => "this computer".to_owned(),
            Place::Ssh(host) => format!("ssh {host}"),
        };
        let root = self
            .extras
            .repos_root
            .as_deref()
            .map(|root| format!(", clones in {root}"))
            .unwrap_or_default();
        format!("{place}{root}, runs {}", self.command())
    }
}

/// Where a scratch clone of `repo` goes on a machine without one configured:
/// `~/.cache/review/<host>/<owner>/<name>`, each part reduced to characters
/// safe in a path.
#[must_use]
pub fn scratch_clone(repo: &RepoRef) -> String {
    let mut path = format!("~/.cache/review/{}", path_part(&repo.host));
    for part in repo.owner.split('/') {
        path.push('/');
        path.push_str(&path_part(part));
    }
    path.push('/');
    path.push_str(&path_part(&repo.name));
    path
}

fn path_part(text: &str) -> String {
    let part: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if part.trim_matches('.').is_empty() {
        "_".repeat(part.len().max(1))
    } else {
        part
    }
}

/// This PC, then each SSH config host in file order, then hosts only the
/// config's `[[machines]]` name; each with its `[[machines]]` extras. Hosts
/// `ssh` could misread are left out.
#[must_use]
pub fn list(entries: &[MachineEntry], ssh_hosts: &[String]) -> Vec<Machine> {
    let extras = |host: &str| entries.iter().find(|entry| entry.host == host);
    let mut machines = vec![Machine::this_pc(extras(LOCAL_HOST))];
    let configured_only = entries
        .iter()
        .map(|entry| entry.host.as_str())
        .filter(|host| *host != LOCAL_HOST && !ssh_hosts.iter().any(|known| known == host));
    let mut seen = HashSet::new();
    for host in ssh_hosts.iter().map(String::as_str).chain(configured_only) {
        if !seen.insert(host) {
            continue;
        }
        let Ok(ssh) = SshHost::new(host) else {
            continue;
        };
        machines.push(Machine {
            name: host.to_owned(),
            place: Place::Ssh(ssh),
            extras: extras(host).cloned().unwrap_or_default(),
            fixture: false,
        });
    }
    machines
}

/// The machines a fixtures run lists: this PC and invented hosts that are
/// never connected to.
#[must_use]
pub fn fixtures() -> Vec<Machine> {
    let mut machines = vec![Machine::this_pc(None)];
    machines.push(fixture("buildbox", Some("~/src")));
    machines.push(fixture("mac-mini", None));
    machines
}

/// An invented SSH host named `name`, never connected to.
///
/// # Panics
/// If `name` is not a plain host name.
#[must_use]
pub fn fixture(name: &str, repos_root: Option<&str>) -> Machine {
    Machine {
        name: name.to_owned(),
        place: Place::Ssh(SshHost::new(name).expect("a plain host name")),
        extras: MachineEntry {
            host: name.to_owned(),
            repos_root: repos_root.map(str::to_owned),
            ..MachineEntry::default()
        },
        fixture: true,
    }
}

/// `~/.ssh/config`.
#[must_use]
pub fn user_ssh_config() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".ssh").join("config"))
}

/// The `Host` names of the SSH config at `path` and the files it includes,
/// in order, each once. Patterns with `*`, `?` or `!` are skipped. A missing
/// or unreadable file lists nothing. Blocks on the disk.
#[must_use]
pub fn ssh_config_hosts(path: &Path) -> Vec<String> {
    let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut hosts = Vec::new();
    let mut visited = HashSet::new();
    read_config(path, &base, 0, &mut visited, &mut hosts);
    let mut seen = HashSet::new();
    hosts.retain(|host| seen.insert(host.clone()));
    hosts
}

fn read_config(
    path: &Path,
    base: &Path,
    depth: usize,
    visited: &mut HashSet<PathBuf>,
    hosts: &mut Vec<String>,
) {
    if depth > MAX_INCLUDE_DEPTH || !visited.insert(path.to_path_buf()) {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let Some((keyword, args)) = split_line(line) else {
            continue;
        };
        match keyword.to_ascii_lowercase().as_str() {
            "host" => hosts.extend(
                words(args)
                    .into_iter()
                    .filter(|pattern| !pattern.contains(['*', '?', '!'])),
            ),
            "include" => {
                for pattern in words(args) {
                    for file in expand_include(&pattern, base) {
                        read_config(&file, base, depth + 1, visited, hosts);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `Keyword args` or `Keyword=args`, without comments and blank lines.
fn split_line(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line
        .find(|c: char| c.is_whitespace() || c == '=')
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(end);
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=').unwrap_or(rest).trim();
    Some((keyword, rest))
}

/// Arguments split on whitespace, double quotes keeping spaces.
fn words(args: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for ch in args.chars() {
        match ch {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// The files an `Include` names: `~` is the home directory, a relative path
/// is under `~/.ssh`, and `*` or `?` in the file name match like a shell
/// glob, in sorted order as `ssh` reads them.
fn expand_include(pattern: &str, base: &Path) -> Vec<PathBuf> {
    let path = match pattern.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(rest), |home| home.join(rest)),
        None => {
            let path = PathBuf::from(pattern);
            if path.is_absolute() {
                path
            } else {
                base.join(path)
            }
        }
    };
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![path];
    }
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| entry.path().is_file())
        .filter(|entry| glob_match(&name, &entry.file_name().to_string_lossy()))
        .map(|entry| entry.path())
        .collect();
    files.sort();
    files
}

/// `*` and `?` wildcards against a whole name.
fn glob_match(pattern: &str, name: &str) -> bool {
    let (pattern, name): (Vec<char>, Vec<char>) =
        (pattern.chars().collect(), name.chars().collect());
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch/machines-tests")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch dir");
        dir
    }

    #[test]
    fn hosts_come_from_the_config_and_its_includes() {
        let dir = scratch("includes");
        std::fs::create_dir_all(dir.join("conf.d")).unwrap();
        std::fs::write(
            dir.join("config"),
            "# comment\nInclude conf.d/*.conf\nHost devbox\n  HostName 10.0.0.2\n  IdentityFile ~/.ssh/id\n\
             Host *.internal\n  User x\nHost=buildbox \"other box\" !nope\nhost web-? web1\n\
             Match host foo\n  User y\nInclude missing.conf\nInclude config\n",
        )
        .unwrap();
        std::fs::write(dir.join("conf.d").join("b.conf"), "Host zeta\n").unwrap();
        std::fs::write(dir.join("conf.d").join("a.conf"), "Host alpha devbox\n").unwrap();
        std::fs::write(dir.join("conf.d").join("skip.txt"), "Host never\n").unwrap();
        assert_eq!(
            ssh_config_hosts(&dir.join("config")),
            ["alpha", "devbox", "zeta", "buildbox", "other box", "web1"]
        );
        assert!(ssh_config_hosts(&dir.join("absent")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_split_into_keyword_and_words() {
        assert_eq!(split_line("  Host  a b "), Some(("Host", "a b")));
        assert_eq!(split_line("Host=a"), Some(("Host", "a")));
        assert_eq!(split_line("Host = a"), Some(("Host", "a")));
        assert_eq!(split_line("# Host a"), None);
        assert_eq!(words("a \"b c\" d"), ["a", "b c", "d"]);
        assert!(glob_match("*.conf", "a.conf"));
        assert!(glob_match("w?b*", "web1"));
        assert!(!glob_match("*.conf", "a.txt"));
    }

    #[test]
    fn machines_merge_the_config_extras() {
        let entries = vec![
            MachineEntry {
                host: "devbox".into(),
                repos_root: Some("~/dev/".into()),
                command: Some("claude --resume".into()),
                ..MachineEntry::default()
            },
            MachineEntry {
                host: LOCAL_HOST.into(),
                shell: Some("cmd".into()),
                ..MachineEntry::default()
            },
            MachineEntry {
                host: "extra.example.org".into(),
                ..MachineEntry::default()
            },
        ];
        let hosts = ["devbox".to_owned(), "-oProxyCommand=x".to_owned()];
        let machines = list(&entries, &hosts);
        let names: Vec<&str> = machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, [THIS_PC, "devbox", "extra.example.org"]);
        assert_eq!(machines[0].shell(), Some("cmd"));
        assert_eq!(machines[0].command(), DEFAULT_COMMAND);
        assert_eq!(machines[1].command(), "claude --resume");
        let repo = RepoRef::new("github.com", "Lovelace", "review-sandbox");
        assert_eq!(
            machines[1].configured_clone(&repo).as_deref(),
            Some("~/dev/review-sandbox")
        );
        assert_eq!(machines[2].configured_clone(&repo), None);
    }

    #[test]
    fn scratch_clones_stay_under_the_cache() {
        let repo = RepoRef::new("github.com", "Lovelace", "review-sandbox");
        assert_eq!(
            scratch_clone(&repo),
            "~/.cache/review/github.com/Lovelace/review-sandbox"
        );
        let evil = RepoRef::new("h", "../..", "a b;c");
        assert_eq!(scratch_clone(&evil), "~/.cache/review/h/__/__/a_b_c");
    }

    #[test]
    fn the_config_section_parses() {
        let text = "[[machines]]\nhost = \"devbox\"\nrepos_root = \"~/dev\"\n\
                    [machines.clones]\n\"Lovelace/review\" = \"~/src/review\"\n";
        #[derive(Deserialize)]
        struct File {
            machines: Vec<MachineEntry>,
        }
        let file: File = toml::from_str(text).unwrap();
        let machine = &list(&file.machines, &[])[1];
        let repo = RepoRef::new("github.com", "Lovelace", "review");
        assert_eq!(
            machine.configured_clone(&repo).as_deref(),
            Some("~/src/review")
        );
        assert!(toml::from_str::<File>("[[machines]]\nhost = \"a\"\nkey = \"x\"").is_err());
    }
}
