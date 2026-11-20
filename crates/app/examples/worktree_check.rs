//! Creates, lists and removes the worktree of pull request 4 of the GitHub
//! sandbox (`REVIEW_SANDBOX_GITHUB`, see `docs/sandboxes.md`), through the
//! app's own worktree code: once here, in an app-owned store under
//! `.scratch/worktree-check`, and once over SSH on the machine named by
//! `REVIEW_REMOTE_HOST`, in a scratch clone under `~/.cache/review` (where
//! an agent's worktree goes). Here it also compares the pull request's agent config
//! with its merge base, as "Open on machine" does. Prints a PASS or
//! FAIL line for each and exits non-zero on a FAIL.
//!
//! cargo run -p app --example worktree_check
//!
//! It cleans up after itself: the local store is deleted, and on the remote
//! machine the worktree, the fetched ref and (when this run made them) the
//! scratch clone and `~/.cache/review` are removed. Only that machine is
//! connected to.

use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use app::agent::{self, ConfigCheck};
use app::machines::{self, Machine};
use app::worktrees::{self, Placement, PrSource, WorktreeInfo};
use forge::model::ForgeKind;
use forge::sandbox::Target;
use repo::{FetchRequest, PrRefs, Worktree};

const NUMBER: u64 = 4;
const HOST_ENV: &str = "REVIEW_REMOTE_HOST";

/// The SSH host the remote check runs on.
fn host() -> Result<String, String> {
    std::env::var(HOST_ENV)
        .ok()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| format!("{HOST_ENV} is not set: it names the SSH machine to check"))
}

fn source() -> Result<PrSource, String> {
    let target = Target::from_env(ForgeKind::GitHub)?;
    Ok(PrSource::new(
        target.repo(),
        FetchRequest {
            remote_url: target.remote.clone(),
            refs: PrRefs::Pull,
            number: NUMBER,
            base_branch: "main".to_owned(),
            timeout: Duration::from_secs(300),
        },
    ))
}

fn log(step: worktrees::Step) {
    eprintln!("  {}", step.label());
}

/// The worktree `info` names, from a list.
fn listed<'a>(list: &'a [Worktree], info: &WorktreeInfo) -> Option<&'a Worktree> {
    list.iter().find(|entry| entry.is_at(&info.path))
}

/// The steps both sides take: ensure (twice, the second reusing it), list,
/// remove, list again.
fn exercise(
    ensure: &dyn Fn() -> Result<WorktreeInfo, String>,
    list: &dyn Fn() -> Result<Vec<Worktree>, String>,
    remove: &dyn Fn() -> Result<(), String>,
) -> Result<String, String> {
    let info = ensure()?;
    let entry = listed(&list()?, &info)
        .cloned()
        .ok_or_else(|| format!("{} is not listed after it was added", info.path))?;
    if entry.head.as_deref() != Some(info.head.as_str()) {
        return Err(format!(
            "listed at {:?}, expected {}",
            entry.head, info.head
        ));
    }
    let again = ensure()?;
    if again.path != info.path || again.head != info.head {
        return Err(format!("a second open moved it: {again:?}"));
    }
    remove()?;
    if listed(&list()?, &info).is_some() {
        return Err(format!("{} is still listed after removal", info.path));
    }
    Ok(format!(
        "created {} at {}, listed, reused, removed",
        info.path,
        info.short_head()
    ))
}

fn local() -> Result<String, String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/worktree-check");
    let _ = std::fs::remove_dir_all(&dir);
    let result = (|| {
        let store = repo::Store::open_or_create_bare(&dir.join("repos"), "review-sandbox")
            .map_err(|e| e.to_string())?;
        let source = source()?;
        let summary = exercise(
            &|| worktrees::ensure_local(&store, &source, &mut log),
            &|| worktrees::list_local(&store),
            &|| worktrees::remove_local(&store, NUMBER),
        )?;
        if store.worktree_path(NUMBER).exists() {
            return Err("the worktree's folder is still there".to_owned());
        }
        let head = store
            .resolve_commit(&repo::pr_head_ref(NUMBER))
            .map_err(|e| e.to_string())?;
        let check = match agent::check_store(&store, NUMBER, &head.to_string()) {
            ConfigCheck::Unchecked(error) => return Err(format!("agent config check: {error}")),
            ConfigCheck::Clean => "no agent config changed".to_owned(),
            ConfigCheck::Changed(changes) => {
                format!("{} agent config files changed", changes.len())
            }
        };
        Ok(format!("{summary}; {check}"))
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Runs `script` on `host` through the system `ssh`; for the checks and
/// the cleanup this example does beyond git.
fn ssh(host: &str, script: &str) -> Result<String, String> {
    let output = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=20",
            host,
            "--",
            script,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("ssh: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "`{script}` on {host}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn remote() -> Result<String, String> {
    let host = host()?;
    let machine: Machine = machines::list(&[], std::slice::from_ref(&host))
        .into_iter()
        .find(|machine| machine.name == host)
        .ok_or_else(|| format!("{host} is not a usable host name"))?;
    let source = source()?;
    let clone = machines::scratch_clone(&source.repo);
    let relative = clone
        .strip_prefix("~/")
        .ok_or("the scratch clone is not under the home directory")?
        .to_owned();
    let had_cache = ssh(&host, "test -d .cache/review && echo yes || echo no")? == "yes";
    let had_clone = worktrees::remote_clone(&machine, &source.repo, Placement::AppOwned)?
        .repo
        .exists()
        .map_err(|e| e.to_string())?;
    let result = exercise(
        &|| worktrees::ensure_remote(&machine, &source, Placement::AppOwned, &mut log),
        &|| worktrees::list_remote(&machine, &source.repo, Placement::AppOwned),
        &|| worktrees::remove_remote(&machine, &source.repo, Placement::AppOwned, NUMBER),
    );
    let worktrees_dir = format!("{relative}.worktrees");
    let cleanup = if !had_cache {
        ssh(&host, "rm -rf -- .cache/review")
    } else if !had_clone {
        ssh(
            &host,
            &format!(
                "rm -rf -- {} {}",
                repo::shell::quote(&relative),
                repo::shell::quote(&worktrees_dir)
            ),
        )
    } else {
        ssh(
            &host,
            &format!(
                "rmdir -- {} 2>/dev/null; true",
                repo::shell::quote(&worktrees_dir)
            ),
        )
    };
    let summary = result?;
    cleanup?;
    Ok(format!("{summary} (on {host}, scratch clone {clone})"))
}

fn main() -> ExitCode {
    let mut failed = false;
    for (name, check) in [("local", local as fn() -> _), ("remote", remote)] {
        eprintln!("{name}:");
        match check() {
            Ok(summary) => println!("PASS {name}: {summary}"),
            Err(error) => {
                failed = true;
                println!("FAIL {name}: {error}");
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
