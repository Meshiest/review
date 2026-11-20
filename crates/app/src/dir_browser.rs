//! Picking a directory on another machine: one non-interactive `ssh` call
//! per folder lists its subdirectories. It only reads; nothing is made,
//! moved or removed there.
//!
//! The folder goes to the remote `/bin/sh` as a positional argument quoted
//! for it ([`crate::worktrees::remote_line`]), never as script text, since it
//! comes from what the user typed.

use std::time::Duration;

use crate::machines::{Machine, Place};
use crate::worktrees::{self, RemoteRun};

/// How long one listing may take, connecting included.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Bytes of a listing kept; a name cut off at the end is dropped.
const LIST_OUTPUT: usize = 256 * 1024;
/// Characters of an error line shown.
const ERROR_CHARS: usize = 240;

/// Lists the folder `$1` (empty or `~` for the home directory, `~/x` under
/// it, anything else as `cd` reads it from the home directory): prints the
/// home directory, the folder's absolute path, then the name of each
/// subdirectory not starting with a dot, each ending in a NUL byte.
pub const LIST_SCRIPT: &str = r#"d=$1
case $d in '' | '~') d=$HOME ;; '~/'*) d=$HOME/${d#'~/'} ;; esac
cd -- "$d" 2>/dev/null || { printf 'no folder %s\n' "$d" >&2; exit 1; }
printf '%s\0%s\0' "$HOME" "$(pwd)"
for f in *; do if [ -d "$f" ]; then printf '%s\0' "$f"; fi; done"#;

/// One folder's subdirectories on a machine.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// The remote home directory; empty when the machine did not say.
    pub home: String,
    /// The folder, absolute.
    pub dir: String,
    /// Its subdirectories' names, sorted ignoring case.
    pub folders: Vec<String>,
}

impl Listing {
    /// The folder as a workspace stores it: under the home directory as
    /// `~/...`, which every machine's paths compare alike.
    #[must_use]
    pub fn chosen(&self) -> String {
        tilde_form(&self.dir, &self.home)
    }
}

/// Reads [`LIST_SCRIPT`]'s output. Whatever follows the last NUL is a name
/// cut short by the output limit, or nothing, and is dropped; so are names
/// with control characters, which a workspace's directory may not have.
///
/// # Errors
/// When the output has no home and folder, or the folder is not absolute.
pub fn parse_listing(output: &str) -> Result<Listing, String> {
    let mut fields: Vec<&str> = output.split('\0').collect();
    fields.pop();
    let mut fields = fields.into_iter();
    let (Some(home), Some(dir)) = (fields.next(), fields.next()) else {
        return Err("the machine sent no listing".to_owned());
    };
    if !dir.starts_with('/') || dir.contains(char::is_control) {
        return Err(format!(
            "the machine sent an unusable folder name: {}",
            dir.escape_debug()
        ));
    }
    let mut folders: Vec<String> = fields
        .filter(|name| {
            !name.is_empty()
                && !matches!(*name, "." | "..")
                && !name.contains('/')
                && !name.contains(char::is_control)
        })
        .map(str::to_owned)
        .collect();
    folders.sort_by_cached_key(|name| name.to_lowercase());
    folders.dedup();
    Ok(Listing {
        home: home.to_owned(),
        dir: dir.to_owned(),
        folders,
    })
}

/// The folder above `dir` (absolute, `/`-separated); `/` stays `/`.
#[must_use]
pub fn parent(dir: &str) -> String {
    let dir = dir.trim_end_matches('/');
    match dir.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(ix) => dir[..ix].to_owned(),
    }
}

/// `name` in `dir`.
#[must_use]
pub fn child(dir: &str, name: &str) -> String {
    format!("{}/{name}", dir.trim_end_matches('/'))
}

/// `dir` as `~` or `~/...` when it is `home` or under it.
#[must_use]
pub fn tilde_form(dir: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    if home.is_empty() {
        return dir.to_owned();
    }
    if dir.trim_end_matches('/') == home {
        return "~".to_owned();
    }
    match dir
        .strip_prefix(home)
        .and_then(|rest| rest.strip_prefix('/'))
    {
        Some(rest) => format!("~/{rest}"),
        None => dir.to_owned(),
    }
}

/// Lists `dir` on `machine` over `ssh` (see [`LIST_SCRIPT`]). Blocks on the
/// network for up to [`LIST_TIMEOUT`]: call it off the UI thread.
///
/// # Errors
/// One readable line: an invented machine or this PC, or what `ssh` or the
/// remote shell said.
pub fn list_remote(machine: &Machine, dir: &str) -> Result<Listing, String> {
    if machine.fixture {
        return Err(format!(
            "{} is an invented machine; nothing connects to it",
            machine.name
        ));
    }
    let Place::Ssh(host) = &machine.place else {
        return Err("this PC is browsed with the system's folder picker".to_owned());
    };
    let run = RemoteRun {
        args: &[dir],
        stdin: None,
        timeout: LIST_TIMEOUT,
        output: LIST_OUTPUT,
    };
    let output =
        worktrees::remote_sh_limited(host, LIST_SCRIPT, &run).map_err(|error| one_line(&error))?;
    parse_listing(&output)
}

/// `text`'s non-empty lines on one line, shortened.
fn one_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    match line.char_indices().nth(ERROR_CHARS) {
        Some((end, _)) => format!("{}...", &line[..end]),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;

    #[test]
    fn a_listing_is_read_from_nul_separated_fields() {
        let output = "/home/u\0/home/u/src\0beta\0Alpha\0it's\0a b\0\0..\0bad\nname\0cut";
        let listing = parse_listing(output).unwrap();
        assert_eq!(listing.home, "/home/u");
        assert_eq!(listing.dir, "/home/u/src");
        assert_eq!(listing.folders, ["a b", "Alpha", "beta", "it's"]);
        assert_eq!(listing.chosen(), "~/src");

        let empty = parse_listing("/home/u\0/\0").unwrap();
        assert!(empty.folders.is_empty());
        assert_eq!(empty.chosen(), "/");
    }

    #[test]
    fn a_broken_listing_is_an_error() {
        for output in ["", "/home/u", "/home/u\0relative\0", "/home/u\0/a\nb\0"] {
            assert!(parse_listing(output).is_err(), "{output:?}");
        }
    }

    #[test]
    fn paths_move_up_and_down() {
        assert_eq!(parent("/home/u/src"), "/home/u");
        assert_eq!(parent("/home/u/"), "/home");
        assert_eq!(parent("/home"), "/");
        assert_eq!(parent("/"), "/");
        assert_eq!(child("/", "etc"), "/etc");
        assert_eq!(child("/home/u", "a b"), "/home/u/a b");
        assert_eq!(tilde_form("/home/u", "/home/u/"), "~");
        assert_eq!(tilde_form("/home/u/x/y", "/home/u"), "~/x/y");
        assert_eq!(tilde_form("/home/user2", "/home/u"), "/home/user2");
        assert_eq!(tilde_form("/srv", ""), "/srv");
    }

    #[test]
    fn errors_read_as_one_line() {
        assert_eq!(
            one_line("ssh box: \n  banner\n\nssh: connect to host box port 22: timed out\n"),
            "ssh box:; banner; ssh: connect to host box port 22: timed out"
        );
        assert!(one_line(&"x".repeat(1000)).ends_with("..."));
    }

    #[test]
    fn an_invented_machine_is_never_connected_to() {
        let machine = crate::machines::fixtures().pop().unwrap();
        let error = list_remote(&machine, "~").unwrap_err();
        assert!(error.contains("invented"), "{error}");
    }

    /// The folder is one argument to the remote `sh`, whatever it holds.
    #[test]
    fn the_folder_is_quoted_as_one_argument() {
        let evil = "x'; rm -rf ~; echo '$(id)";
        let line = worktrees::remote_line(LIST_SCRIPT, &[evil]);
        assert!(
            line.ends_with(r#" review 'x'\''; rm -rf ~; echo '\''$(id)'"#),
            "{line}"
        );
        assert!(line.starts_with("exec /bin/sh -c 'd=$1"), "{line}");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../.scratch")
            .join(format!("dir-browser-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::path::absolute(dir).unwrap()
    }

    /// Runs the line `ssh` would hand the remote shell in a local `sh`, with
    /// `home` as `$HOME`.
    fn run_locally(sh: &Path, home: &Path, arg: &str) -> std::process::Output {
        Command::new(sh)
            .arg("-c")
            .arg(worktrees::remote_line(LIST_SCRIPT, &[arg]))
            .env("HOME", home)
            .current_dir(home)
            .output()
            .unwrap()
    }

    /// The script and its quoting, run for real where a POSIX `sh` is on
    /// the PATH (Git's on Windows); no `ssh` is involved.
    #[test]
    fn the_listing_script_lists_folders_and_runs_nothing_it_is_given() {
        let Ok(sh) = term::program::resolve("sh") else {
            eprintln!("no sh on the PATH; the script is not run");
            return;
        };
        let home = scratch("home");
        for name in ["src", "a b", "it's", "$(touch pwned)", "-dash", ".hidden"] {
            std::fs::create_dir_all(home.join("projects").join(name)).unwrap();
        }
        std::fs::write(home.join("projects").join("file.txt"), "").unwrap();

        for arg in ["~/projects", "projects"] {
            let output = run_locally(&sh, &home, arg);
            assert!(output.status.success(), "{output:?}");
            let listing = parse_listing(&String::from_utf8_lossy(&output.stdout)).unwrap();
            assert!(listing.dir.ends_with("/projects"), "{listing:?}");
            assert_eq!(
                listing.folders,
                ["$(touch pwned)", "-dash", "a b", "it's", "src"]
            );
        }
        let output = run_locally(&sh, &home, "");
        let listing = parse_listing(&String::from_utf8_lossy(&output.stdout)).unwrap();
        assert_eq!(listing.folders, ["projects"]);
        assert_eq!(listing.chosen(), "~");

        for evil in [
            "x'; touch pwned; echo '",
            "$(touch pwned)",
            "`touch pwned`",
            "; touch pwned",
        ] {
            let output = run_locally(&sh, &home, evil);
            assert!(!output.status.success(), "{evil}: {output:?}");
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(error.contains("no folder"), "{evil}: {error}");
        }
        let output = run_locally(&sh, &home, "projects/$(touch pwned)");
        assert!(output.status.success(), "{output:?}");
        assert!(
            !home.join("pwned").exists() && !home.join("projects").join("pwned").exists(),
            "nothing the folder named ran"
        );
        let _ = std::fs::remove_dir_all(&home);
    }
}
