//! Where the live checks run, and how recordings stay anonymous.
//!
//! The committed fixtures name fictional accounts ([`Target::placeholder`]).
//! A live run reads the real sandbox repositories from the environment:
//!
//! | variable                       | value                          |
//! |--------------------------------|--------------------------------|
//! | `REVIEW_SANDBOX_GITHUB`        | `owner/review-sandbox`         |
//! | `REVIEW_SANDBOX_GITLAB`        | `owner/review-sandbox`         |
//! | `REVIEW_SANDBOX_FORGEJO`       | `host/owner/review-sandbox`    |
//! | `REVIEW_SANDBOX_FORGEJO_REMOTE`| git remote URL (optional)      |
//! | `REVIEW_RECORD_REWRITES`       | `real=>placeholder;...` (optional) |
//!
//! Only a repository named [`SANDBOX`] is accepted, so a live tool can never
//! be pointed at a real project. [`Identity`] maps the real names to the
//! placeholders and back; the recorder uses it so a re-recorded fixture holds
//! no personal data.

use crate::http::{HttpRequest, HttpResponse, Transport, Url};
use crate::model::{ForgeKind, RepoRef};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

/// The only repository name the live tools write to.
pub const SANDBOX: &str = "review-sandbox";

pub const GITHUB_ENV: &str = "REVIEW_SANDBOX_GITHUB";
pub const GITLAB_ENV: &str = "REVIEW_SANDBOX_GITLAB";
pub const FORGEJO_ENV: &str = "REVIEW_SANDBOX_FORGEJO";
pub const FORGEJO_REMOTE_ENV: &str = "REVIEW_SANDBOX_FORGEJO_REMOTE";
/// Extra `real=>placeholder` pairs (display names, account ids), separated
/// by `;`, applied when recording.
pub const REWRITES_ENV: &str = "REVIEW_RECORD_REWRITES";

const PLACEHOLDER_OWNER: &str = "Lovelace";
const PLACEHOLDER_FORGEJO_OWNER: &str = "cora";
const PLACEHOLDER_FORGEJO_HOST: &str = "forgejo.example.com";

/// A sandbox repository on one forge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub kind: ForgeKind,
    pub host: String,
    pub owner: String,
    pub name: String,
    /// What `git clone` takes.
    pub remote: String,
}

impl Target {
    /// The fictional repository the committed fixtures describe.
    #[must_use]
    pub fn placeholder(kind: ForgeKind) -> Self {
        let (host, owner) = match kind {
            ForgeKind::GitHub => ("github.com", PLACEHOLDER_OWNER),
            ForgeKind::GitLab => ("gitlab.com", PLACEHOLDER_OWNER),
            ForgeKind::Forgejo => (PLACEHOLDER_FORGEJO_HOST, PLACEHOLDER_FORGEJO_OWNER),
        };
        Self::new(kind, host, owner, None)
    }

    fn new(kind: ForgeKind, host: &str, owner: &str, remote: Option<&str>) -> Self {
        let remote = remote.map_or_else(
            || format!("git@{host}:{owner}/{SANDBOX}.git"),
            str::to_owned,
        );
        Self {
            kind,
            host: host.to_owned(),
            owner: owner.to_owned(),
            name: SANDBOX.to_owned(),
            remote,
        }
    }

    /// The variable that configures `kind`.
    #[must_use]
    pub fn env_name(kind: ForgeKind) -> &'static str {
        match kind {
            ForgeKind::GitHub => GITHUB_ENV,
            ForgeKind::GitLab => GITLAB_ENV,
            ForgeKind::Forgejo => FORGEJO_ENV,
        }
    }

    /// Reads the real sandbox of `kind` from the environment.
    ///
    /// # Errors
    /// A message naming the variable when it is unset or malformed, or when
    /// the repository is not named [`SANDBOX`].
    pub fn from_env(kind: ForgeKind) -> Result<Self, String> {
        let var = Self::env_name(kind);
        let value = std::env::var(var).map_err(|_| {
            format!(
                "{var} is not set: it names the live sandbox ({}); see docs/sandboxes.md",
                Self::usage(kind)
            )
        })?;
        let remote = std::env::var(FORGEJO_REMOTE_ENV)
            .ok()
            .filter(|_| kind == ForgeKind::Forgejo);
        Self::parse(kind, &value, remote.as_deref()).map_err(|err| format!("{var}: {err}"))
    }

    fn usage(kind: ForgeKind) -> &'static str {
        match kind {
            ForgeKind::GitHub | ForgeKind::GitLab => "owner/review-sandbox",
            ForgeKind::Forgejo => "host/owner/review-sandbox",
        }
    }

    /// Parses `owner/review-sandbox` (GitHub, GitLab) or
    /// `host/owner/review-sandbox` (Forgejo).
    ///
    /// # Errors
    /// A message when the shape is wrong or the repository is not
    /// [`SANDBOX`].
    pub fn parse(kind: ForgeKind, value: &str, remote: Option<&str>) -> Result<Self, String> {
        let parts: Vec<&str> = value.trim().split('/').collect();
        let (host, owner, name) = match (kind, parts.as_slice()) {
            (ForgeKind::GitHub, [owner, name]) => ("github.com", *owner, *name),
            (ForgeKind::GitLab, [owner, name]) => ("gitlab.com", *owner, *name),
            (ForgeKind::Forgejo, [host, owner, name]) => (*host, *owner, *name),
            _ => return Err(format!("expected {}, got {value:?}", Self::usage(kind))),
        };
        if owner.is_empty() || host.is_empty() {
            return Err(format!("expected {}, got {value:?}", Self::usage(kind)));
        }
        if name != SANDBOX {
            return Err(format!(
                "refusing {name:?}: only repositories named {SANDBOX} may be used"
            ));
        }
        Ok(Self::new(kind, host, owner, remote))
    }

    #[must_use]
    pub fn repo(&self) -> RepoRef {
        RepoRef::new(&self.host, &self.owner, &self.name)
    }

    /// `owner/name`.
    #[must_use]
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }

    /// Real name to placeholder, for the owner and (Forgejo) the host.
    fn pairs(&self) -> Vec<(String, String)> {
        let placeholder = Self::placeholder(self.kind);
        let mut pairs = vec![(self.owner.clone(), placeholder.owner)];
        if self.kind == ForgeKind::Forgejo {
            pairs.push((self.host.clone(), placeholder.host));
        }
        pairs
    }
}

/// Real names and their placeholders. Whole-word matches only, and the
/// lower- and upper-case spellings of each name are covered too.
#[derive(Clone, Debug, Default)]
pub struct Identity {
    /// `(real, placeholder)`, longest real name first.
    pairs: Vec<(String, String)>,
}

impl Identity {
    #[must_use]
    pub fn from_pairs(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut all: Vec<(String, String)> = Vec::new();
        for (real, placeholder) in pairs {
            if real.is_empty() || real == placeholder {
                continue;
            }
            for pair in [
                (real.clone(), placeholder.clone()),
                (real.to_lowercase(), placeholder.to_lowercase()),
                (real.to_uppercase(), placeholder.to_uppercase()),
            ] {
                if !all.iter().any(|(r, _)| *r == pair.0) {
                    all.push(pair);
                }
            }
        }
        all.sort_by_key(|(real, _)| std::cmp::Reverse(real.len()));
        Self { pairs: all }
    }

    /// The owner (and Forgejo host) of one target.
    #[must_use]
    pub fn for_target(target: &Target) -> Self {
        Self::from_pairs(target.pairs())
    }

    /// Every configured sandbox plus [`REWRITES_ENV`]; for scrubbing a
    /// recording.
    #[must_use]
    pub fn from_env() -> Self {
        let mut pairs = Vec::new();
        for kind in [ForgeKind::GitHub, ForgeKind::GitLab, ForgeKind::Forgejo] {
            if let Ok(target) = Target::from_env(kind) {
                pairs.extend(target.pairs());
            }
        }
        if let Ok(extra) = std::env::var(REWRITES_ENV) {
            pairs.extend(parse_rewrites(&extra));
        }
        Self::from_pairs(pairs)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    #[must_use]
    pub fn to_placeholder(&self, text: &str) -> String {
        replace_words(
            text,
            self.pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())),
        )
    }

    #[must_use]
    pub fn to_real(&self, text: &str) -> String {
        let mut inverse: Vec<(&str, &str)> = self
            .pairs
            .iter()
            .map(|(real, placeholder)| (placeholder.as_str(), real.as_str()))
            .collect();
        inverse.sort_by_key(|(from, _)| std::cmp::Reverse(from.len()));
        replace_words(text, inverse)
    }

    fn map_value(&self, value: &mut Value, f: &impl Fn(&Self, &str) -> String) {
        match value {
            Value::String(text) => *text = f(self, text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.map_value(item, f)),
            Value::Object(map) => map.values_mut().for_each(|item| self.map_value(item, f)),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    /// Placeholders to real names in every string of `value`.
    pub fn value_to_real(&self, value: &mut Value) {
        self.map_value(value, &Self::to_real);
    }

    /// Real names to placeholders in every string of `value`.
    pub fn value_to_placeholder(&self, value: &mut Value) {
        self.map_value(value, &Self::to_placeholder);
    }
}

/// `real=>placeholder;real=>placeholder`.
fn parse_rewrites(text: &str) -> Vec<(String, String)> {
    text.split(';')
        .filter_map(|pair| pair.split_once("=>"))
        .map(|(real, placeholder)| (real.trim().to_owned(), placeholder.trim().to_owned()))
        .collect()
}

/// Replaces each `from` where it is not part of a longer alphanumeric word.
/// Pairs are tried in order at every position.
fn replace_words<'a>(text: &str, pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let pairs: Vec<(&str, &str)> = pairs.into_iter().collect();
    if pairs.is_empty() {
        return text.to_owned();
    }
    let bytes = text.as_bytes();
    let is_word = |b: u8| b.is_ascii_alphanumeric();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    'scan: while at < text.len() {
        if !text.is_char_boundary(at) {
            at += 1;
            continue;
        }
        let before_ok = at == 0 || !is_word(bytes[at - 1]);
        if before_ok {
            for (from, to) in &pairs {
                let end = at + from.len();
                if text[at..].starts_with(from) && (end == text.len() || !is_word(bytes[end])) {
                    out.push_str(to);
                    at = end;
                    continue 'scan;
                }
            }
        }
        let ch = text[at..].chars().next().unwrap_or_default();
        out.push(ch);
        at += ch.len_utf8();
    }
    out
}

/// Lets tests written against the placeholder names run against the real
/// sandbox: requests go out with real names, responses come back with
/// placeholders.
pub struct RewritingTransport {
    inner: Arc<dyn Transport>,
    identity: Identity,
}

impl RewritingTransport {
    #[must_use]
    pub fn new(inner: Arc<dyn Transport>, identity: Identity) -> Self {
        Self { inner, identity }
    }
}

#[async_trait]
impl Transport for RewritingTransport {
    async fn send(&self, request: &HttpRequest) -> crate::Result<HttpResponse> {
        let mut real = request.clone();
        if let Ok(url) = Url::parse(&self.identity.to_real(request.url.as_str())) {
            real.url = url;
        }
        if let Some(body) = &mut real.body {
            self.identity.value_to_real(body);
        }
        let mut response = self.inner.send(&real).await?;
        for (_, value) in &mut response.headers {
            *value = self.identity.to_placeholder(value);
        }
        if let Ok(text) = std::str::from_utf8(&response.body) {
            response.body = self.identity.to_placeholder(text).into_bytes();
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_sandbox_repositories() {
        let github = Target::parse(ForgeKind::GitHub, "someone/review-sandbox", None).unwrap();
        assert_eq!(github.host, "github.com");
        assert_eq!(github.remote, "git@github.com:someone/review-sandbox.git");
        let forgejo = Target::parse(
            ForgeKind::Forgejo,
            "git.example.net/someone/review-sandbox",
            Some("ssh://git@git.example.net:2222/someone/review-sandbox.git"),
        )
        .unwrap();
        assert_eq!(forgejo.host, "git.example.net");
        assert_eq!(forgejo.slug(), "someone/review-sandbox");
        assert!(forgejo.remote.starts_with("ssh://"));

        for bad in [
            "someone/work",
            "review-sandbox",
            "/review-sandbox",
            "a/b/review-sandbox",
        ] {
            assert!(
                Target::parse(ForgeKind::GitHub, bad, None).is_err(),
                "{bad}"
            );
        }
        assert!(Target::parse(ForgeKind::Forgejo, "someone/review-sandbox", None).is_err());
    }

    #[test]
    fn rewrites_whole_words_in_both_directions() {
        let target = Target::parse(
            ForgeKind::Forgejo,
            "git.real.test/realuser/review-sandbox",
            None,
        )
        .unwrap();
        let identity = Identity::for_target(&target);
        let real =
            "https://git.real.test/api/v1/repos/realuser/review-sandbox by REALUSER, realusers";
        let fake = identity.to_placeholder(real);
        assert_eq!(
            fake,
            "https://forgejo.example.com/api/v1/repos/cora/review-sandbox by CORA, realusers"
        );
        assert_eq!(
            identity.to_real("cora/review-sandbox, decorate, forgejo.example.com"),
            "realuser/review-sandbox, decorate, git.real.test"
        );
    }

    #[test]
    fn extra_pairs_prefer_the_longest_name() {
        let pairs = parse_rewrites("Real Name=>Ada Lovelace; Real=>Ada ;MDQ6=>MDQ7");
        let identity = Identity::from_pairs(pairs);
        assert_eq!(
            identity.to_placeholder("Real Name and Real, MDQ6="),
            "Ada Lovelace and Ada, MDQ7="
        );
    }

    #[test]
    fn rewrites_json_strings() {
        let target = Target::parse(ForgeKind::GitLab, "realuser/review-sandbox", None).unwrap();
        let identity = Identity::for_target(&target);
        let mut value = serde_json::json!({"path": "realuser/review-sandbox", "n": 1});
        identity.value_to_placeholder(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"path": "Lovelace/review-sandbox", "n": 1})
        );
        identity.value_to_real(&mut value);
        assert_eq!(value["path"], "realuser/review-sandbox");
    }
}
