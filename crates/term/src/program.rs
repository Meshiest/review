//! Finding a program on the `PATH` the way a shell would, minus the current
//! directory. A terminal often starts in a checkout someone else wrote: `cmd`
//! looks there before the `PATH`, so a `claude.cmd` in a pull request would
//! run in place of the real one. The terminals start the absolute path this
//! returns instead of a bare name.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// What Windows uses when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ResolveError {
    /// A path relative to some directory (`.\claude`, `bin/claude`), which
    /// would depend on where the program starts.
    #[error("`{0}` is a relative path; name a program on the PATH or give its full path")]
    Relative(String),
    #[error("`{0}` is not a program on the PATH")]
    NotFound(String),
}

/// `program` as an absolute path: itself when absolute, else the first match
/// in the `PATH`'s absolute directories (with `PATHEXT` on Windows). Never
/// the current directory, and never a relative or empty `PATH` entry. Blocks
/// on the disk.
///
/// # Errors
/// When `program` is a relative path, or no `PATH` directory holds it.
pub fn resolve(program: &str) -> Result<PathBuf, ResolveError> {
    resolve_in(
        program,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PATHEXT").as_deref(),
    )
}

/// [`resolve`] with the `PATH` and `PATHEXT` given (`PATHEXT` is only read on
/// Windows).
///
/// # Errors
/// As [`resolve`].
pub fn resolve_in(
    program: &str,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<PathBuf, ResolveError> {
    let exts = extensions(pathext);
    let as_path = Path::new(program);
    if as_path.is_absolute() {
        return candidates(as_path, &exts)
            .into_iter()
            .find(|candidate| is_program(candidate, &exts))
            .ok_or_else(|| ResolveError::NotFound(program.to_owned()));
    }
    if !is_bare_name(program) {
        return Err(ResolveError::Relative(program.to_owned()));
    }
    path.map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|dir| dir.is_absolute())
        .flat_map(|dir| candidates(&dir.join(program), &exts))
        .find(|candidate| is_program(candidate, &exts))
        .ok_or_else(|| ResolveError::NotFound(program.to_owned()))
}

/// One plain file name: no directory, drive or `.`/`..`.
fn is_bare_name(program: &str) -> bool {
    let mut components = Path::new(program).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    ) && !program.contains(['/', '\\'])
}

/// The executable extensions, lowercase with their dot; empty off Windows.
fn extensions(pathext: Option<&OsStr>) -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let text = pathext
        .map(OsStr::to_string_lossy)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT.into());
    text.split(';')
        .map(|ext| ext.trim().to_ascii_lowercase())
        .filter(|ext| ext.len() > 1 && ext.starts_with('.'))
        .collect()
}

/// The files `path` may name: itself, then (on Windows) with each extension
/// appended, as `cmd` tries them.
fn candidates(path: &Path, exts: &[String]) -> Vec<PathBuf> {
    let mut list = vec![path.to_owned()];
    for ext in exts {
        let mut name = OsString::from(path.as_os_str());
        name.push(ext);
        list.push(PathBuf::from(name));
    }
    list
}

#[cfg(windows)]
fn is_program(path: &Path, exts: &[String]) -> bool {
    let executable = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy().to_ascii_lowercase()))
        .is_some_and(|ext| exts.contains(&ext));
    executable && path.is_file()
}

#[cfg(not(windows))]
fn is_program(path: &Path, _: &[String]) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../.scratch/term-tests")
                .join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch dir");
            Self(std::path::absolute(dir).expect("an absolute scratch dir"))
        }

        fn program(&self, sub: &str, name: &str) -> PathBuf {
            let dir = self.0.join(sub);
            std::fs::create_dir_all(&dir).expect("create the dir");
            let file = dir.join(name);
            std::fs::write(&file, "@echo planted\n").expect("write the program");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
                    .expect("make it executable");
            }
            file
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[cfg(windows)]
    fn path_of(dirs: &[PathBuf]) -> OsString {
        std::env::join_paths(dirs).expect("joinable")
    }

    fn exe(name: &str) -> String {
        if cfg!(windows) {
            format!("{name}.cmd")
        } else {
            name.to_owned()
        }
    }

    /// The terminal starts in a pull request's checkout, which holds a fake
    /// `claude.cmd`, as does a folder the PATH names relatively. Only the
    /// PATH's absolute directories count.
    #[test]
    fn a_program_in_the_checkout_is_never_picked() {
        let dir = Dir::new("checkout");
        let planted = dir.program("", &exe("claude"));
        dir.program("nested", &exe("claude"));
        let real = dir.program("bin", &exe("claude"));
        let exts = Some(OsStr::new(".EXE;.CMD"));
        let relative =
            std::env::join_paths([PathBuf::from("."), PathBuf::new(), PathBuf::from("nested")])
                .unwrap();
        let mut with_real = relative.clone();
        with_real.push(if cfg!(windows) { ";" } else { ":" });
        with_real.push(dir.0.join("bin"));
        let previous = std::env::current_dir().expect("a working directory");
        std::env::set_current_dir(&dir.0).expect("enter the checkout");
        let only_relative = resolve_in("claude", Some(&relative), exts);
        let found = resolve_in("claude", Some(&with_real), exts);
        let bare = resolve_in("claude", None, exts);
        std::env::set_current_dir(previous).expect("leave the checkout");
        assert_eq!(only_relative, Err(ResolveError::NotFound("claude".into())));
        assert_eq!(bare, Err(ResolveError::NotFound("claude".into())));
        assert_eq!(found, Ok(real));
        assert_ne!(found, Ok(planted));
    }

    #[test]
    fn relative_program_paths_are_refused() {
        for program in ["./claude", ".\\claude", "bin/claude", "..", "."] {
            assert_eq!(
                resolve_in(program, Some(OsStr::new("/usr/bin")), None),
                Err(ResolveError::Relative(program.to_owned())),
                "{program}"
            );
        }
        assert!(matches!(
            resolve_in("", None, None),
            Err(ResolveError::Relative(_))
        ));
    }

    #[test]
    fn absolute_programs_stay_as_they_are() {
        let dir = Dir::new("absolute");
        let real = dir.program("bin", &exe("tool"));
        assert_eq!(resolve_in(&real.to_string_lossy(), None, None), Ok(real));
        let missing = dir.0.join("bin").join("missing");
        assert!(matches!(
            resolve_in(&missing.to_string_lossy(), None, None),
            Err(ResolveError::NotFound(_))
        ));
    }

    #[cfg(windows)]
    #[test]
    fn only_pathext_extensions_run_on_windows() {
        let dir = Dir::new("pathext");
        dir.program("a", "claude.txt");
        dir.program("a", "claude");
        dir.program("b", "claude.CMD");
        let path = path_of(&[dir.0.join("a"), dir.0.join("b")]);
        let found = resolve_in("claude", Some(&path), Some(OsStr::new(".COM;.EXE;.CMD")));
        assert_eq!(found, Ok(dir.0.join("b").join("claude.cmd")));
        assert_eq!(
            resolve_in("claude.txt", Some(&path), Some(OsStr::new(".EXE"))),
            Err(ResolveError::NotFound("claude.txt".into()))
        );
        assert!(resolve("cmd").is_ok_and(|found| found.is_absolute()));
    }
}
