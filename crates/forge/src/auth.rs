//! Tokens come from the forges' own CLIs, read at startup on the forge
//! runtime and held in memory only. The app stores no token.
//!
//! - GitHub: `gh auth token --hostname <host>` (gh keeps it in the OS
//!   keyring or `hosts.yml`; asking gh covers both).
//! - GitLab: `glab config get token --host <host>`, else `hosts.<host>.token`
//!   in glab's `config.yml`: `%LOCALAPPDATA%\glab-cli` on Windows,
//!   `$GLAB_CONFIG_DIR` or `~/.config/glab-cli` elsewhere. An OAuth token
//!   expires after two hours; [`CredentialSource::refresh`] runs a cheap
//!   `glab api user` so glab refreshes it, then reads it again.
//! - Forgejo: forgejo-cli's `keys.json`, `hosts.<host>` (after `aliases`),
//!   `{"type": "Application", "token"}` or `{"type": "OAuth", "token",
//!   "refresh_token", "expires_at"}`. It lives in
//!   `%APPDATA%\forgejo-cli\forgejo-cli\data\keys.json` on Windows and
//!   `~/.local/share/forgejo-cli/keys.json` on Linux (older installs:
//!   `Cyborus` in place of the first `forgejo-cli`). For OAuth, `fj -H <host>
//!   whoami` makes fj refresh and save it before it is read.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::model::ForgeKind;

/// A secret. `Debug` and `Display` print `***`, it is not `Serialize`, and
/// the only way to the text is [`Token::expose`], so it cannot reach a log
/// or a cache by accident.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Token(String);

impl Token {
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret itself, for the one place that sends it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// How the token goes into the `Authorization` header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheme {
    /// `Bearer <token>`: GitHub, GitLab (personal and OAuth), Forgejo OAuth.
    Bearer,
    /// `token <token>`: Forgejo application tokens.
    Token,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    pub token: Token,
    pub scheme: Scheme,
}

impl Credential {
    #[must_use]
    pub fn bearer(token: Token) -> Self {
        Self {
            token,
            scheme: Scheme::Bearer,
        }
    }

    /// The whole `Authorization` header value, itself a secret.
    #[must_use]
    pub fn authorization(&self) -> Token {
        let prefix = match self.scheme {
            Scheme::Bearer => "Bearer",
            Scheme::Token => "token",
        };
        Token(format!("{prefix} {}", self.token.expose()))
    }
}

/// Where a backend gets its credential. Called on the forge runtime, never
/// on the UI thread.
#[async_trait]
pub trait CredentialSource: Send + Sync {
    async fn load(&self) -> Result<Credential>;

    /// Called once after a 401, before the request is retried.
    async fn refresh(&self) -> Result<Credential> {
        self.load().await
    }
}

/// A fixed credential, for replayed tests.
pub struct StaticCredential(pub Credential);

#[async_trait]
impl CredentialSource for StaticCredential {
    async fn load(&self) -> Result<Credential> {
        Ok(self.0.clone())
    }
}

/// Reads the token from the forge's CLI.
pub struct CliCredentials {
    pub kind: ForgeKind,
    pub host: String,
}

#[async_trait]
impl CredentialSource for CliCredentials {
    async fn load(&self) -> Result<Credential> {
        match self.kind {
            ForgeKind::GitHub => github_token(&self.host).await,
            ForgeKind::GitLab => gitlab_token(&self.host).await,
            ForgeKind::Forgejo => forgejo_token(&self.host, Refresh::IfOAuth).await,
        }
    }

    async fn refresh(&self) -> Result<Credential> {
        match self.kind {
            ForgeKind::GitHub => github_token(&self.host).await,
            ForgeKind::GitLab => {
                // glab refreshes an expired OAuth token on any API call.
                run("glab", &["api", "--hostname", &self.host, "user"])
                    .await
                    .map_err(|error| refresh_failed(&self.host, "glab auth login", error))?;
                gitlab_token(&self.host).await
            }
            ForgeKind::Forgejo => forgejo_token(&self.host, Refresh::Always).await,
        }
    }
}

const CLI_TIMEOUT: Duration = Duration::from_secs(15);

async fn run(program: &str, args: &[&str]) -> Result<String> {
    let fail = |reason: String| Error::Cli {
        program: program.to_owned(),
        reason,
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // Without it every CLI call flashes a console window.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = tokio::time::timeout(CLI_TIMEOUT, command.output())
        .await
        .map_err(|_| fail(format!("timed out after {CLI_TIMEOUT:?}")))?
        .map_err(|err| fail(err.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(fail(format!("{}: {}", output.status, cli_message(&stderr))));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// What a CLI printed on failure, on one line: glab pads its message with
/// blank lines and an `ERROR` banner, and wraps it.
fn cli_message(stderr: &str) -> String {
    let words: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && *line != "ERROR")
        .collect();
    words.join(" ").chars().take(300).collect()
}

/// A token refresh through the CLI failed: the CLI's own login is gone,
/// and only signing in again brings it back.
fn refresh_failed(host: &str, login: &str, error: Error) -> Error {
    no_token(host, format!("{error}; run `{login}`"))
}

fn no_token(host: &str, reason: impl Into<String>) -> Error {
    Error::Auth {
        host: host.to_owned(),
        reason: reason.into(),
    }
}

async fn github_token(host: &str) -> Result<Credential> {
    let token = run("gh", &["auth", "token", "--hostname", host]).await?;
    if token.is_empty() {
        return Err(no_token(
            host,
            "`gh auth token` printed nothing; run `gh auth login`",
        ));
    }
    Ok(Credential::bearer(Token(token)))
}

async fn gitlab_token(host: &str) -> Result<Credential> {
    if let Ok(token) = run("glab", &["config", "get", "token", "--host", host]).await
        && !token.is_empty()
    {
        return Ok(Credential::bearer(Token(token)));
    }
    for path in glab_config_paths() {
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        if let Some(token) = glab_config_token(&text, host) {
            return Ok(Credential::bearer(token));
        }
    }
    Err(no_token(
        host,
        "glab has no token for this host; run `glab auth login`",
    ))
}

fn glab_config_paths() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("GLAB_CONFIG_DIR") {
        dirs.push(dir.into());
    }
    dirs.extend(dirs::config_local_dir().map(|d| d.join("glab-cli")));
    dirs.extend(dirs::config_dir().map(|d| d.join("glab-cli")));
    dirs.extend(dirs::home_dir().map(|d| d.join(".config").join("glab-cli")));
    dirs.dedup();
    dirs.into_iter().map(|d| d.join("config.yml")).collect()
}

/// `hosts.<host>.token` from glab's `config.yml`. A line scanner, not a YAML
/// parser: glab writes this file itself in a fixed block style.
fn glab_config_token(text: &str, host: &str) -> Option<Token> {
    let mut in_hosts = false;
    let mut host_indent: Option<usize> = None;
    let mut in_host = false;
    for line in text.lines() {
        let content = line.trim();
        if content.is_empty() || content.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            in_hosts = content == "hosts:";
            host_indent = None;
            in_host = false;
            continue;
        }
        let Some((key, value)) = content.split_once(':') else {
            continue;
        };
        if !in_hosts {
            continue;
        }
        if host_indent.is_none_or(|level| indent <= level) {
            host_indent = Some(indent);
            in_host = unquote(key) == host;
        } else if in_host && key.trim() == "token" {
            let value = unquote(value.trim());
            return (!value.is_empty()).then(|| Token(value.to_owned()));
        }
    }
    None
}

fn unquote(text: &str) -> &str {
    let text = text.trim();
    for quote in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote)) {
            return inner;
        }
    }
    text
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Refresh {
    IfOAuth,
    Always,
}

async fn forgejo_token(host: &str, refresh: Refresh) -> Result<Credential> {
    if refresh == Refresh::Always {
        run("fj", &["-H", host, "whoami"])
            .await
            .map_err(|error| refresh_failed(host, "fj auth login", error))?;
    }
    let login = read_fj_login(host).await?;
    if refresh == Refresh::IfOAuth && matches!(login, FjLogin::OAuth { .. }) {
        // fj refreshes an expired OAuth token and saves it when it is used.
        run("fj", &["-H", host, "whoami"]).await?;
        return Ok(read_fj_login(host).await?.into_credential());
    }
    Ok(login.into_credential())
}

async fn read_fj_login(host: &str) -> Result<FjLogin> {
    for path in fj_keys_paths() {
        let Ok(text) = tokio::fs::read_to_string(&path).await else {
            continue;
        };
        let keys: FjKeys = serde_json::from_str(&text)
            .map_err(|err| no_token(host, format!("cannot read {}: {err}", path.display())))?;
        if let Some(login) = keys.take(host) {
            return Ok(login);
        }
    }
    Err(no_token(
        host,
        "fj has no login for this host; run `fj auth login`",
    ))
}

fn fj_keys_paths() -> Vec<PathBuf> {
    let Some(data) = dirs::data_dir() else {
        return Vec::new();
    };
    // `directories::ProjectDirs::from("", org, "forgejo-cli")`: Windows puts
    // it at `{data}\{org}\{app}\data`, Linux at `{data}/{app}`.
    let layout = |org: &str| {
        if cfg!(windows) {
            data.join(org).join("forgejo-cli").join("data")
        } else {
            data.join("forgejo-cli")
        }
    };
    let mut paths = vec![layout("forgejo-cli"), layout("Cyborus")];
    paths.dedup();
    paths.into_iter().map(|d| d.join("keys.json")).collect()
}

#[derive(Deserialize)]
struct FjKeys {
    hosts: BTreeMap<String, FjLogin>,
    #[serde(default)]
    aliases: BTreeMap<String, String>,
}

impl FjKeys {
    fn take(mut self, host: &str) -> Option<FjLogin> {
        let host = self
            .aliases
            .get(host)
            .map_or(host, String::as_str)
            .to_owned();
        self.hosts.remove(&host)
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum FjLogin {
    Application { token: Token },
    OAuth { token: Token },
}

impl FjLogin {
    fn into_credential(self) -> Credential {
        match self {
            Self::Application { token } => Credential {
                token,
                scheme: Scheme::Token,
            },
            Self::OAuth { token } => Credential::bearer(token),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_never_prints() {
        let token = Token::new("ghp_supersecret");
        assert_eq!(format!("{token:?}"), "***");
        assert_eq!(format!("{token}"), "***");
        let credential = Credential::bearer(token);
        let debug = format!("{credential:?} {:?}", credential.authorization());
        assert!(!debug.contains("supersecret"), "{debug}");
        assert_eq!(
            credential.authorization().expose(),
            "Bearer ghp_supersecret"
        );
    }

    const GLAB: &str = "\
git_protocol: ssh
host: gitlab.com
hosts:
    gitlab.example.org:
        api_host: gitlab.example.org
        token: other
    gitlab.com:
        api_protocol: https
        token: \"glpat-abc\"
        is_oauth2: \"true\"
        oauth2_refresh_token: refresh
        custom_headers: []
        user: Lovelace
aliases:
    token: not-a-host
";

    #[test]
    fn reads_glab_host_token() {
        assert_eq!(
            glab_config_token(GLAB, "gitlab.com").unwrap().expose(),
            "glpat-abc"
        );
        assert_eq!(
            glab_config_token(GLAB, "gitlab.example.org")
                .unwrap()
                .expose(),
            "other"
        );
        assert!(glab_config_token(GLAB, "missing.example").is_none());
        assert!(
            glab_config_token("hosts:\n    gitlab.com:\n        token:\n", "gitlab.com").is_none()
        );
    }

    #[test]
    fn a_cli_failure_reads_on_one_line() {
        let glab = "          \n   ERROR  \n          \n  Oauth2: \"invalid_grant\" \"The grant is invalid, does not match the          \n  redirection URI.\".   \n\n";
        assert_eq!(
            cli_message(glab),
            "Oauth2: \"invalid_grant\" \"The grant is invalid, does not match the redirection URI.\"."
        );
        assert_eq!(cli_message(""), "");
        let error = refresh_failed(
            "gitlab.com",
            "glab auth login",
            Error::Cli {
                program: "glab".into(),
                reason: "exit code: 1: Oauth2: \"invalid_grant\"".into(),
            },
        );
        assert_eq!(
            error.to_string(),
            "not signed in to gitlab.com: `glab` failed: exit code: 1: Oauth2: \"invalid_grant\"; run `glab auth login`"
        );
    }

    #[test]
    fn reads_fj_login_through_aliases() {
        let json = r#"{
            "hosts": {
                "git.example.org": {"type": "Application", "token": "abc"},
                "oauth.example.org": {"type": "OAuth", "token": "def",
                    "refresh_token": "r", "expires_at": [2026, 320, 1, 2, 3, 0, 0, 0, 0]}
            },
            "aliases": {"git.example.org:2222": "git.example.org"},
            "default_ssh": ["git.example.org"]
        }"#;
        let keys = || serde_json::from_str::<FjKeys>(json).unwrap();
        let app = keys()
            .take("git.example.org:2222")
            .unwrap()
            .into_credential();
        assert_eq!(app.authorization().expose(), "token abc");
        let oauth = keys().take("oauth.example.org").unwrap().into_credential();
        assert_eq!(oauth.authorization().expose(), "Bearer def");
        assert!(keys().take("nope.example.org").is_none());
    }
}
