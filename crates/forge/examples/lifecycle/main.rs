//! Live lifecycle round trip: on a forge's `review-sandbox`, open two scratch
//! pull requests, draft and ready the first and squash-merge it with its
//! expected head and `delete_branch`, then close, reopen and close the second.
//! Every state is read back through the same backend, and the merged branch
//! is checked with git.
//!
//! `cargo run -p forge --example lifecycle -- --forge github|gitlab|forgejo`
//!
//! Prints `PASS`, `FAIL` or `SKIP <step>: <why>` per step and exits non-zero
//! on any FAIL. With `REVIEW_RECORD=1` the forge's HTTP is recorded, scrubbed,
//! into `tests/fixtures/<forge>/lifecycle.json` for the replay test.
//!
//! Safety: it refuses any repository not named `review-sandbox`, never
//! force-pushes, and only touches branches it created (always under
//! `lifecycle/`) and files under `scratch/`. The merge changes the sandbox's
//! `main`: it adds one small file under `scratch/` per run. It never touches
//! the CI workflow or the seeded pull requests' branches. Git runs in
//! `.scratch/lifecycle/<forge>`, a clone over SSH that this tool owns.

#[path = "../sandbox/mod.rs"]
mod sandbox;
mod steps;

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge::Forge;
use forge::model::{PrNumber, RepoRef};
use sandbox::{SANDBOX, Sandbox, SandboxCli, git};

/// Every branch this tool creates, and the only ones it deletes, start so.
const BRANCH_PREFIX: &str = "lifecycle/";

/// Where the scratch change goes. Nothing else of the repository is edited.
const SCRATCH_DIR: &str = "scratch";

/// A new branch from `origin/main` with one new file under `scratch/`, pushed.
/// No force anywhere.
fn push_scratch_branch(
    sandbox: &Sandbox,
    dir: &Path,
    branch: &str,
    file: &str,
) -> Result<(), String> {
    sandbox::fresh_branch(sandbox, dir, branch)?;
    let relative = format!("{SCRATCH_DIR}/{file}");
    let path = dir.join(&relative);
    std::fs::create_dir_all(path.parent().ok_or("no parent directory")?)
        .map_err(|err| err.to_string())?;
    std::fs::write(
        &path,
        format!("Lifecycle round trip scratch file for {branch}.\n"),
    )
    .map_err(|err| err.to_string())?;
    git(dir, &["add", &relative])?;
    git(
        dir,
        &["commit", "--quiet", "-m", "Lifecycle: scratch change"],
    )?;
    sandbox::push(dir, branch)
}

/// Whether `branch` is on the remote.
fn remote_has(dir: &Path, branch: &str) -> Result<bool, String> {
    let output = git(dir, &["ls-remote", "--heads", "origin", branch])?;
    Ok(output
        .lines()
        .any(|line| line.ends_with(&format!("refs/heads/{branch}"))))
}

/// Opens a scratch branch and its pull request.
fn open_scratch(
    sandbox: &Sandbox,
    dir: &Path,
    branch: &str,
    file: &str,
    title: &str,
) -> Result<PrNumber, String> {
    if let Err(err) = push_scratch_branch(sandbox, dir, branch, file) {
        // The push may have landed before a later failure.
        let _ = delete_scratch_branch(dir, branch);
        return Err(err);
    }
    let body = "Scratch pull request of the lifecycle round trip.";
    sandbox
        .open_pull_request(dir, branch, title, body)
        .inspect_err(|_| {
            let _ = delete_scratch_branch(dir, branch);
        })
}

fn delete_scratch_branch(dir: &Path, branch: &str) -> Result<(), String> {
    sandbox::delete_branch(dir, branch, BRANCH_PREFIX)
}

/// Closes what is still open and deletes what is still there, through the
/// CLIs and git rather than the code under test. After a clean run both pull
/// requests are known to be finished, so the forge is asked only after a
/// failure (which also keeps these reads out of a recording).
async fn clean_up(
    sandbox: &Sandbox,
    forge: &dyn Forge,
    repo: &RepoRef,
    dir: &Path,
    scratch: &[(PrNumber, &str)],
    after_failure: bool,
) -> bool {
    let mut ok = true;
    for &(number, branch) in scratch {
        if after_failure {
            match forge.pull_request(repo, number).await {
                Ok(pr) if pr.state.is_open() => {
                    if let Err(err) = sandbox.close_pull_request(dir, number) {
                        eprintln!("cleanup: {err}");
                        ok = false;
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    eprintln!("cleanup: {err}");
                    ok = false;
                }
            }
        }
        match remote_has(dir, branch) {
            Ok(true) => {
                if let Err(err) = delete_scratch_branch(dir, branch) {
                    eprintln!("cleanup: {err}");
                    ok = false;
                } else {
                    println!("cleanup: deleted {branch}");
                }
            }
            Ok(false) => {}
            Err(err) => {
                eprintln!("cleanup: {err}");
                ok = false;
            }
        }
    }
    ok
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let sandbox = match sandbox::parse_args("lifecycle") {
        Ok(sandbox) => sandbox,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::FAILURE;
        }
    };
    let sandbox = &sandbox;
    let repo = sandbox.repo();
    if repo.name != SANDBOX {
        eprintln!("refusing {repo}: only repositories named {SANDBOX} are written to");
        return ExitCode::FAILURE;
    }
    let forge = match sandbox::backend(sandbox, "lifecycle", &[]).await {
        Ok(forge) => forge,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            return ExitCode::FAILURE;
        }
    };

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let kind = sandbox.kind.as_str();
    let merge_branch = format!("{BRANCH_PREFIX}{kind}-{stamp}-merge");
    let close_branch = format!("{BRANCH_PREFIX}{kind}-{stamp}-close");
    let dir = sandbox::scratch_dir("lifecycle", sandbox.kind);

    let merged = match open_scratch(
        sandbox,
        &dir,
        &merge_branch,
        &format!("lifecycle-{stamp}-merge.md"),
        &format!("Lifecycle {merge_branch}"),
    ) {
        Ok(number) => number,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            return ExitCode::FAILURE;
        }
    };
    let closed = match open_scratch(
        sandbox,
        &dir,
        &close_branch,
        &format!("lifecycle-{stamp}-close.md"),
        &format!("Lifecycle {close_branch}"),
    ) {
        Ok(number) => number,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            clean_up(
                sandbox,
                forge.as_ref(),
                &repo,
                &dir,
                &[(merged, &merge_branch)],
                true,
            )
            .await;
            return ExitCode::FAILURE;
        }
    };
    let sigil = sandbox.kind.sigil();
    println!(
        "{} {repo}: {sigil}{merged} from {merge_branch}, {sigil}{closed} from {close_branch}",
        sandbox.kind
    );

    let branch_exists = |branch: &str| remote_has(&dir, branch);
    let hooks = steps::Hooks {
        branch_exists: &branch_exists,
        settle: Duration::from_secs(1),
    };
    let outcomes = steps::run(
        forge.as_ref(),
        &repo,
        steps::Scratch { merged, closed },
        &hooks,
        |outcome| println!("{outcome}"),
    )
    .await;

    let failed = outcomes.iter().filter(|o| o.failed()).count();
    let clean = clean_up(
        sandbox,
        forge.as_ref(),
        &repo,
        &dir,
        &[(merged, &merge_branch), (closed, &close_branch)],
        failed > 0,
    )
    .await;
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o.status, steps::Status::Skip(_)))
        .count();
    println!(
        "lifecycle {}: {} passed, {failed} failed, {skipped} skipped",
        sandbox.kind,
        outcomes.len() - failed - skipped
    );
    if failed == 0 && clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
