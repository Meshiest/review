//! What the live examples share: the `review-sandbox` repositories (named by
//! the `REVIEW_SANDBOX_*` variables, see `docs/sandboxes.md`), opening and closing a pull request through each forge's CLI, git in a
//! scratch clone, and a backend that records when `REVIEW_RECORD=1`.
//!
//! Safety: only repositories named [`SANDBOX`] are written to, nothing is
//! force-pushed, and a branch is only deleted when it starts with the
//! caller's prefix.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use forge::Forge;
use forge::auth::CliCredentials;
use forge::forgejo::Forgejo;
use forge::github::GitHub;
use forge::gitlab::GitLab;
use forge::http::{HttpClient, HttpOptions, ReqwestTransport, Transport};
use forge::model::{ForgeKind, PrNumber};
use forge::record::{RECORD_ENV, RecordingTransport, fixture_path};
use forge::sandbox::Identity;

#[allow(unused_imports, reason = "each example uses a different subset")]
pub use forge::sandbox::{SANDBOX, Target as Sandbox};

/// Opening and closing a pull request through the forge's CLI.
pub trait SandboxCli {
    fn open_pull_request(
        &self,
        dir: &Path,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PrNumber, String>;
    fn close_pull_request(&self, dir: &Path, number: PrNumber) -> Result<(), String>;
}

impl SandboxCli for Sandbox {
    /// Opens the pull request; returns its number.
    fn open_pull_request(
        &self,
        dir: &Path,
        branch: &str,
        title: &str,
        body: &str,
    ) -> Result<PrNumber, String> {
        let slug = self.slug();
        let output = match self.kind {
            ForgeKind::GitHub => run(
                "gh",
                &[
                    "pr", "create", "-R", &slug, "--head", branch, "--base", "main", "--title",
                    title, "--body", body,
                ],
                None,
            )?,
            // `glab mr create` needs a local clone with a GitLab remote even
            // with `-R`; the API does not.
            ForgeKind::GitLab => run(
                "glab",
                &[
                    "api",
                    "-X",
                    "POST",
                    &format!("projects/{}/merge_requests", slug.replace('/', "%2F")),
                    "-f",
                    &format!("source_branch={branch}"),
                    "-f",
                    "target_branch=main",
                    "-f",
                    &format!("title={title}"),
                    "-f",
                    &format!("description={body}"),
                ],
                None,
            )?,
            ForgeKind::Forgejo => {
                let output = run(
                    "fj",
                    &[
                        "-H", &self.host, "pr", "create", "--repo", &slug, "--base", "main",
                        "--head", branch, "--body", body, title,
                    ],
                    Some(dir),
                )?;
                return forgejo_created_number(&output)
                    .ok_or_else(|| format!("no pull request number in the fj output: {output}"));
            }
        };
        pull_request_number(&output)
            .ok_or_else(|| format!("no pull request number in the CLI output: {output}"))
    }

    fn close_pull_request(&self, dir: &Path, number: PrNumber) -> Result<(), String> {
        let slug = self.slug();
        let n = number.to_string();
        match self.kind {
            ForgeKind::GitHub => run("gh", &["pr", "close", &n, "-R", &slug], None),
            ForgeKind::GitLab => run(
                "glab",
                &[
                    "api",
                    "-X",
                    "PUT",
                    &format!("projects/{}/merge_requests/{n}", slug.replace('/', "%2F")),
                    "-f",
                    "state_event=close",
                ],
                None,
            ),
            ForgeKind::Forgejo => {
                let id = format!("{slug}#{n}");
                run("fj", &["-H", &self.host, "pr", "close", &id], Some(dir))
            }
        }
        .map(drop)
    }
}

/// `fj pr create` prints `created pull request #N: title`, wrapping each
/// styled run in Unicode isolates (U+2066 to U+2069).
pub fn forgejo_created_number(output: &str) -> Option<PrNumber> {
    let plain: String = output
        .chars()
        .filter(|c| !('\u{2066}'..='\u{2069}').contains(c))
        .collect();
    let (_, rest) = plain.split_once("pull request #")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().map(PrNumber)
}

/// The last `/pull/N`, `/pulls/N` or `/merge_requests/N` in the output.
pub fn pull_request_number(output: &str) -> Option<PrNumber> {
    ["/pull/", "/pulls/", "/merge_requests/"]
        .iter()
        .filter_map(|marker| {
            let (at, _) = output.rmatch_indices(marker).next()?;
            let digits: String = output[at + marker.len()..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            Some((at, digits.parse().ok()?))
        })
        .max_by_key(|(at, _)| *at)
        .map(|(_, n)| PrNumber(n))
}

/// Runs a program, returning stdout and stderr together; a non-zero exit is
/// an error naming the command.
pub fn run(program: &str, args: &[&str], dir: Option<&Path>) -> Result<String, String> {
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    let output = command
        .output()
        .map_err(|err| format!("`{program}` did not start: {err}"))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if output.status.success() {
        Ok(text)
    } else {
        Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            text.trim()
        ))
    }
}

pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut full = vec!["-C", dir.to_str().ok_or("scratch path is not UTF-8")?];
    full.extend_from_slice(args);
    run("git", &full, None)
}

/// `.scratch/<tool>/<forge>` at the workspace root.
pub fn scratch_dir(tool: &str, kind: ForgeKind) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../.scratch")
        .join(tool)
        .join(kind.as_str())
}

/// Clones (or fetches) the sandbox into `dir` and checks out a new local
/// `branch` from `origin/main`. Only this tool's own clone is reset.
pub fn fresh_branch(sandbox: &Sandbox, dir: &Path, branch: &str) -> Result<(), String> {
    if dir.join(".git").is_dir() {
        git(dir, &["fetch", "--prune", "origin"])?;
    } else {
        let parent = dir.parent().ok_or("scratch dir has no parent")?;
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        let target = dir.to_str().ok_or("scratch path is not UTF-8")?;
        run("git", &["clone", "--quiet", &sandbox.remote, target], None)?;
    }
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
    )
    .map(drop)
}

/// Pushes the local `branch` to the same name; never forced.
pub fn push(dir: &Path, branch: &str) -> Result<(), String> {
    git(
        dir,
        &["push", "--quiet", "origin", &format!("{branch}:{branch}")],
    )
    .map(drop)
}

/// Deletes the remote `branch`, only when it starts with `prefix`.
pub fn delete_branch(dir: &Path, branch: &str, prefix: &str) -> Result<(), String> {
    if !branch.starts_with(prefix) {
        return Err(format!(
            "refusing to delete {branch}: not a {prefix} branch"
        ));
    }
    git(dir, &["push", "--quiet", "origin", "--delete", branch]).map(drop)
}

/// The backend for a sandbox over the network with the CLI's token. With
/// `REVIEW_RECORD=1` its HTTP is recorded into `tests/fixtures/<forge>/<fixture>.json`,
/// leaving out the JSON members named in `pruned`.
pub async fn backend(
    sandbox: &Sandbox,
    fixture: &str,
    pruned: &'static [&'static str],
) -> forge::Result<Arc<dyn Forge>> {
    let (kind, host) = (sandbox.kind, sandbox.host.as_str());
    let mut transport: Arc<dyn Transport> = Arc::new(ReqwestTransport::new()?);
    if std::env::var(RECORD_ENV).is_ok_and(|v| v == "1") {
        let path = fixture_path(kind, fixture);
        println!("recording to {}", path.display());
        transport = Arc::new(
            RecordingTransport::new(transport, path)
                .pruning(pruned)
                .with_identity(Identity::from_env()),
        );
    }
    let credentials = Arc::new(CliCredentials {
        kind,
        host: host.to_owned(),
    });
    let http = HttpClient::new(
        kind,
        host,
        transport,
        credentials,
        HttpOptions::for_kind(kind),
    );
    http.authorize().await?;
    Ok(match kind {
        ForgeKind::GitHub => Arc::new(GitHub::connect(http).await?),
        ForgeKind::GitLab => Arc::new(GitLab::connect(http).await?),
        ForgeKind::Forgejo => Arc::new(Forgejo::connect(http).await?),
    })
}

/// `--forge github|gitlab|forgejo`, the only argument; the repository comes
/// from the environment.
pub fn parse_args(tool: &str) -> Result<Sandbox, String> {
    let usage = format!("usage: {tool} --forge github|gitlab|forgejo");
    let mut args = std::env::args().skip(1);
    let (Some(flag), Some(value), None) = (args.next(), args.next(), args.next()) else {
        return Err(usage);
    };
    if flag != "--forge" {
        return Err(usage);
    }
    let kind = [ForgeKind::GitHub, ForgeKind::GitLab, ForgeKind::Forgejo]
        .into_iter()
        .find(|kind| kind.as_str() == value)
        .ok_or(usage)?;
    Sandbox::from_env(kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_pull_request_number() {
        let cases = [
            ("https://github.com/Lovelace/review-sandbox/pull/12\n", 12),
            (
                "!3 Round trip (round-trip/x)\n https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/31\n",
                31,
            ),
            ("created https://git.example/cora/review-sandbox/pulls/9", 9),
        ];
        for (text, n) in cases {
            assert_eq!(pull_request_number(text), Some(PrNumber(n)), "{text}");
        }
        assert_eq!(pull_request_number("no url"), None);
    }

    #[test]
    fn finds_the_number_fj_prints() {
        let output = "created pull request #\u{2068}9\u{2069}: \u{2068}Round trip x\u{2069}\n";
        assert_eq!(forgejo_created_number(output), Some(PrNumber(9)));
        assert_eq!(forgejo_created_number("no number"), None);
    }
}
