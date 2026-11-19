//! Inbox oracle for GitLab: each section's merge request set from the
//! backend against `glab api` for the same query. Live and read-only.
//!
//! `cargo run -p forge --example inbox_check_gitlab [-- group/project]`
//!
//! Prints the differences per section and exits 1 if there is any. The
//! project defaults to the sandbox named by `REVIEW_SANDBOX_GITLAB` (see
//! `docs/sandboxes.md`).

use std::collections::BTreeSet;
use std::process::{Command, ExitCode};
use std::sync::Arc;

use forge::auth::CliCredentials;
use forge::gitlab::GitLab;
use forge::http::{HttpClient, HttpOptions, ReqwestTransport};
use forge::model::{ForgeKind, InboxSection, PullRequest, RepoRef};
use forge::sandbox::Target;
use forge::{Forge, INBOX_LIMIT};
use serde_json::Value;

const HOST: &str = "gitlab.com";

/// `path!iid` of every merge request the REST list `endpoint` returns, the
/// first `INBOX_LIMIT` in the order GitLab gives them.
fn glab_set(endpoint: &str) -> Result<BTreeSet<String>, String> {
    let url = format!("{endpoint}&per_page={INBOX_LIMIT}&order_by=updated_at&sort=desc");
    let output = Command::new("glab")
        .args(["api", "--hostname", HOST, &url])
        .output()
        .map_err(|err| format!("cannot run glab: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "glab api {endpoint}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let rows: Vec<Value> = serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("glab api {endpoint}: {err}"))?;
    Ok(rows
        .iter()
        .filter_map(|row| row.pointer("/references/full")?.as_str().map(str::to_owned))
        .collect())
}

fn ours(prs: &[PullRequest]) -> BTreeSet<String> {
    prs.iter()
        .map(|pr| format!("{}!{}", pr.repo.path(), pr.number))
        .collect()
}

/// Prints the section's result; true when the two sets are equal.
fn compare(label: &str, ours: &BTreeSet<String>, theirs: &BTreeSet<String>) -> bool {
    let only_ours: Vec<&String> = ours.difference(theirs).collect();
    let only_theirs: Vec<&String> = theirs.difference(ours).collect();
    let differences = only_ours.len() + only_theirs.len();
    println!(
        "{label}: backend {}, glab {}, {differences} differences",
        ours.len(),
        theirs.len()
    );
    for item in only_ours {
        println!("  only in the backend: {item}");
    }
    for item in only_theirs {
        println!("  only in glab: {item}");
    }
    differences == 0
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("inbox_check_gitlab: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<bool, String> {
    let project = match std::env::args().nth(1) {
        Some(project) => project,
        None => Target::from_env(ForgeKind::GitLab)?.slug(),
    };
    let (owner, name) = project
        .rsplit_once('/')
        .ok_or("the project must be group/name")?;
    let repo = RepoRef::new(HOST, owner, name);

    let transport = ReqwestTransport::new().map_err(|err| err.to_string())?;
    let credentials = CliCredentials {
        kind: ForgeKind::GitLab,
        host: HOST.to_owned(),
    };
    let http = HttpClient::new(
        ForgeKind::GitLab,
        HOST,
        Arc::new(transport),
        Arc::new(credentials),
        HttpOptions::for_kind(ForgeKind::GitLab),
    );
    let gitlab = GitLab::connect(http).await.map_err(|err| err.to_string())?;
    let me = gitlab.whoami().await.map_err(|err| err.to_string())?.login;
    println!("signed in as {me}");

    let encoded = repo.path().replace('/', "%2F");
    let free_query = format!("scope=all&state=opened&author_username={me}");
    let sections = [
        (
            "review requested",
            InboxSection::ReviewRequested,
            format!("merge_requests?scope=all&state=opened&reviewer_username={me}"),
        ),
        (
            "authored by me",
            InboxSection::AuthoredByMe,
            format!("merge_requests?scope=all&state=opened&author_username={me}"),
        ),
        (
            "open in project",
            InboxSection::OpenInRepo(repo.clone()),
            format!("projects/{encoded}/merge_requests?state=opened"),
        ),
        (
            "free query",
            InboxSection::Query(free_query.clone()),
            format!("merge_requests?{free_query}"),
        ),
    ];

    let mut clean = true;
    for (label, section, endpoint) in sections {
        let found = gitlab
            .inbox(&section)
            .await
            .map_err(|err| err.to_string())?;
        let expected = glab_set(&endpoint)?;
        clean &= compare(label, &ours(&found), &expected);
    }
    println!(
        "{}",
        if clean {
            "0 differences"
        } else {
            "DIFFERENCES FOUND"
        }
    );
    Ok(clean)
}
