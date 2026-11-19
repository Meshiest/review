//! Compares each inbox section of the Forgejo backend with the forge's own
//! search, read-only, against the live instance with `fj`'s token.
//!
//! ```sh
//! cargo run -p forge --example inbox_check_forgejo [host] [owner/repo]
//! ```
//!
//! Without arguments it uses `REVIEW_SANDBOX_FORGEJO` (see `docs/sandboxes.md`).
//!
//! - Review requested and authored by me: the issue search API with the same
//!   parameters, paged on its own (until a page comes back empty, not by
//!   `X-Total-Count`).
//! - Open in a repo: `fj pr search -r <repo> -s open`.
//! - A query (`state=closed`): the issue search API again.
//!
//! Prints every difference and exits non-zero if there is one.

use std::collections::BTreeSet;
use std::process::{Command, ExitCode};
use std::sync::Arc;

use forge::auth::CliCredentials;
use forge::forgejo::Forgejo;
use forge::http::{HttpClient, HttpOptions, HttpRequest, ReqwestTransport, Url};
use forge::model::{ForgeKind, InboxSection, RepoRef};
use forge::sandbox::Target;
use forge::{Forge, INBOX_LIMIT};
use serde::Deserialize;

type Key = (String, u64);

#[derive(Deserialize)]
struct Row {
    number: u64,
    repository: RowRepo,
}

#[derive(Deserialize)]
struct RowRepo {
    full_name: String,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (host, repo_path) = match (args.next(), args.next()) {
        (Some(host), Some(repo_path)) => (host, repo_path),
        _ => match Target::from_env(ForgeKind::Forgejo) {
            Ok(target) => (target.host.clone(), target.slug()),
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        },
    };
    match run(&host, &repo_path).await {
        Ok(0) => {
            println!("0 differences");
            ExitCode::SUCCESS
        }
        Ok(count) => {
            println!("{count} differences");
            ExitCode::FAILURE
        }
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn run(host: &str, repo_path: &str) -> Result<usize, String> {
    let (owner, name) = repo_path.split_once('/').ok_or("repo must be owner/name")?;
    let repo = RepoRef::new(host, owner, name);
    let http = HttpClient::new(
        ForgeKind::Forgejo,
        host,
        Arc::new(ReqwestTransport::new().map_err(|e| e.to_string())?),
        Arc::new(CliCredentials {
            kind: ForgeKind::Forgejo,
            host: host.to_owned(),
        }),
        HttpOptions::for_kind(ForgeKind::Forgejo),
    );
    let forgejo = Forgejo::connect(http.clone())
        .await
        .map_err(|e| e.to_string())?;
    println!("Forgejo {} on {host}", forgejo.version());

    let mut differences = 0;

    let cases: [(&str, InboxSection, Vec<Key>); 3] = [
        (
            "review requested",
            InboxSection::ReviewRequested,
            search_keys(
                &http,
                host,
                &[("state", "open"), ("review_requested", "true")],
            )
            .await?,
        ),
        (
            "authored by me",
            InboxSection::AuthoredByMe,
            search_keys(&http, host, &[("state", "open"), ("created", "true")]).await?,
        ),
        (
            "query state=closed",
            InboxSection::Query("state=closed".to_owned()),
            search_keys(&http, host, &[("state", "closed")]).await?,
        ),
    ];
    for (label, section, expected) in cases {
        differences += compare(&forgejo, label, &section, expected, true).await?;
    }

    let from_fj = fj_open_numbers(host, repo_path)?
        .into_iter()
        .map(|number| (repo_path.to_owned(), number))
        .collect();
    differences += compare(
        &forgejo,
        &format!("open in {repo_path} (fj)"),
        &InboxSection::OpenInRepo(repo),
        from_fj,
        false,
    )
    .await?;
    Ok(differences)
}

/// Runs a section and diffs its pull request set with `expected`. With
/// `capped`, `expected` is the forge's own list, cut to [`INBOX_LIMIT`].
async fn compare(
    forgejo: &Forgejo,
    label: &str,
    section: &InboxSection,
    mut expected: Vec<Key>,
    capped: bool,
) -> Result<usize, String> {
    let pulls = forgejo.inbox(section).await.map_err(|e| e.to_string())?;
    if capped {
        expected.truncate(INBOX_LIMIT);
    }
    let got: BTreeSet<Key> = pulls
        .iter()
        .map(|pr| (pr.repo.path(), pr.number.0))
        .collect();
    let want: BTreeSet<Key> = expected.into_iter().collect();
    let mut count = 0;
    for (repo, number) in want.difference(&got) {
        println!("{label}: missing {repo}#{number}");
        count += 1;
    }
    for (repo, number) in got.difference(&want) {
        println!("{label}: extra {repo}#{number}");
        count += 1;
    }
    let unsorted = pulls.windows(2).any(|w| w[0].updated_at < w[1].updated_at);
    if unsorted {
        println!("{label}: not most recently updated first");
        count += 1;
    }
    println!("{label}: {} pull requests, {count} differences", got.len());
    Ok(count)
}

/// The search API's own answer, paged until a page is empty.
async fn search_keys(
    http: &HttpClient,
    host: &str,
    params: &[(&str, &str)],
) -> Result<Vec<Key>, String> {
    let mut keys = Vec::new();
    for page in 1_u32.. {
        let page = page.to_string();
        let mut query: Vec<(&str, &str)> = params.to_vec();
        query.extend([
            ("type", "pulls"),
            ("sort", "recentupdate"),
            ("limit", "50"),
            ("page", &page),
        ]);
        let url =
            Url::parse_with_params(&format!("https://{host}/api/v1/repos/issues/search"), query)
                .map_err(|e| e.to_string())?;
        let request = HttpRequest::get(url);
        let describe = request.describe();
        let response = http.send(request).await.map_err(|e| e.to_string())?;
        let rows: Vec<Row> = response.json(&describe).map_err(|e| e.to_string())?;
        if rows.is_empty() || keys.len() >= INBOX_LIMIT * 4 {
            break;
        }
        keys.extend(
            rows.into_iter()
                .map(|row| (row.repository.full_name, row.number)),
        );
    }
    Ok(keys)
}

/// `#8: WIP: Start a feature (by cora)` lines of `fj pr search`, whose
/// numbers are wrapped in Unicode isolation marks.
fn fj_open_numbers(host: &str, repo_path: &str) -> Result<Vec<u64>, String> {
    let output = Command::new("fj")
        .args([
            "-H", host, "--style", "minimal", "pr", "search", "-r", repo_path, "-s", "open",
        ])
        .output()
        .map_err(|e| format!("fj: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "fj pr search failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(text
        .lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix('#')?;
            let digits: String = rest
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse().ok()
        })
        .collect())
}
