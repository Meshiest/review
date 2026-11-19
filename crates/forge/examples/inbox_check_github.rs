//! Inbox oracle for GitHub: each inbox section against `gh search prs`
//! for the same query, live and read-only. Prints the differences and exits
//! non-zero if there are any.
//!
//! `cargo run -p forge --example inbox_check_github`; the sandbox comes from
//! `REVIEW_SANDBOX_GITHUB` (see `docs/sandboxes.md`).

use std::collections::BTreeSet;
use std::process::ExitCode;
use std::sync::Arc;

use forge::auth::CliCredentials;
use forge::github::GitHub;
use forge::http::{HttpClient, HttpOptions, ReqwestTransport};
use forge::model::{ForgeKind, InboxSection};
use forge::sandbox::Target;
use forge::{Forge, INBOX_LIMIT};
use tokio::process::Command;

const HOST: &str = "github.com";

/// `owner/name#number`.
type Key = String;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(0) => ExitCode::SUCCESS,
        Ok(differences) => {
            eprintln!("{differences} difference(s)");
            ExitCode::FAILURE
        }
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<usize, String> {
    let kind = ForgeKind::GitHub;
    let transport = Arc::new(ReqwestTransport::new().map_err(|e| e.to_string())?);
    let credentials = Arc::new(CliCredentials {
        kind,
        host: HOST.to_owned(),
    });
    let http = HttpClient::new(
        kind,
        HOST,
        transport,
        credentials,
        HttpOptions::for_kind(kind),
    );
    let github = GitHub::connect(http).await.map_err(|e| e.to_string())?;

    let target = Target::from_env(kind)?;
    let slug = target.slug();
    let sandbox = target.repo();
    let sections: Vec<(InboxSection, Vec<&str>)> = vec![
        (
            InboxSection::ReviewRequested,
            vec!["--review-requested", "@me", "--state", "open"],
        ),
        (
            InboxSection::AuthoredByMe,
            vec!["--author", "@me", "--state", "open"],
        ),
        (
            InboxSection::OpenInRepo(sandbox.clone()),
            vec!["--repo", &slug, "--state", "open"],
        ),
        (
            InboxSection::Query(format!("repo:{slug} is:closed")),
            vec!["--repo", &slug, "--state", "closed"],
        ),
    ];

    let mut differences = 0;
    for (section, flags) in sections {
        let ours: BTreeSet<Key> = github
            .inbox(&section)
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .map(|pr| format!("{}#{}", pr.repo.path(), pr.number))
            .collect();
        let theirs = gh_search(&flags).await?;
        let only_ours: Vec<&Key> = ours.difference(&theirs).collect();
        let only_gh: Vec<&Key> = theirs.difference(&ours).collect();
        println!(
            "{section:?}: ours {}, gh {}, only ours {}, only gh {}",
            ours.len(),
            theirs.len(),
            only_ours.len(),
            only_gh.len()
        );
        for key in &only_ours {
            println!("  only in our result: {key}");
        }
        for key in &only_gh {
            println!("  only in gh search: {key}");
        }
        differences += only_ours.len() + only_gh.len();
    }
    Ok(differences)
}

async fn gh_search(flags: &[&str]) -> Result<BTreeSet<Key>, String> {
    let limit = INBOX_LIMIT.to_string();
    let output = Command::new("gh")
        .args([
            "search", "prs", "--limit", &limit, "--sort", "updated", "--order", "desc",
        ])
        .args(["--json", "number,repository"])
        .args(flags)
        .output()
        .await
        .map_err(|e| format!("cannot run gh: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh search prs failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<serde_json::Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("gh output: {e}"))?;
    rows.iter()
        .map(|row| {
            let repo = row["repository"]["nameWithOwner"].as_str();
            let number = row["number"].as_u64();
            match (repo, number) {
                (Some(repo), Some(number)) => Ok(format!("{repo}#{number}")),
                _ => Err(format!("unexpected gh row: {row}")),
            }
        })
        .collect()
}
