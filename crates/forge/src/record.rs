//! Recorded HTTP for offline tests.
//!
//! [`RecordingTransport`] wraps the real network and writes every request
//! and response pair to `tests/fixtures/<forge>/<name>.json`, scrubbed:
//! no request headers at all (so never `Authorization`), the sending token
//! and anything shaped like a GitHub or GitLab token replaced by `***`, and
//! emails replaced by `user@example.invalid`, Gravatar and avatar hashes
//! zeroed, and the real sandbox owner and host (see [`crate::sandbox`])
//! replaced by the placeholders the committed fixtures use.
//! [`ReplayTransport`] serves them
//! back, matching on method, path, sorted query and the JSON body; a GraphQL
//! body matches on its operation name and variables only.
//!
//! [`fixture_client`] picks the mode: replay by default, record when
//! `REVIEW_RECORD=1` is set (a manual run against the sandbox repos).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{CliCredentials, Credential, CredentialSource, StaticCredential, Token};
use crate::error::{Error, Result};
use crate::http::{
    HttpClient, HttpOptions, HttpRequest, HttpResponse, ManualClock, Method, ReqwestTransport,
    Transport,
};
use crate::model::ForgeKind;
use crate::sandbox::{Identity, RewritingTransport, Target};

/// Set to `1` to record fixtures against the live forge instead of replaying.
pub const RECORD_ENV: &str = "REVIEW_RECORD";

/// `crates/forge/tests/fixtures/<forge>/<name>.json`.
#[must_use]
pub fn fixture_path(kind: ForgeKind, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(kind.as_str())
        .join(format!("{name}.json"))
}

/// An [`HttpClient`] for a conformance test: replaying `<forge>/<name>.json`
/// with a dummy token and a fake clock (recorded retries do not sleep), or,
/// with `REVIEW_RECORD=1`, recording through the network with the CLI's
/// token.
///
/// # Errors
/// [`Error::Recording`] when replaying and the fixture is missing or invalid.
pub fn fixture_client(kind: ForgeKind, host: &str, name: &str) -> Result<HttpClient> {
    let path = fixture_path(kind, name);
    let recording = std::env::var(RECORD_ENV).is_ok_and(|v| v == "1");
    let (transport, credentials, options): (Arc<dyn Transport>, Arc<dyn CredentialSource>, _) =
        if recording {
            let target = Target::from_env(kind).map_err(|reason| Error::Recording {
                path: path.display().to_string(),
                reason,
            })?;
            let names = Identity::for_target(&target);
            let live = Arc::new(RewritingTransport::new(
                Arc::new(ReqwestTransport::new()?),
                names.clone(),
            ));
            let recorder = RecordingTransport::new(live, path).with_identity(Identity::from_env());
            // The account also holds private work merge requests: only the
            // sandbox's may be recorded.
            let recorder = match kind {
                ForgeKind::GitLab => {
                    recorder.only_merge_requests_of(Target::placeholder(kind).slug())
                }
                ForgeKind::GitHub | ForgeKind::Forgejo => recorder,
            };
            (
                Arc::new(recorder),
                Arc::new(CliCredentials {
                    kind,
                    host: names.to_real(host),
                }),
                HttpOptions::for_kind(kind),
            )
        } else {
            let options = HttpOptions {
                clock: Arc::new(ManualClock::new(Duration::from_secs(1_700_000_000))),
                ..HttpOptions::for_kind(kind)
            };
            (
                Arc::new(ReplayTransport::load(&path)?),
                Arc::new(StaticCredential(Credential::bearer(Token::new("replay")))),
                options,
            )
        };
    Ok(HttpClient::new(kind, host, transport, credentials, options))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Cassette {
    pub exchanges: Vec<Exchange>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    pub request: RecordedRequest,
    pub response: RecordedResponse,
}

/// The match key of a request, as stored. Two requests match when these
/// are equal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedRequest {
    pub method: Method,
    pub path: String,
    /// Sorted by name, then value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub query: Vec<(String, String)>,
    /// GraphQL `operationName` (or the name in the query text).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// GraphQL variables; `{}` when there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variables: Option<Value>,
    /// A non-GraphQL JSON body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Value>,
}

impl RecordedRequest {
    fn from_request(request: &HttpRequest, scrub: &Scrubber) -> Self {
        let mut query: Vec<(String, String)> = request
            .url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), scrub.text(&v)))
            .collect();
        query.sort();
        let mut key = Self {
            method: request.method,
            path: scrub.identity.to_placeholder(request.url.path()),
            query,
            operation: None,
            variables: None,
            body: None,
        };
        let Some(body) = &request.body else {
            return key;
        };
        match body.get("query").and_then(Value::as_str) {
            Some(text) => {
                key.operation = body
                    .get("operationName")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| operation_name(text));
                let mut variables = match body.get("variables") {
                    None | Some(Value::Null) => Value::Object(serde_json::Map::new()),
                    Some(vars) => vars.clone(),
                };
                scrub.value(&mut variables);
                key.variables = Some(variables);
            }
            None => {
                let mut body = body.clone();
                scrub.value(&mut body);
                key.body = Some(body);
            }
        }
        key
    }

    /// `GET /path?a=1`, or `POST /graphql operation=Name variables={...}`.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut text = format!("{} {}", self.method, self.path);
        if !self.query.is_empty() {
            let pairs: Vec<String> = self.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
            text.push('?');
            text.push_str(&pairs.join("&"));
        }
        if let Some(operation) = &self.operation {
            text.push_str(&format!(" operation={operation}"));
        }
        if let Some(variables) = &self.variables {
            text.push_str(&format!(" variables={variables}"));
        }
        if let Some(body) = &self.body {
            text.push_str(&format!(" body={body}"));
        }
        text
    }
}

/// `query Name(` / `mutation Name {` -> `Name`.
fn operation_name(query: &str) -> Option<String> {
    let mut words = query.split(|c: char| c.is_whitespace() || c == '(' || c == '{');
    words
        .by_ref()
        .find(|w| matches!(*w, "query" | "mutation" | "subscription"))?;
    words.find(|w| !w.is_empty()).map(str::to_owned)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecordedResponse {
    pub status: u16,
    /// Only the headers the client reads (paging, ETag, rate limits).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    pub body: RecordedBody,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedBody {
    Empty,
    Json(Value),
    Text(String),
}

const KEPT_HEADERS: &[&str] = &[
    "content-type",
    "content-range",
    "etag",
    "link",
    "retry-after",
    "x-total-count",
    "x-total",
    "x-total-pages",
    "x-page",
    "x-next-page",
    "x-per-page",
    "x-ratelimit-remaining",
    "x-ratelimit-reset",
    "ratelimit-remaining",
    "ratelimit-reset",
];

impl RecordedResponse {
    fn from_response(response: &HttpResponse, scrub: &Scrubber) -> Self {
        let headers = response
            .headers
            .iter()
            .filter(|(name, _)| KEPT_HEADERS.contains(&name.to_ascii_lowercase().as_str()))
            .map(|(name, value)| (name.to_ascii_lowercase(), scrub.text(value)))
            .collect();
        let body = if response.body.is_empty() {
            RecordedBody::Empty
        } else if let Ok(mut json) = serde_json::from_slice::<Value>(&response.body) {
            scrub.value(&mut json);
            RecordedBody::Json(json)
        } else {
            RecordedBody::Text(scrub.text(&response.text()))
        };
        Self {
            status: response.status,
            headers,
            body,
        }
    }

    fn to_response(&self) -> HttpResponse {
        let body = match &self.body {
            RecordedBody::Empty => Vec::new(),
            RecordedBody::Json(json) => serde_json::to_vec(json).unwrap_or_default(),
            RecordedBody::Text(text) => text.as_bytes().to_vec(),
        };
        HttpResponse {
            status: self.status,
            headers: self.headers.clone().into_iter().collect(),
            body,
            cached: false,
        }
    }
}

const SCRUBBED: &str = "***";
const SCRUBBED_EMAIL: &str = "user@example.invalid";
/// Whether a JSON key names a credential. Deny by default: any key that
/// mentions a token, secret, password or key material is scrubbed, so a
/// field a forge adds later (GitLab's `runners_token` was one) cannot leak.
#[must_use]
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "token",
        "secret",
        "password",
        "passwd",
        "private_key",
        "api_key",
        "apikey",
        "credential",
    ]
    .iter()
    .any(|word| key.contains(word))
}

/// Prefixes of forge credentials; a word starting with one is scrubbed
/// wherever it appears, and fixtures are checked for them.
pub const TOKEN_PREFIXES: &[&str] = &[
    "GR1348941",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "gloas-",
    "glrt-",
    "glptt-",
    "gldt-",
    "glsoat-",
    "glcbt-",
    "glft-",
    "glimt-",
    "glagent-",
];

/// Removes secrets and emails. Deterministic, so a live request scrubbed at
/// replay time equals the one scrubbed when it was recorded.
#[derive(Default)]
struct Scrubber {
    secrets: Vec<String>,
    identity: Identity,
}

impl Scrubber {
    /// Knows the token the request was sent with, header scheme removed, and
    /// the real names to replace.
    fn for_request(request: &HttpRequest, identity: &Identity) -> Self {
        let secrets = request
            .authorization
            .as_ref()
            .map(|auth| {
                let value = auth.expose();
                value
                    .split_once(' ')
                    .map_or(value, |(_, token)| token)
                    .to_owned()
            })
            .filter(|token| token.len() >= 8)
            .into_iter()
            .collect();
        Self {
            secrets,
            identity: identity.clone(),
        }
    }

    fn value(&self, value: &mut Value) {
        match value {
            Value::String(text) => *text = self.text(text),
            Value::Array(items) => items.iter_mut().for_each(|item| self.value(item)),
            Value::Object(map) => {
                for (key, item) in map.iter_mut() {
                    let key = key.to_ascii_lowercase();
                    if let Value::String(text) = item
                        && !text.is_empty()
                    {
                        if key.contains("email") {
                            *text = SCRUBBED_EMAIL.to_owned();
                            continue;
                        }
                        if is_secret_key(&key) {
                            *text = SCRUBBED.to_owned();
                            continue;
                        }
                        if PROFILE_KEYS.contains(&key.as_str()) {
                            text.clear();
                            continue;
                        }
                    }
                    self.value(item);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }

    fn text(&self, text: &str) -> String {
        let mut text = self.identity.to_placeholder(text);
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), SCRUBBED);
            }
        }
        zero_avatar_hashes(&scrub_emails(&scrub_token_words(&text)))
    }
}

/// Profile strings a GitLab user object carries (`website_url`, `bio`, ...):
/// personal, and never read by a backend.
const PROFILE_KEYS: &[&str] = &[
    "bio",
    "discord",
    "job_title",
    "linkedin",
    "location",
    "organization",
    "pronouns",
    "twitter",
    "website_url",
    "work_information",
];

/// An avatar URL carries a hash of an email address or account: Gravatar's
/// path, Forgejo's `/avatars/<hash>`, GitHub's `?u=<hash>`. Each is replaced
/// by zeros of the same length.
fn zero_avatar_hashes(text: &str) -> String {
    let text = zero_hex_after(text, "gravatar.com/avatar/", 1);
    let text = zero_hex_after(&text, "/avatars/", 16);
    if text.contains("githubusercontent.com/") {
        zero_hex_after(&text, "?u=", 16)
    } else {
        text
    }
}

/// Zeroes the run of hex digits after each `marker`, when it has at least
/// `min` digits.
fn zero_hex_after(text: &str, marker: &str, min: usize) -> String {
    if !text.contains(marker) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        let end = at + marker.len();
        out.push_str(&rest[..end]);
        rest = &rest[end..];
        let hash = rest
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(rest.len());
        let digits = if hash >= min {
            "0".repeat(hash)
        } else {
            rest[..hash].to_owned()
        };
        out.push_str(&digits);
        rest = &rest[hash..];
    }
    out.push_str(rest);
    out
}

/// Removes every object member named in `keys`, at any depth.
fn prune(value: &mut Value, keys: &[&str]) {
    match value {
        Value::Array(items) => items.iter_mut().for_each(|item| prune(item, keys)),
        Value::Object(map) => {
            map.retain(|key, _| !keys.contains(&key.as_str()));
            map.values_mut().for_each(|item| prune(item, keys));
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// A merge request row carries its page URL as `webUrl` (GraphQL) or
/// `web_url` (REST); users and projects have none that contains
/// `/-/merge_requests/`.
fn is_foreign_merge_request(value: &Value, own: &str) -> bool {
    let Some(map) = value.as_object() else {
        return false;
    };
    ["webUrl", "web_url"]
        .iter()
        .filter_map(|key| map.get(*key)?.as_str())
        .any(|url| url.contains("/-/merge_requests/") && !url.contains(own))
}

/// Removes the array elements that are merge requests of another project.
fn drop_foreign_merge_requests(value: &mut Value, project: &str) {
    let own = format!("/{project}/-/merge_requests/");
    drop_foreign(value, &own);
}

fn drop_foreign(value: &mut Value, own: &str) {
    match value {
        Value::Array(items) => {
            items.retain(|item| !is_foreign_merge_request(item, own));
            items.iter_mut().for_each(|item| drop_foreign(item, own));
        }
        Value::Object(map) => map.values_mut().for_each(|item| drop_foreign(item, own)),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn scrub_token_words(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_token_char) {
        out.push_str(&rest[..start]);
        let word_len = rest[start..]
            .find(|c| !is_token_char(c))
            .unwrap_or(rest.len() - start);
        let word = &rest[start..start + word_len];
        let is_token = TOKEN_PREFIXES
            .iter()
            .any(|prefix| word.starts_with(prefix) && word.len() >= prefix.len() + 16);
        out.push_str(if is_token { SCRUBBED } else { word });
        rest = &rest[start + word_len..];
    }
    out.push_str(rest);
    out
}

fn scrub_emails(text: &str) -> String {
    let is_local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
    let is_domain = |b: u8| b.is_ascii_alphanumeric() || b".-".contains(&b);
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut copied = 0;
    for (at, _) in text.match_indices('@') {
        if at < copied {
            continue;
        }
        let start = bytes[copied..at]
            .iter()
            .rposition(|&b| !is_local(b))
            .map_or(copied, |i| copied + i + 1);
        let end = bytes[at + 1..]
            .iter()
            .position(|&b| !is_domain(b))
            .map_or(bytes.len(), |i| at + 1 + i);
        let local = &text[start..at];
        let domain = text[at + 1..end].trim_end_matches('.');
        let end = at + 1 + domain.len();
        // `git@host:` is an SSH URL, not an address.
        let is_email =
            !local.is_empty() && local != "git" && domain.contains('.') && !domain.starts_with('.');
        if is_email {
            out.push_str(&text[copied..start]);
            out.push_str(SCRUBBED_EMAIL);
            copied = end;
        }
    }
    out.push_str(&text[copied..]);
    out
}

/// Sends through `inner` and appends each scrubbed pair to the cassette
/// file, rewriting it after every exchange so an aborted run keeps what it
/// saw. Recording is a manual tool, so the write is synchronous.
pub struct RecordingTransport {
    inner: Arc<dyn Transport>,
    path: PathBuf,
    cassette: Mutex<Cassette>,
    pruned: &'static [&'static str],
    only_project: Option<String>,
    identity: Identity,
}

impl RecordingTransport {
    /// Starts an empty cassette at `path`, replacing any old one on the
    /// first exchange.
    #[must_use]
    pub fn new(inner: Arc<dyn Transport>, path: PathBuf) -> Self {
        Self {
            inner,
            path,
            cassette: Mutex::new(Cassette::default()),
            pruned: &[],
            only_project: None,
            identity: Identity::default(),
        }
    }

    /// Drops merge request rows of any project but `project` (`group/name`)
    /// from recorded JSON responses, at any depth: an account-wide inbox
    /// also lists merge requests from private repositories, which must never
    /// reach a cassette.
    #[must_use]
    pub fn only_merge_requests_of(mut self, project: impl Into<String>) -> Self {
        self.only_project = Some(project.into());
        self
    }

    /// Replaces the real names in `identity` with their placeholders in
    /// everything recorded.
    #[must_use]
    pub fn with_identity(mut self, identity: Identity) -> Self {
        self.identity = identity;
        self
    }

    /// Leaves the members named in `keys` out of recorded JSON responses, at
    /// any depth: for long polling recordings whose responses embed large
    /// objects no backend reads (a run's `repository`, a job's `user`).
    #[must_use]
    pub fn pruning(mut self, keys: &'static [&'static str]) -> Self {
        self.pruned = keys;
        self
    }

    fn save(&self, exchange: Exchange) -> Result<()> {
        let fail = |reason: String| Error::Recording {
            path: self.path.display().to_string(),
            reason,
        };
        let text = {
            let mut cassette = self.cassette.lock().unwrap_or_else(PoisonError::into_inner);
            cassette.exchanges.push(exchange);
            serde_json::to_string_pretty(&*cassette).map_err(|err| fail(err.to_string()))?
        };
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|err| fail(err.to_string()))?;
        }
        std::fs::write(&self.path, text + "\n").map_err(|err| fail(err.to_string()))
    }
}

#[async_trait]
impl Transport for RecordingTransport {
    async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let response = self.inner.send(request).await?;
        let scrub = Scrubber::for_request(request, &self.identity);
        let mut recorded = RecordedResponse::from_response(&response, &scrub);
        if let RecordedBody::Json(json) = &mut recorded.body {
            prune(json, self.pruned);
            if let Some(project) = &self.only_project {
                drop_foreign_merge_requests(json, project);
            }
        }
        self.save(Exchange {
            request: RecordedRequest::from_request(request, &scrub),
            response: recorded,
        })?;
        Ok(response)
    }
}

/// Serves a cassette. Each exchange is served once, in order; when all
/// exchanges for a request are used, the last one repeats (a poll). A
/// request with no recording is [`Error::ReplayMiss`], naming it.
pub struct ReplayTransport {
    name: String,
    exchanges: Vec<Exchange>,
    used: Mutex<Vec<bool>>,
}

impl ReplayTransport {
    /// # Errors
    /// [`Error::Recording`] when the file is missing or not a cassette.
    pub fn load(path: &Path) -> Result<Self> {
        let fail = |reason: String| Error::Recording {
            path: path.display().to_string(),
            reason,
        };
        let text = std::fs::read_to_string(path).map_err(|err| fail(err.to_string()))?;
        let cassette: Cassette =
            serde_json::from_str(&text).map_err(|err| fail(err.to_string()))?;
        Ok(Self::new(path.display().to_string(), cassette))
    }

    #[must_use]
    pub fn new(name: impl Into<String>, cassette: Cassette) -> Self {
        let used = vec![false; cassette.exchanges.len()];
        Self {
            name: name.into(),
            exchanges: cassette.exchanges,
            used: Mutex::new(used),
        }
    }

    /// Recorded requests that were never asked for; a conformance test can
    /// assert this is empty to catch a backend that stopped making a call.
    #[must_use]
    pub fn unused(&self) -> Vec<String> {
        let used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        self.exchanges
            .iter()
            .zip(used.iter())
            .filter(|(_, used)| !**used)
            .map(|(exchange, _)| exchange.request.describe())
            .collect()
    }
}

#[async_trait]
impl Transport for ReplayTransport {
    async fn send(&self, request: &HttpRequest) -> Result<HttpResponse> {
        let key = RecordedRequest::from_request(request, &Scrubber::default());
        let mut used = self.used.lock().unwrap_or_else(PoisonError::into_inner);
        let matching = || {
            self.exchanges
                .iter()
                .enumerate()
                .filter(|(_, exchange)| exchange.request == key)
        };
        let fresh = matching().find(|(i, _)| !used[*i]).map(|(i, _)| i);
        let index = fresh.or_else(|| matching().next_back().map(|(i, _)| i));
        let Some(index) = index else {
            return Err(Error::ReplayMiss {
                cassette: self.name.clone(),
                request: key.describe(),
            });
        };
        used[index] = true;
        Ok(self.exchanges[index].response.to_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Url;

    #[test]
    fn scrubs_emails_but_not_mentions_or_ssh() {
        let text = "Signed-off-by: A <a.b+c@mail.example.com>. review-requested:@me git@github.com:o/r.git";
        assert_eq!(
            scrub_emails(text),
            "Signed-off-by: A <user@example.invalid>. review-requested:@me git@github.com:o/r.git"
        );
        assert_eq!(scrub_emails("x@y.z@"), "user@example.invalid@");
    }

    #[test]
    fn scrubs_token_shapes() {
        let text = "token glpat-0123456789abcdefghij and ghp_short";
        assert_eq!(scrub_token_words(text), "token *** and ghp_short");
    }

    #[test]
    fn scrubs_json_keys_and_the_sending_token() {
        let request = HttpRequest {
            authorization: Some(Token::new("Bearer sekrit-token-value")),
            ..HttpRequest::get(Url::parse("https://x.example/").unwrap())
        };
        let scrub = Scrubber::for_request(&request, &Identity::default());
        let mut value = serde_json::json!({
            "commit_email": "me@example.org",
            "public_email": "",
            "token": "abc",
            "nested": [{"note": "echo sekrit-token-value"}],
        });
        scrub.value(&mut value);
        assert_eq!(
            value,
            serde_json::json!({
                "commit_email": SCRUBBED_EMAIL,
                "public_email": "",
                "token": SCRUBBED,
                "nested": [{"note": "echo ***"}],
            })
        );
    }

    #[test]
    fn zeroes_gravatar_hashes_and_blanks_profiles() {
        let text = "https://secure.gravatar.com/avatar/c1b5f9ab?s=80 and gravatar.com/avatar/";
        assert_eq!(
            zero_avatar_hashes(text),
            "https://secure.gravatar.com/avatar/00000000?s=80 and gravatar.com/avatar/"
        );
        let mut value =
            serde_json::json!({"user": {"website_url": "me.example", "bio": "", "name": "x"}});
        Scrubber::default().value(&mut value);
        assert_eq!(
            value,
            serde_json::json!({"user": {"website_url": "", "bio": "", "name": "x"}})
        );
    }

    #[test]
    fn zeroes_forgejo_and_github_avatar_hashes() {
        let hash = "9fa31d710b045f92813fa0f1da870051654188c5f3264c7cb759a783a9b93e5a";
        let forgejo = format!("https://forge.example/avatars/{hash}");
        assert_eq!(
            zero_avatar_hashes(&forgejo),
            format!("https://forge.example/avatars/{}", "0".repeat(64))
        );
        assert_eq!(
            zero_avatar_hashes("https://forge.example/avatars/scrubbed"),
            "https://forge.example/avatars/scrubbed"
        );
        let github = "https://avatars.githubusercontent.com/u/7?u=0000000000000000000000000000000000000000&v=4";
        assert_eq!(
            zero_avatar_hashes(github),
            format!(
                "https://avatars.githubusercontent.com/u/7?u={}&v=4",
                "0".repeat(40)
            )
        );
        assert_eq!(
            zero_avatar_hashes("https://x.example/?u=abc"),
            "https://x.example/?u=abc"
        );
    }

    #[test]
    fn records_placeholders_for_real_names() {
        let target =
            crate::sandbox::Target::parse(ForgeKind::GitLab, "realuser/review-sandbox", None)
                .unwrap();
        let identity = Identity::for_target(&target);
        let request = HttpRequest::get(
            Url::parse("https://gitlab.com/api/v4/projects/realuser%2Freview-sandbox?q=realuser")
                .unwrap(),
        );
        let scrub = Scrubber::for_request(&request, &identity);
        let key = RecordedRequest::from_request(&request, &scrub);
        assert_eq!(key.path, "/api/v4/projects/Lovelace%2Freview-sandbox");
        assert_eq!(key.query, [("q".to_owned(), "Lovelace".to_owned())]);
        let mut value = serde_json::json!({"web_url": "https://gitlab.com/realuser"});
        scrub.value(&mut value);
        assert_eq!(value["web_url"], "https://gitlab.com/Lovelace");
    }

    #[test]
    fn prunes_members_at_any_depth() {
        let mut value = serde_json::json!({
            "workflow_runs": [{"id": 1, "repository": {"big": true}, "head_sha": "a"}],
            "repository": 1,
        });
        prune(&mut value, &["repository"]);
        assert_eq!(
            value,
            serde_json::json!({"workflow_runs": [{"id": 1, "head_sha": "a"}]})
        );
    }

    #[test]
    fn drops_merge_requests_of_other_projects() {
        let mut value = serde_json::json!({"data": {"currentUser": {"mergeRequests": {
            "nodes": [
                {"iid": "1", "webUrl": "https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/1"},
                {"iid": "9", "webUrl": "https://gitlab.com/work/secret/-/merge_requests/9"},
                {"iid": "2", "web_url": "https://gitlab.com/Lovelace/review-sandbox-2/-/merge_requests/2"},
                {"username": "me", "web_url": "https://gitlab.com/me"},
            ],
        }}}});
        drop_foreign_merge_requests(&mut value, "Lovelace/review-sandbox");
        assert_eq!(
            value,
            serde_json::json!({"data": {"currentUser": {"mergeRequests": {"nodes": [
                {"iid": "1", "webUrl": "https://gitlab.com/Lovelace/review-sandbox/-/merge_requests/1"},
                {"username": "me", "web_url": "https://gitlab.com/me"},
            ]}}}})
        );
    }

    #[test]
    fn finds_operation_names() {
        assert_eq!(
            operation_name("query Inbox($q: String!) { x }").as_deref(),
            Some("Inbox")
        );
        assert_eq!(
            operation_name("mutation\n  Resolve{ x }").as_deref(),
            Some("Resolve")
        );
        assert_eq!(operation_name("{ viewer { login } }"), None);
    }
}
