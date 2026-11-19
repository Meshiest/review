//! Live people round trip: on a forge's `review-sandbox`, open one scratch
//! pull request, put a label on it and take it off, assign yourself and
//! unassign, ask yourself for a review where the forge allows it, and read
//! every state back through the same backend. Then close the pull request
//! and delete its branch.
//!
//! `cargo run -p forge --example people -- --forge github|gitlab|forgejo`
//!
//! Prints `PASS`, `FAIL` or `SKIP <step>: <why>` per step and exits non-zero
//! on any FAIL. With `REVIEW_RECORD=1` the forge's HTTP is recorded,
//! scrubbed, into `tests/fixtures/<forge>/people.json` for the replay test.
//!
//! Safety: it refuses any repository not named `review-sandbox`, never
//! force-pushes, and only touches the branch it created (under `people/`) and
//! one file under `scratch/`. If the sandbox has no label named `review-test`
//! it creates one first (through the forge's CLI) and says so; nothing else
//! of the repository's labels is touched. Git runs in
//! `.scratch/people/<forge>`, a clone over SSH that this tool owns.

#[path = "../sandbox/mod.rs"]
mod sandbox;
mod steps;

use std::path::Path;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use forge::model::ForgeKind;
use sandbox::{SANDBOX, Sandbox, SandboxCli, git, run};

/// Every branch this tool creates, and the only ones it deletes, start so.
const BRANCH_PREFIX: &str = "people/";

/// Where the scratch change goes. Nothing else of the repository is edited.
const SCRATCH_DIR: &str = "scratch";

const LABEL_COLOR: &str = "1d76db";
const LABEL_DESCRIPTION: &str = "Put on a scratch pull request by the people round trip";

/// Makes sure the sandbox has [`steps::LABEL`]; returns whether it created it.
fn ensure_label(sandbox: &Sandbox) -> Result<bool, String> {
    let slug = format!("{}/{}", sandbox.owner, sandbox.name);
    let label = steps::LABEL;
    let present = match sandbox.kind {
        ForgeKind::GitHub => run(
            "gh",
            &[
                "label", "list", "-R", &slug, "--limit", "200", "--json", "name",
            ],
            None,
        )?
        .contains(&format!("\"name\":\"{label}\"")),
        ForgeKind::GitLab => run(
            "glab",
            &[
                "api",
                &format!("projects/{}/labels", slug.replace('/', "%2F")),
            ],
            None,
        )?
        .contains(&format!("\"name\":\"{label}\"")),
        ForgeKind::Forgejo => run(
            "fj",
            &["-H", &sandbox.host, "repo", "labels", &slug, "view"],
            None,
        )?
        .contains(label),
    };
    if present {
        return Ok(false);
    }
    match sandbox.kind {
        ForgeKind::GitHub => run(
            "gh",
            &[
                "label",
                "create",
                label,
                "-R",
                &slug,
                "--color",
                LABEL_COLOR,
                "--description",
                LABEL_DESCRIPTION,
            ],
            None,
        ),
        ForgeKind::GitLab => run(
            "glab",
            &[
                "api",
                "-X",
                "POST",
                &format!("projects/{}/labels", slug.replace('/', "%2F")),
                "-f",
                &format!("name={label}"),
                "-f",
                &format!("color=#{LABEL_COLOR}"),
                "-f",
                &format!("description={LABEL_DESCRIPTION}"),
            ],
            None,
        ),
        ForgeKind::Forgejo => run(
            "fj",
            &[
                "-H",
                &sandbox.host,
                "repo",
                "labels",
                &slug,
                "create",
                label,
                LABEL_COLOR,
                "--description",
                LABEL_DESCRIPTION,
            ],
            None,
        ),
    }?;
    Ok(true)
}

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
        format!("People round trip scratch file for {branch}.\n"),
    )
    .map_err(|err| err.to_string())?;
    git(dir, &["add", &relative])?;
    git(dir, &["commit", "--quiet", "-m", "People: scratch change"])?;
    sandbox::push(dir, branch)
}

fn remote_has(dir: &Path, branch: &str) -> Result<bool, String> {
    let output = git(dir, &["ls-remote", "--heads", "origin", branch])?;
    Ok(output
        .lines()
        .any(|line| line.ends_with(&format!("refs/heads/{branch}"))))
}

fn delete_scratch_branch(dir: &Path, branch: &str) -> Result<(), String> {
    sandbox::delete_branch(dir, branch, BRANCH_PREFIX)
}

/// Closes the pull request and deletes the branch, through the CLIs and git
/// rather than the code under test.
fn clean_up(sandbox: &Sandbox, dir: &Path, number: forge::model::PrNumber, branch: &str) -> bool {
    let mut ok = true;
    if let Err(err) = sandbox.close_pull_request(dir, number) {
        eprintln!("cleanup: {err}");
        ok = false;
    }
    match remote_has(dir, branch) {
        Ok(true) => match delete_scratch_branch(dir, branch) {
            Ok(()) => println!("cleanup: deleted {branch}"),
            Err(err) => {
                eprintln!("cleanup: {err}");
                ok = false;
            }
        },
        Ok(false) => {}
        Err(err) => {
            eprintln!("cleanup: {err}");
            ok = false;
        }
    }
    ok
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let sandbox = match sandbox::parse_args("people") {
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
    match ensure_label(sandbox) {
        Ok(true) => println!(
            "created the label {} in {repo} (color #{LABEL_COLOR})",
            steps::LABEL
        ),
        Ok(false) => println!("{repo} already has the label {}", steps::LABEL),
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            return ExitCode::FAILURE;
        }
    }
    let forge = match sandbox::backend(sandbox, "people", &[]).await {
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
    let dir = sandbox::scratch_dir("people", sandbox.kind);
    if let Err(err) = push_scratch_branch(sandbox, &dir, &branch, &format!("people-{stamp}.md")) {
        eprintln!("FAIL setup: {err}");
        let _ = delete_scratch_branch(&dir, &branch);
        return ExitCode::FAILURE;
    }
    let body = "Scratch pull request of the people round trip.";
    let number = match sandbox.open_pull_request(&dir, &branch, &format!("People {branch}"), body) {
        Ok(number) => number,
        Err(err) => {
            eprintln!("FAIL setup: {err}");
            let _ = delete_scratch_branch(&dir, &branch);
            return ExitCode::FAILURE;
        }
    };
    println!(
        "{} {repo}: {}{number} from {branch}",
        sandbox.kind,
        sandbox.kind.sigil()
    );

    let outcomes = steps::run(forge.as_ref(), &repo, number, |outcome| {
        println!("{outcome}");
    })
    .await;

    let failed = outcomes.iter().filter(|o| o.failed()).count();
    let skipped = outcomes
        .iter()
        .filter(|o| matches!(o.status, steps::Status::Skip(_)))
        .count();
    let clean = clean_up(sandbox, &dir, number, &branch);
    println!(
        "people {}: {} passed, {failed} failed, {skipped} skipped",
        sandbox.kind,
        outcomes.len() - failed - skipped
    );
    if failed == 0 && clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
