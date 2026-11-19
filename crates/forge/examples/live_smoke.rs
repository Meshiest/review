//! Live read smoke: through the real `ForgeHub`, read `review-sandbox`
//! PR 4 (the threads PR) on each forge and check what `docs/sandboxes.md`
//! says it holds. Read-only. Prints one line per forge and exits non-zero
//! if any forge fails.
//!
//! `cargo run -p forge --example live_smoke`; the sandboxes come from the
//! `REVIEW_SANDBOX_*` variables (see `docs/sandboxes.md`).

use std::process::ExitCode;

use forge::model::{ForgeKind, PrNumber, Snapshot};
use forge::sandbox::Target;
use forge::{ForgeConfig, ForgeHub};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut targets = Vec::new();
    for kind in [ForgeKind::GitHub, ForgeKind::GitLab, ForgeKind::Forgejo] {
        match Target::from_env(kind) {
            Ok(target) => targets.push(target),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    let configs = targets
        .iter()
        .map(|target| ForgeConfig {
            kind: target.kind,
            host: target.host.clone(),
        })
        .collect();
    let hub = match ForgeHub::start(configs) {
        Ok(hub) => hub,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut failures = 0;
    for target in targets {
        let kind = target.kind;
        let repo = target.repo();
        let pending = hub.spawn(&target.host, move |forge| async move {
            forge.snapshot(&repo, PrNumber(4)).await
        });
        let verdict = match pending.await {
            Ok(snapshot) => check(&snapshot),
            Err(err) => Err(err.to_string()),
        };
        match verdict {
            Ok(summary) => println!("PASS  {kind:?}  {summary}"),
            Err(problem) => {
                failures += 1;
                println!("FAIL  {kind:?}  {problem}");
            }
        }
    }
    println!("live_smoke: {} of 3 passed", 3 - failures);
    if failures == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// PR 4 is titled "Threads: ..." and has at least 3 inline threads.
fn check(snapshot: &Snapshot) -> Result<String, String> {
    let title = &snapshot.pull_request.title;
    if !title.starts_with("Threads") {
        return Err(format!("unexpected title {title:?}"));
    }
    let anchored = snapshot
        .threads
        .iter()
        .filter(|t| t.anchor.is_some())
        .count();
    if anchored < 3 {
        return Err(format!(
            "expected at least 3 inline threads, got {anchored}"
        ));
    }
    Ok(format!(
        "{title:?}, {anchored} inline threads, {} checks",
        snapshot.checks.len()
    ))
}
