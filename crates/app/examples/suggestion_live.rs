//! Live check of applying suggestions on GitHub, where it is a commit the
//! app builds from git objects and pushes:
//!
//! 1. push a scratch branch to the GitHub sandbox (`REVIEW_SANDBOX_GITHUB`,
//!    see `docs/sandboxes.md`) and open a pull request from it;
//! 2. leave a review with two suggestion comments on it;
//! 3. apply both as one batch through the app's own code: the backend's
//!    `head_writable`, `suggestions::applicable`, then
//!    `suggestions::apply_locally` (commit from objects, fast-forward push);
//! 4. read the branch back: one new commit on the old head, the file as the
//!    suggestions say;
//! 5. close the pull request and delete the scratch branch.
//!
//! `cargo run -p app --example suggestion_live`
//!
//! It refuses any repository not named `review-sandbox`, never forces a
//! push, and deletes only the branch it created (under `round-trip/`).

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use app::config::Config;
use app::services::{Services, Spawn};
use app::suggestions::{self, Apply};
use forge::model::{
    CommentAnchor, ForgeKind, LineRef, NewComment, PendingPlace, PrNumber, RepoRef, Side, Snapshot,
    Verdict,
};
use forge::sandbox::Target;
use forge::{ForgeConfig, ForgeHub};
use futures::FutureExt as _;

static SANDBOX: OnceLock<Target> = OnceLock::new();

/// The sandbox `main` read from the environment.
fn sandbox() -> &'static Target {
    SANDBOX.get().expect("main sets the sandbox first")
}
const BRANCH_PREFIX: &str = "round-trip/";
const TARGET: &str = "docs/intro.md";
/// The scratch change: line 6 of the file on `main`.
const CHANGED_LINE: u32 = 6;
const CHANGED_TEXT: &str = "- second point, changed";
/// Two suggestions on the context lines around it.
const FIRST: (u32, &str) = (5, "- first point, suggested");
const SECOND: (u32, [&str; 2]) = (7, ["- third point, suggested", "- fourth point"]);

fn main() -> ExitCode {
    let target = match Target::from_env(ForgeKind::GitHub) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let repo = target.repo();
    let _ = SANDBOX.set(target);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let branch = format!("{BRANCH_PREFIX}suggest-{stamp}");
    let dir = scratch().join("clone");
    if let Err(error) = push_scratch_branch(&dir, &branch) {
        println!("FAIL setup: {error}");
        return ExitCode::FAILURE;
    }
    let number = match open_pull_request(&branch) {
        Ok(number) => number,
        Err(error) => {
            println!("FAIL open: {error}");
            let _ = delete_branch(&dir, &branch);
            return ExitCode::FAILURE;
        }
    };
    println!("opened {repo}#{number} from {branch}");
    let result = futures::executor::block_on(check(&repo, number, &dir, &branch));
    let closed = close_pull_request(number);
    let deleted = delete_branch(&dir, &branch);
    let mut failed = false;
    match result {
        Ok(detail) => println!("PASS apply: {detail}"),
        Err(error) => {
            println!("FAIL apply: {error}");
            failed = true;
        }
    }
    for (step, outcome) in [("close", closed), ("delete branch", deleted)] {
        match outcome {
            Ok(()) => println!("PASS {step}"),
            Err(error) => {
                println!("FAIL {step}: {error}");
                failed = true;
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

async fn check(
    repo: &RepoRef,
    number: PrNumber,
    dir: &Path,
    branch: &str,
) -> Result<String, String> {
    let hub = Arc::new(
        ForgeHub::start(vec![ForgeConfig {
            kind: ForgeKind::GitHub,
            host: repo.host.clone(),
        }])
        .map_err(|error| error.to_string())?,
    );
    let data = scratch().join("data");
    let config = Config::parse(&format!(
        "data_dir = \"{}\"\n\n[[forges]]\nkind = \"github\"\nhost = \"github.com\"\n",
        data.display().to_string().replace('\\', "/")
    ))
    .map_err(|error| error.to_string())?;
    let spawn: Spawn = Arc::new(|future| {
        std::thread::spawn(move || futures::executor::block_on(future));
    });
    let services = Services::new(Arc::new(config), None, Some(hub.clone()), spawn);

    let snapshot = read(&services, repo, number).await?;
    let old_head = snapshot.pull_request.head.sha.clone();
    // A review with the two suggestions, submitted as Comment.
    let (sandbox, head) = (repo.clone(), old_head.clone());
    services
        .forge(&repo.host, move |forge| async move {
            let review = forge.start_review(&sandbox, number, &head).await?.review.id;
            for (line, body) in [
                (FIRST.0, suggestion(&[FIRST.1])),
                (SECOND.0, suggestion(&SECOND.1)),
            ] {
                let comment = NewComment {
                    place: PendingPlace::New(CommentAnchor::Line {
                        path: TARGET.to_owned(),
                        line: LineRef {
                            side: Side::New,
                            line,
                        },
                        start: None,
                    }),
                    body,
                };
                forge
                    .add_review_comment(&sandbox, number, &review, &comment)
                    .await?;
            }
            forge
                .submit_review(&sandbox, number, &review, Verdict::Comment, "")
                .await
                .map(drop)
        })
        .await
        .map_err(|error| format!("review: {error}"))?;

    // The objects of the head, as opening the pull request fetches them.
    let objects = {
        let (objects, repo) = (services.objects.clone(), repo.clone());
        services
            .run_blocking(move || objects.resolve(&repo))
            .await
            .ok_or("the object store did not open")??
    };
    services
        .fetches
        .fetch(&objects, services.fetch_request(repo, number, "main"), None)
        .await?;

    let snapshot = read(&services, repo, number).await?;
    let pr = &snapshot.pull_request;
    if !pr.head_writable {
        return Err("the forge says the head branch is not writable".to_owned());
    }
    let caps = forge::Capabilities::GITHUB;
    let mut edits = Vec::new();
    for thread in snapshot.threads.iter().filter(|t| t.anchor.is_some()) {
        for comment in &thread.comments {
            match suggestions::applicable(thread, comment, pr, &caps) {
                Some(Apply::Local(edit)) => edits.push(edit),
                Some(Apply::Forge(_)) => return Err("GitHub cannot apply on the forge".to_owned()),
                None => {}
            }
        }
    }
    if edits.len() != 2 {
        return Err(format!(
            "expected 2 applicable suggestions, found {}",
            edits.len()
        ));
    }
    let commit = suggestions::apply_locally(&services, pr, edits)
        .await?
        .to_string();

    // Read the branch back from the remote.
    git(dir, &["fetch", "--quiet", "origin", branch])?;
    let remote_head = git(dir, &["rev-parse", "FETCH_HEAD"])?.trim().to_owned();
    if remote_head != commit {
        return Err(format!(
            "the branch is at {remote_head}, not the pushed {commit}"
        ));
    }
    let parent = git(dir, &["rev-parse", "FETCH_HEAD^"])?.trim().to_owned();
    if parent != old_head.as_str() {
        return Err(format!(
            "the commit's parent is {parent}, not the old head {old_head}"
        ));
    }
    let file = git(dir, &["show", &format!("FETCH_HEAD:{TARGET}")])?;
    let expected = expected_file(&git(dir, &["show", &format!("origin/main:{TARGET}")])?);
    if file.trim_end() != expected {
        return Err(format!("{TARGET} reads\n{file}\nexpected\n{expected}"));
    }
    let message = git(dir, &["log", "-1", "--format=%s", "FETCH_HEAD"])?;
    Ok(format!(
        "one commit {} on {}, \"{}\", {TARGET} as suggested",
        &commit[..7],
        old_head.short(),
        message.trim()
    ))
}

async fn read(services: &Services, repo: &RepoRef, number: PrNumber) -> Result<Snapshot, String> {
    services.forget_snapshot(&store::PrKey::new(repo.clone(), number));
    let (repo, wait) = (repo.clone(), Duration::from_secs(2));
    // GitHub needs a moment after a push before the head moves.
    futures_timer(wait).await;
    let call = services.forge(&repo.host.clone(), move |forge| async move {
        forge.snapshot(&repo, number).await
    });
    call.await.map_err(|error| error.to_string())
}

fn futures_timer(wait: Duration) -> impl Future<Output = ()> {
    let (tx, rx) = futures::channel::oneshot::channel::<()>();
    std::thread::spawn(move || {
        std::thread::sleep(wait);
        let _ = tx.send(());
    });
    rx.map(drop)
}

fn suggestion(lines: &[&str]) -> String {
    format!(
        "Suggestion check:\n\n```suggestion\n{}\n```",
        lines.join("\n")
    )
}

fn expected_file(main: &str) -> String {
    let mut lines: Vec<String> = main.lines().map(str::to_owned).collect();
    lines[CHANGED_LINE as usize - 1] = CHANGED_TEXT.to_owned();
    lines[FIRST.0 as usize - 1] = FIRST.1.to_owned();
    let at = SECOND.0 as usize - 1;
    lines.splice(at..=at, SECOND.1.iter().map(|&l| l.to_owned()));
    lines.join("\n")
}

fn scratch() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.scratch/suggestion-live")
}

fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("`{program}` did not start: {error}"))?;
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "`{program} {}` failed: {}{}",
            args.join(" "),
            text.trim(),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut full = vec!["-C", dir.to_str().ok_or("scratch path is not UTF-8")?];
    full.extend_from_slice(args);
    run("git", &full)
}

/// Clones (or fetches) the sandbox, commits the scratch change on a new
/// branch from `origin/main` and pushes it. No force anywhere.
fn push_scratch_branch(dir: &Path, branch: &str) -> Result<(), String> {
    if dir.join(".git").is_dir() {
        git(dir, &["fetch", "--quiet", "--prune", "origin"])?;
    } else {
        std::fs::create_dir_all(dir.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
        run(
            "git",
            &[
                "clone",
                "--quiet",
                &sandbox().remote,
                dir.to_str().ok_or("scratch path is not UTF-8")?,
            ],
        )?;
    }
    // Only this tool's own clone is reset.
    git(
        dir,
        &[
            "checkout",
            "--quiet",
            "--force",
            "-B",
            branch,
            "origin/main",
        ],
    )?;
    let path = dir.join(TARGET);
    let text = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
    let mut lines: Vec<&str> = text.lines().collect();
    let at = CHANGED_LINE as usize - 1;
    if lines.len() < SECOND.0 as usize {
        return Err(format!("{TARGET} is shorter than expected"));
    }
    lines[at] = CHANGED_TEXT;
    let mut changed = lines.join("\n");
    if text.ends_with('\n') {
        changed.push('\n');
    }
    std::fs::write(&path, changed).map_err(|e| e.to_string())?;
    git(
        dir,
        &[
            "commit",
            "--quiet",
            "-am",
            "Suggestion check: scratch change",
        ],
    )?;
    git(
        dir,
        &["push", "--quiet", "origin", &format!("{branch}:{branch}")],
    )?;
    Ok(())
}

fn open_pull_request(branch: &str) -> Result<PrNumber, String> {
    let slug = sandbox().slug();
    let output = run(
        "gh",
        &[
            "pr",
            "create",
            "-R",
            &slug,
            "--head",
            branch,
            "--base",
            "main",
            "--title",
            &format!("Suggestion check {branch}"),
            "--body",
            "Scratch pull request of the suggestion check; closed when it ends.",
        ],
    )?;
    output
        .trim()
        .rsplit('/')
        .next()
        .and_then(|n| n.parse().ok())
        .map(PrNumber)
        .ok_or_else(|| format!("no pull request number in {output:?}"))
}

fn close_pull_request(number: PrNumber) -> Result<(), String> {
    let slug = sandbox().slug();
    run("gh", &["pr", "close", &number.to_string(), "-R", &slug]).map(drop)
}

fn delete_branch(dir: &Path, branch: &str) -> Result<(), String> {
    if !branch.starts_with(BRANCH_PREFIX) {
        return Err(format!("refusing to delete {branch}: not a scratch branch"));
    }
    git(dir, &["push", "--quiet", "origin", "--delete", branch]).map(drop)
}
