//! Live CI check: on a forge's `review-sandbox`,
//! push a scratch branch that adds `ci/fail`, open a pull request, watch
//! the sandbox workflow (`build`, `test`, `slow`), tail `slow`'s log while
//! it runs, retry the failing `test` unchanged and see it fail again, then
//! push a commit that removes `ci/fail` and see the new run pass. Closes
//! the pull request and deletes the scratch branch at the end.
//!
//! `cargo run -p forge --example ci_live -- --forge github|gitlab|forgejo`
//!
//! Prints `PASS`, `FAIL` or `SKIP <step>: <capability>` per step and exits
//! non-zero on any FAIL. A run takes a few minutes (the `slow` job prints
//! for about a minute, and runs twice). With `REVIEW_RECORD=1` the forge's
//! HTTP is recorded, scrubbed and pruned, into `tests/fixtures/<forge>/ci.json`
//! for the replay tests.
//!
//! Safety: only repositories named `review-sandbox`,
//! never `main`, no force-push, and only its own branch (under `ci-live/`)
//! is deleted. Git runs in `.scratch/ci-live/<forge>`, a clone over SSH
//! that this tool owns.

#[path = "../sandbox/mod.rs"]
mod sandbox;
mod steps;

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use forge::model::Sha;
use sandbox::{SANDBOX, Sandbox, SandboxCli, git};

/// Every branch this tool creates, and the only ones it deletes, start so.
const BRANCH_PREFIX: &str = "ci-live/";
/// The file whose presence makes the sandbox's `test` job fail.
const FAIL_FILE: &str = "ci/fail";

/// Members of recorded CI responses no backend reads; leaving them out
/// keeps a few minutes of polling small and free of profile data.
const PRUNED: &[&str] = &[
    "repository",
    "head_repository",
    "head_commit",
    "event_payload",
    "actor",
    "triggering_actor",
    "trigger_user",
    "user",
    "pull_requests",
    "referenced_workflows",
    "commit",
    "runner",
    "artifacts_file",
    "artifacts",
    "project",
    "detailed_status",
];

fn head(dir: &Path) -> Result<Sha, String> {
    Ok(Sha::new(git(dir, &["rev-parse", "HEAD"])?.trim()))
}

/// A new branch from `origin/main` with one commit adding [`FAIL_FILE`],
/// pushed; returns the commit.
fn push_failing_branch(sandbox: &Sandbox, dir: &Path, branch: &str) -> Result<Sha, String> {
    sandbox::fresh_branch(sandbox, dir, branch)?;
    let path = dir.join(FAIL_FILE);
    std::fs::create_dir_all(path.parent().ok_or("no parent")?).map_err(|err| err.to_string())?;
    std::fs::write(&path, "Makes the sandbox's test job fail.\n").map_err(|err| err.to_string())?;
    git(dir, &["add", FAIL_FILE])?;
    git(dir, &["commit", "--quiet", "-m", "CI live: add ci/fail"])?;
    sandbox::push(dir, branch)?;
    head(dir)
}

/// A second commit removing [`FAIL_FILE`], pushed; returns it.
fn push_fix(dir: &Path, branch: &str) -> Result<Sha, String> {
    git(dir, &["rm", "--quiet", FAIL_FILE])?;
    git(dir, &["commit", "--quiet", "-m", "CI live: remove ci/fail"])?;
    sandbox::push(dir, branch)?;
    head(dir)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let sandbox = match sandbox::parse_args("ci_live") {
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
    let forge = match sandbox::backend(sandbox, "ci", PRUNED).await {
        Ok(forge) => forge,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            return ExitCode::FAILURE;
        }
    };

    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let branch = format!("{BRANCH_PREFIX}{}-{stamp}", sandbox.kind.as_str());
    let dir = sandbox::scratch_dir("ci-live", sandbox.kind);
    let cleanup_branch = |dir: &Path| sandbox::delete_branch(dir, &branch, BRANCH_PREFIX);
    let head = match push_failing_branch(sandbox, &dir, &branch) {
        Ok(head) => head,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            // The push may have landed before a later failure.
            let _ = cleanup_branch(&dir);
            return ExitCode::FAILURE;
        }
    };
    let title = format!("CI live {branch}");
    let body = "Scratch pull request of the live CI check; closed when it ends.";
    let number = match sandbox.open_pull_request(&dir, &branch, &title, body) {
        Ok(number) => number,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            if let Err(err) = cleanup_branch(&dir) {
                eprintln!("cleanup: {err}");
            }
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{} {repo} {}{number} from {branch} at {}",
        sandbox.kind,
        sandbox.kind.sigil(),
        head.short()
    );

    let (outcomes, _) = steps::run(
        forge.as_ref(),
        &repo,
        &head,
        steps::Pace::LIVE,
        || push_fix(&dir, &branch),
        |outcome| println!("{outcome}"),
    )
    .await;

    let mut cleanup_failed = false;
    if let Err(err) = sandbox.close_pull_request(&dir, number) {
        eprintln!("cleanup: {err}");
        cleanup_failed = true;
    }
    if let Err(err) = cleanup_branch(&dir) {
        eprintln!("cleanup: {err}");
        cleanup_failed = true;
    }
    let failed = outcomes.iter().filter(|o| o.failed()).count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o.status, steps::Status::Skip(_)))
        .count();
    println!(
        "ci_live {}: {} passed, {failed} failed, {skipped} skipped",
        sandbox.kind,
        outcomes.len() - failed - skipped
    );
    if failed == 0 && !cleanup_failed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
