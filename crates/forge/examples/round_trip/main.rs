//! Live round trip of the review writes: on a forge's `review-sandbox`,
//! push a scratch branch with one small change, open a pull request from
//! it, run every review write and read each back through the same backend,
//! then close the pull request and delete the scratch branch.
//!
//! `cargo run -p forge --example round_trip -- --forge github|gitlab|forgejo`
//!
//! Prints `PASS`, `FAIL` or `SKIP <step>: <capability>` per step and exits
//! non-zero on any FAIL. With `REVIEW_RECORD=1` the forge's HTTP is recorded,
//! scrubbed, into `tests/fixtures/<forge>/writes.json` for the replay test.
//!
//! Safety: it refuses any repository not named
//! `review-sandbox`, never force-pushes, and deletes only the branch it
//! created (always under `round-trip/`). Git runs in
//! `.scratch/round-trip/<forge>`, a clone over SSH that this tool owns.

#[path = "../sandbox/mod.rs"]
mod sandbox;
mod steps;

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use sandbox::{SANDBOX, Sandbox, SandboxCli, git};

/// Every branch this tool creates, and the only ones it deletes, start so.
const BRANCH_PREFIX: &str = "round-trip/";

/// Commits the scratch change on a new branch from `origin/main` and pushes
/// it. No force anywhere.
fn push_scratch_branch(sandbox: &Sandbox, dir: &Path, branch: &str) -> Result<(), String> {
    sandbox::fresh_branch(sandbox, dir, branch)?;
    let path = dir.join(steps::TARGET);
    let original = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
    let changed = scratch_change(&original)
        .ok_or_else(|| format!("{} has fewer than {} lines", steps::TARGET, steps::REPLACED))?;
    std::fs::write(&path, changed).map_err(|err| err.to_string())?;
    git(
        dir,
        &["commit", "--quiet", "-am", "Round trip: scratch change"],
    )?;
    sandbox::push(dir, branch)
}

/// Line [`steps::REPLACED`] replaced by [`steps::ADDED`].
fn scratch_change(original: &str) -> Option<String> {
    let index = usize::try_from(steps::REPLACED).ok()? - 1;
    let mut lines: Vec<&str> = original.lines().collect();
    if index >= lines.len() {
        return None;
    }
    lines.splice(index..=index, steps::ADDED);
    let mut text = lines.join("\n");
    if original.ends_with('\n') {
        text.push('\n');
    }
    Some(text)
}

fn delete_scratch_branch(dir: &Path, branch: &str) -> Result<(), String> {
    sandbox::delete_branch(dir, branch, BRANCH_PREFIX)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let sandbox = match sandbox::parse_args("round_trip") {
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
    let forge = match sandbox::backend(sandbox, "writes", &[]).await {
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
    let dir = sandbox::scratch_dir("round-trip", sandbox.kind);
    if let Err(err) = push_scratch_branch(sandbox, &dir, &branch) {
        eprintln!("FAIL setup: {err}");
        // The push may have landed before a later failure.
        let _ = delete_scratch_branch(&dir, &branch);
        return ExitCode::FAILURE;
    }
    let title = format!("Round trip {branch}");
    let body = "Scratch pull request of the live round trip; closed when it ends.";
    let number = match sandbox.open_pull_request(&dir, &branch, &title, body) {
        Ok(number) => number,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            if let Err(err) = delete_scratch_branch(&dir, &branch) {
                eprintln!("cleanup: {err}");
            }
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{} {repo} {}{number} from {branch}",
        sandbox.kind,
        sandbox.kind.sigil()
    );

    let outcomes = steps::run(forge.as_ref(), &repo, number, |outcome| {
        println!("{outcome}");
    })
    .await;

    let mut cleanup_failed = false;
    if let Err(err) = sandbox.close_pull_request(&dir, number) {
        eprintln!("cleanup: {err}");
        cleanup_failed = true;
    }
    if let Err(err) = delete_scratch_branch(&dir, &branch) {
        eprintln!("cleanup: {err}");
        cleanup_failed = true;
    }
    let failed = outcomes.iter().filter(|o| o.failed()).count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o.status, steps::Status::Skip(_)))
        .count();
    println!(
        "round_trip {}: {} passed, {failed} failed, {skipped} skipped",
        sandbox.kind,
        outcomes.len() - failed - skipped
    );
    if failed == 0 && !cleanup_failed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_change_replaces_one_line_with_three() {
        let changed = scratch_change("# Intro\n\nold\n\n- a\n").unwrap();
        assert_eq!(
            changed,
            "# Intro\n\nRound trip line one.\nRound trip line two.\nRound trip line three.\n\n- a\n"
        );
        assert_eq!(scratch_change("one\n"), None);
    }
}
